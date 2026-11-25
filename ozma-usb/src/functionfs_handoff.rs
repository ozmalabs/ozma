// SPDX-License-Identifier: AGPL-3.0-only
//! Ordered FunctionFS handoff: prepare, mount, daemon, bind, single unbind.
//!
//! FunctionFS needs the function to exist before it can be mounted and the
//! mount (plus a userspace daemon that writes the descriptors) before the UDC
//! may be bound. A node that creates and binds the whole gadget in one step can
//! never satisfy that order. [`FunctionFsHandoff`] keeps the order explicit:
//!
//! ```text
//! prepare(gadget)   create the composite gadget and ffs function, unbound
//! mount()           validate a mount or mount FunctionFS through a mounter
//! daemon_started()  the caller's daemon wrote ep0 descriptors/strings
//! bind()            write the UDC, rolling the gadget/mount back on failure
//! unbind_once()     clear the UDC exactly once
//! cleanup()         unmount, then remove the function/gadget
//! ```
//!
//! Every failure is typed and rolls back the steps this handoff performed: a
//! failed mount removes the freshly created gadget, a failed bind unbinds,
//! removes the gadget, and unmounts a mount it created. The handoff never starts
//! a daemon and never opens a data endpoint; the node owns the media daemon and
//! calls [`FunctionFsHandoff::daemon_started`] after the daemon has written its
//! `ep0` descriptors.
//!
//! Tests use [`FakeUsbGadgetController`](crate::FakeUsbGadgetController) plus
//! [`FakeFunctionFsMounter`](crate::FakeFunctionFsMounter), so the default suite
//! never mounts FunctionFS, writes a UDC, or requires root.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::config::{is_safe_identifier, is_safe_udc, UsbFunctionKind, UsbGadgetConfig};
use crate::controller::UsbGadgetController;
use crate::error::{io_error, UsbError};
use crate::functionfs_daemon::FunctionFsMount;
use crate::functionfs_lifecycle::FunctionFsMounter;

/// Explicit phases of the mount-before-bind handoff.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FunctionFsHandoffPhase {
    /// No gadget or mount work has been done.
    Idle,
    /// The gadget, function, and config link exist; the UDC is unbound.
    Prepared,
    /// FunctionFS is mounted and exposes `ep0`.
    Mounted,
    /// The caller's daemon wrote the descriptor and string blobs.
    DaemonStarted,
    /// The UDC attribute is written.
    Bound,
    /// The UDC attribute was cleared once.
    Unbound,
    /// A step failed and its partial work was rolled back.
    Failed,
    /// The mount and function/gadget were removed.
    TornDown,
}

impl FunctionFsHandoffPhase {
    /// Stable lower-case name used by logs and status JSON.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Prepared => "prepared",
            Self::Mounted => "mounted",
            Self::DaemonStarted => "daemon_started",
            Self::Bound => "bound",
            Self::Unbound => "unbound",
            Self::Failed => "failed",
            Self::TornDown => "torn_down",
        }
    }
}

/// Observable handoff state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionFsHandoffStatus {
    /// Current phase.
    pub phase: FunctionFsHandoffPhase,
    /// ConfigFS gadget directory name.
    pub gadget: String,
    /// FunctionFS instance used as the mount source (`ffs.<instance>`).
    pub instance: String,
    /// UDC written on bind.
    pub udc: String,
    /// Expected/actual FunctionFS mountpoint.
    pub mountpoint: PathBuf,
    /// Whether a UDC is currently written.
    pub bound: bool,
    /// Whether a FunctionFS mount is present.
    pub mounted: bool,
    /// Whether this handoff created the mount (and therefore owns unmounting).
    pub mounted_by_us: bool,
    /// Successful UDC binds performed by this handoff.
    pub binds: u64,
    /// UDC unbind calls performed by this handoff.
    pub unbinds: u64,
}

/// A FunctionFS handoff bound to an injected controller and mounter.
pub struct FunctionFsHandoff {
    gadget: String,
    instance: String,
    udc: String,
    mountpoint: PathBuf,
    controller: Arc<dyn UsbGadgetController>,
    mounter: Option<Box<dyn FunctionFsMounter>>,
    phase: FunctionFsHandoffPhase,
    mounted: bool,
    mounted_by_us: bool,
    binds: u64,
    unbinds: u64,
}

impl std::fmt::Debug for FunctionFsHandoff {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FunctionFsHandoff")
            .field("gadget", &self.gadget)
            .field("instance", &self.instance)
            .field("udc", &self.udc)
            .field("mountpoint", &self.mountpoint)
            .field("phase", &self.phase)
            .field("mounted", &self.mounted)
            .field("mounted_by_us", &self.mounted_by_us)
            .field(
                "mounter",
                &self.mounter.as_ref().map(|mounter| mounter.name()),
            )
            .field("binds", &self.binds)
            .field("unbinds", &self.unbinds)
            .finish_non_exhaustive()
    }
}

impl FunctionFsHandoff {
    /// Build a handoff for one gadget, FunctionFS instance, UDC, and mountpoint.
    ///
    /// No filesystem work is performed here. Mounting is only possible after
    /// [`Self::prepare`] created the `ffs.<instance>` function.
    pub fn new(
        controller: Arc<dyn UsbGadgetController>,
        gadget: impl Into<String>,
        instance: impl Into<String>,
        udc: impl Into<String>,
        mountpoint: impl Into<PathBuf>,
    ) -> Result<Self, UsbError> {
        let gadget = gadget.into();
        let instance = instance.into();
        let udc = udc.into();
        let mountpoint = mountpoint.into();
        if !is_safe_identifier(&gadget) {
            return Err(UsbError::InvalidField {
                field: "functionfs.gadget",
                reason: format!("{gadget:?} is not a safe gadget name"),
            });
        }
        if !is_safe_identifier(&instance) {
            return Err(UsbError::InvalidField {
                field: "functionfs.instance",
                reason: format!("{instance:?} is not a safe FunctionFS instance"),
            });
        }
        if !is_safe_udc(&udc) {
            return Err(UsbError::InvalidField {
                field: "functionfs.udc",
                reason: format!("{udc:?} is not a valid UDC name"),
            });
        }
        if !mountpoint.is_absolute() {
            return Err(UsbError::InvalidField {
                field: "functionfs.mountpoint",
                reason: "FunctionFS mountpoint must be an absolute path".to_owned(),
            });
        }
        Ok(Self {
            gadget,
            instance,
            udc,
            mountpoint,
            controller,
            mounter: None,
            phase: FunctionFsHandoffPhase::Idle,
            mounted: false,
            mounted_by_us: false,
            binds: 0,
            unbinds: 0,
        })
    }

    /// Inject a mounter. Without one, only an already mounted FunctionFS is
    /// accepted; a missing mount is a typed `missing-functionfs-mount`.
    pub fn with_mounter<M: FunctionFsMounter + 'static>(mut self, mounter: M) -> Self {
        self.mounter = Some(Box::new(mounter));
        self
    }

    /// Current phase.
    pub fn phase(&self) -> FunctionFsHandoffPhase {
        self.phase
    }

    /// FunctionFS instance (`vendor0`).
    pub fn instance(&self) -> &str {
        &self.instance
    }

    /// Expected/actual mountpoint.
    pub fn mountpoint(&self) -> &Path {
        &self.mountpoint
    }

    /// Number of successful binds performed by this handoff.
    pub fn binds(&self) -> u64 {
        self.binds
    }

    /// Number of UDC unbind calls performed by this handoff.
    pub fn unbinds(&self) -> u64 {
        self.unbinds
    }

    /// Snapshot, including the controller's current UDC state when readable.
    pub fn status(&self) -> FunctionFsHandoffStatus {
        let bound = self.currently_bound();
        FunctionFsHandoffStatus {
            phase: self.phase,
            gadget: self.gadget.clone(),
            instance: self.instance.clone(),
            udc: self.udc.clone(),
            mountpoint: self.mountpoint.clone(),
            bound,
            mounted: self.mounted,
            mounted_by_us: self.mounted_by_us,
            binds: self.binds,
            unbinds: self.unbinds,
        }
    }

    /// Create the composite gadget and the `ffs.<instance>` function, leaving
    /// the UDC unbound. A failure removes any partially created gadget.
    pub fn prepare(
        &mut self,
        config: &UsbGadgetConfig,
    ) -> Result<FunctionFsHandoffStatus, UsbError> {
        if !matches!(
            self.phase,
            FunctionFsHandoffPhase::Idle
                | FunctionFsHandoffPhase::TornDown
                | FunctionFsHandoffPhase::Failed
        ) {
            return Err(self.invalid_phase("prepare"));
        }
        config.validate()?;
        if config.name != self.gadget {
            return Err(UsbError::InvalidField {
                field: "functionfs.gadget",
                reason: format!(
                    "handoff gadget {:?} does not match gadget config {:?}",
                    self.gadget, config.name
                ),
            });
        }
        if config.udc != self.udc {
            return Err(UsbError::InvalidField {
                field: "functionfs.udc",
                reason: format!(
                    "handoff UDC {:?} does not match gadget config {:?}",
                    self.udc, config.udc
                ),
            });
        }
        let expected = format!("ffs.{}", self.instance);
        if !config.functions.iter().any(|function| {
            function.kind() == UsbFunctionKind::Vendor && function.configfs_name() == expected
        }) {
            return Err(UsbError::FunctionFsLifecycle {
                operation: "prepare",
                path: self.mountpoint.clone(),
                reason: format!("gadget config has no {expected} FunctionFS function"),
            });
        }

        if let Err(error) = self.controller.create_gadget(config) {
            self.phase = FunctionFsHandoffPhase::Failed;
            return Err(error);
        }
        let linked = self
            .controller
            .status(&self.gadget)
            .map(|status| {
                status.functions.iter().any(|function| {
                    function.kind == UsbFunctionKind::Vendor
                        && function.name == expected
                        && function.linked
                })
            })
            .unwrap_or(false);
        if !linked {
            let error = UsbError::FunctionFsLifecycle {
                operation: "prepare",
                path: self.mountpoint.clone(),
                reason: format!("ConfigFS function {expected} was not created or linked"),
            };
            self.rollback();
            self.phase = FunctionFsHandoffPhase::Failed;
            return Err(error);
        }
        self.phase = FunctionFsHandoffPhase::Prepared;
        Ok(self.status())
    }

    /// Validate the configured mountpoint or mount FunctionFS with the injected
    /// mounter. A missing mounter is a typed error; a failed mount rolls the
    /// freshly created gadget back.
    pub fn mount(&mut self) -> Result<FunctionFsHandoffStatus, UsbError> {
        if !matches!(
            self.phase,
            FunctionFsHandoffPhase::Prepared | FunctionFsHandoffPhase::Mounted
        ) {
            return Err(self.invalid_phase("mount"));
        }
        if self.mounted {
            return Ok(self.status());
        }

        let mount = FunctionFsMount::new(&self.mountpoint);
        match mount.validate() {
            Ok(()) => {
                self.mounted = true;
                self.mounted_by_us = false;
            }
            Err(
                error @ (UsbError::MissingFunctionFsMount(_) | UsbError::MissingFunctionFsEp0(_)),
            ) => {
                let Some(mounter) = &self.mounter else {
                    self.rollback();
                    self.phase = FunctionFsHandoffPhase::Failed;
                    return Err(error);
                };
                if let Err(source) = fs::create_dir_all(&self.mountpoint) {
                    let error = io_error(
                        "create FunctionFS mountpoint",
                        self.mountpoint.clone(),
                        source,
                    );
                    self.rollback();
                    self.phase = FunctionFsHandoffPhase::Failed;
                    return Err(error);
                }
                if let Err(error) = mounter.mount(&self.instance, &self.mountpoint) {
                    self.rollback();
                    self.phase = FunctionFsHandoffPhase::Failed;
                    return Err(error);
                }
                if let Err(error) = mount.validate() {
                    let _ = mounter.unmount(&self.mountpoint);
                    self.rollback();
                    self.phase = FunctionFsHandoffPhase::Failed;
                    return Err(error);
                }
                self.mounted = true;
                self.mounted_by_us = true;
            }
            Err(error) => {
                self.rollback();
                self.phase = FunctionFsHandoffPhase::Failed;
                return Err(error);
            }
        }

        self.phase = FunctionFsHandoffPhase::Mounted;
        Ok(self.status())
    }

    /// Record that the caller's daemon wrote the descriptor and string blobs to
    /// `ep0`. Bind is rejected until this step.
    pub fn daemon_started(&mut self) -> Result<FunctionFsHandoffStatus, UsbError> {
        if self.phase != FunctionFsHandoffPhase::Mounted {
            return Err(self.invalid_phase("daemon_started"));
        }
        self.phase = FunctionFsHandoffPhase::DaemonStarted;
        Ok(self.status())
    }

    /// Write the UDC after the daemon is ready. A bind failure unbinds (when
    /// needed), removes the gadget, and unmounts a mount this handoff created.
    pub fn bind(&mut self) -> Result<FunctionFsHandoffStatus, UsbError> {
        if self.phase != FunctionFsHandoffPhase::DaemonStarted {
            return Err(self.invalid_phase("bind"));
        }
        match self.controller.bind(&self.gadget, &self.udc) {
            Ok(_) => {
                self.binds = self.binds.saturating_add(1);
                self.phase = FunctionFsHandoffPhase::Bound;
                Ok(self.status())
            }
            Err(error) => {
                self.rollback();
                self.phase = FunctionFsHandoffPhase::Failed;
                Err(error)
            }
        }
    }

    /// Clear the UDC exactly once. Calling this again after a successful unbind
    /// is a no-op, so shutdown never needs an extra external unbind.
    pub fn unbind_once(&mut self) -> Result<FunctionFsHandoffStatus, UsbError> {
        if !self.currently_bound() {
            if self.phase == FunctionFsHandoffPhase::Bound {
                self.phase = FunctionFsHandoffPhase::Unbound;
            }
            return Ok(self.status());
        }
        match self.controller.unbind(&self.gadget) {
            Ok(_) => {
                self.unbinds = self.unbinds.saturating_add(1);
                self.phase = FunctionFsHandoffPhase::Unbound;
                Ok(self.status())
            }
            Err(error) => {
                self.phase = FunctionFsHandoffPhase::Failed;
                Err(error)
            }
        }
    }

    /// Unmount (when this handoff mounted FunctionFS) and remove the function
    /// and gadget. If the UDC is still bound this performs the one unbind first.
    /// Idempotent.
    pub fn cleanup(&mut self) -> Result<(), UsbError> {
        if matches!(
            self.phase,
            FunctionFsHandoffPhase::Idle | FunctionFsHandoffPhase::TornDown
        ) {
            return Ok(());
        }
        let mut first_error = None;
        if self.currently_bound() {
            if let Err(error) = self.unbind_once() {
                first_error = Some(error);
            }
        }

        // FunctionFS must be unmounted before its ConfigFS function can be
        // removed; the kernel rejects rmdir of a mounted ffs function.
        if self.mounted_by_us {
            if let Some(mounter) = &self.mounter {
                if let Err(error) = mounter.unmount(&self.mountpoint) {
                    first_error.get_or_insert(error);
                }
            }
        }
        match self.controller.remove_gadget(&self.gadget) {
            Ok(()) | Err(UsbError::MissingGadget(_)) => {}
            Err(error) => {
                first_error.get_or_insert(error);
            }
        }
        self.mounted = false;
        self.mounted_by_us = false;
        self.phase = FunctionFsHandoffPhase::TornDown;
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Best-effort rollback after a later step failed: unbind once, unmount an
    /// owned mount, remove the gadget, and mark the phase `failed`.
    pub fn rollback(&mut self) {
        let _ = self.cleanup();
        self.phase = FunctionFsHandoffPhase::Failed;
    }

    fn currently_bound(&self) -> bool {
        self.controller
            .status(&self.gadget)
            .map(|status| status.bound)
            .unwrap_or(false)
    }

    fn invalid_phase(&self, operation: &'static str) -> UsbError {
        UsbError::FunctionFsLifecycle {
            operation,
            path: self.mountpoint.clone(),
            reason: format!("{operation} cannot run in phase {}", self.phase.as_str()),
        }
    }
}
