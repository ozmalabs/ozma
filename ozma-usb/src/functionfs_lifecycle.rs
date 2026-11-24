// SPDX-License-Identifier: AGPL-3.0-only
//! Safe FunctionFS lifecycle helpers: ConfigFS function creation, mountpoint
//! validation, descriptor writes, readiness waiting, and UDC bind/unbind.
//!
//! FunctionFS needs three cooperating pieces that the rest of this crate models
//! separately:
//!
//! 1. a ConfigFS `functions/ffs.<instance>` directory linked into `configs/c.1`;
//! 2. a FunctionFS mount whose `ep0` accepts the descriptor blob and string
//!    table;
//! 3. a userspace daemon that opens the data endpoints and signals readiness
//!    before the gadget is bound to a UDC.
//!
//! [`FunctionFsLifecycle`] sequences those steps with explicit state and typed
//! failures. It performs no privileged operation by default: mounting and
//! unmounting require an injected [`FunctionFsMounter`]. The default suite uses
//! a temporary ConfigFS root, a temporary mountpoint, and
//! [`FakeFunctionFsMounter`], so it never mounts FunctionFS, binds a real UDC,
//! or requires root. [`CommandFunctionFsMounter`] is the explicit real path and
//! runs argv-only `mount`/`umount` when a caller opts in.
//!
//! The sequence is:
//!
//! ```text
//! prepare()            create gadget + ffs.<instance> + config link
//! mount()              validate the expected mountpoint or call the mounter
//! write_descriptors()  write the v2 blob then the string table to ep0
//! wait_for_ready()     poll the configured daemon marker with a deadline
//! bind()               write UDC, rolling the gadget back on failure
//! unbind() / cleanup() clear UDC, remove the gadget, optionally unmount
//! ```
//!
//! Readiness is never assumed: a marker file is required, and a bind is
//! rejected until [`FunctionFsLifecyclePhase::Ready`]. A real mount still needs
//! root and a real UDC; missing tools, mounts, or permissions remain typed
//! [`UsbError`] values.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::config::{is_safe_identifier, is_safe_udc, UsbFunction, UsbGadgetConfig};
use crate::configfs::ConfigFsUsbController;
use crate::controller::UsbGadgetController;
use crate::error::{io_error, UsbError};
use crate::functionfs::{FunctionFsConfig, FunctionFsStatus};
use crate::functionfs_daemon::FunctionFsMount;

/// Default parent for `/dev/ffs-<instance>` mountpoints.
pub const DEFAULT_FUNCTIONFS_MOUNT_BASE: &str = "/dev";

/// Lifecycle configuration for one FunctionFS vendor function.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionFsLifecycleConfig {
    /// ConfigFS gadget directory name.
    pub gadget: String,
    /// ConfigFS root (normally `/sys/kernel/config/usb_gadget`).
    pub configfs_root: PathBuf,
    /// Parent directory for the default `ffs-<instance>` mountpoint.
    pub mount_base: PathBuf,
    /// Explicit mountpoint, overriding `mount_base`.
    pub mountpoint: Option<PathBuf>,
    /// UDC to bind after readiness.
    pub udc: String,
    /// FunctionFS function descriptor/daemon model.
    pub function: FunctionFsConfig,
    /// Readiness deadline in milliseconds.
    pub readiness_timeout_ms: u64,
    /// Readiness poll interval in milliseconds.
    pub poll_interval_ms: u64,
}

impl FunctionFsLifecycleConfig {
    /// A lifecycle for one gadget/function/UDC with default timings.
    pub fn new(
        gadget: impl Into<String>,
        configfs_root: impl Into<PathBuf>,
        udc: impl Into<String>,
        function: FunctionFsConfig,
    ) -> Self {
        Self {
            gadget: gadget.into(),
            configfs_root: configfs_root.into(),
            mount_base: PathBuf::from(DEFAULT_FUNCTIONFS_MOUNT_BASE),
            mountpoint: None,
            udc: udc.into(),
            function,
            readiness_timeout_ms: 5_000,
            poll_interval_ms: 50,
        }
    }

    /// Use an explicit FunctionFS mountpoint.
    pub fn with_mountpoint(mut self, mountpoint: impl Into<PathBuf>) -> Self {
        self.mountpoint = Some(mountpoint.into());
        self
    }

    /// Override the default mount parent.
    pub fn with_mount_base(mut self, mount_base: impl Into<PathBuf>) -> Self {
        self.mount_base = mount_base.into();
        self
    }

    /// Override readiness timeout and poll interval.
    pub fn with_readiness(mut self, timeout_ms: u64, poll_interval_ms: u64) -> Self {
        self.readiness_timeout_ms = timeout_ms;
        self.poll_interval_ms = poll_interval_ms;
        self
    }

    /// The expected mountpoint (`mountpoint` or `mount_base/ffs-<instance>`).
    pub fn expected_mountpoint(&self) -> PathBuf {
        match &self.mountpoint {
            Some(path) => path.clone(),
            None => self
                .mount_base
                .join(format!("ffs-{}", self.function.instance)),
        }
    }

    /// The ConfigFS function name (`ffs.<instance>`).
    pub fn function_name(&self) -> String {
        self.function.configfs_name()
    }

    /// Validate names, paths, geometry, and timing before any filesystem work.
    pub fn validate(&self) -> Result<(), UsbError> {
        if !is_safe_identifier(&self.gadget) {
            return Err(UsbError::InvalidField {
                field: "functionfs.gadget",
                reason: format!(
                    "{:?} must be a non-empty ASCII name containing only letters, digits, '-' or '_'",
                    self.gadget
                ),
            });
        }
        if !self.configfs_root.is_absolute() {
            return Err(UsbError::InvalidField {
                field: "functionfs.configfs_root",
                reason: "ConfigFS root must be an absolute path".to_owned(),
            });
        }
        if !self.mount_base.is_absolute() {
            return Err(UsbError::InvalidField {
                field: "functionfs.mount_base",
                reason: "mount base must be an absolute path".to_owned(),
            });
        }
        if let Some(mountpoint) = &self.mountpoint {
            if !mountpoint.is_absolute() {
                return Err(UsbError::InvalidField {
                    field: "functionfs.mountpoint",
                    reason: "mountpoint must be an absolute path".to_owned(),
                });
            }
            if mountpoint == &self.configfs_root {
                return Err(UsbError::InvalidField {
                    field: "functionfs.mountpoint",
                    reason: "mountpoint must not be the ConfigFS root".to_owned(),
                });
            }
        }
        if !is_safe_udc(&self.udc) {
            return Err(UsbError::InvalidField {
                field: "functionfs.udc",
                reason: format!("{:?} is not a valid UDC name", self.udc),
            });
        }
        self.function.validate()?;
        if self.readiness_timeout_ms == 0 || self.readiness_timeout_ms > 600_000 {
            return Err(UsbError::InvalidField {
                field: "functionfs.readiness_timeout_ms",
                reason: "readiness timeout must be 1..=600000 ms".to_owned(),
            });
        }
        if self.poll_interval_ms == 0 || self.poll_interval_ms > self.readiness_timeout_ms {
            return Err(UsbError::InvalidField {
                field: "functionfs.poll_interval_ms",
                reason: "poll interval must be 1 ms and not exceed the timeout".to_owned(),
            });
        }
        Ok(())
    }
}

/// Explicit lifecycle state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FunctionFsLifecyclePhase {
    /// No ConfigFS work has been done.
    Idle,
    /// The gadget, function directory, and config link exist.
    Prepared,
    /// A FunctionFS mount with `ep0` is available.
    Mounted,
    /// The descriptor blob and string table were written to `ep0`.
    DescriptorsWritten,
    /// The daemon readiness marker is present.
    Ready,
    /// The gadget is bound to its UDC.
    Bound,
    /// A bind or lifecycle step failed and was rolled back.
    Failed,
    /// The gadget and mount were cleaned up.
    TornDown,
}

impl FunctionFsLifecyclePhase {
    /// Stable lower-case name used in JSON.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Prepared => "prepared",
            Self::Mounted => "mounted",
            Self::DescriptorsWritten => "descriptors_written",
            Self::Ready => "ready",
            Self::Bound => "bound",
            Self::Failed => "failed",
            Self::TornDown => "torn_down",
        }
    }
}

/// Observable lifecycle status. Paths are supplied for local diagnostics; the
/// node can omit them at its API boundary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionFsLifecycleStatus {
    /// Local lifecycle phase.
    pub phase: FunctionFsLifecyclePhase,
    /// Gadget directory name.
    pub gadget: String,
    /// Function instance.
    pub instance: String,
    /// Full ConfigFS function name.
    pub function: String,
    /// `functions/ffs.<instance>` path.
    pub function_path: PathBuf,
    /// Configuration symlink path.
    pub config_link: PathBuf,
    /// Expected/actual FunctionFS mountpoint.
    pub mountpoint: PathBuf,
    /// Whether a UDC is currently written to ConfigFS.
    pub bound: bool,
    /// Whether a mount with `ep0` was observed.
    pub mounted: bool,
    /// Whether the descriptor blob was written by this lifecycle.
    pub ep0_written: bool,
    /// Whether the string table was written by this lifecycle.
    pub strings_written: bool,
    /// Marker/daemon readiness and descriptor summary.
    pub function_fs: FunctionFsStatus,
}

/// Injectable FunctionFS mount implementation.
///
/// Implementations must be explicit; the default lifecycle has no mounter and
/// reports a missing mount instead of silently mounting anything.
pub trait FunctionFsMounter: Send + Sync {
    /// Mount `instance` at `mountpoint`.
    fn mount(&self, instance: &str, mountpoint: &Path) -> Result<(), UsbError>;

    /// Unmount `mountpoint`.
    fn unmount(&self, mountpoint: &Path) -> Result<(), UsbError>;

    /// Stable backend name for diagnostics.
    fn name(&self) -> &'static str {
        "custom"
    }
}

/// Real argv-only `mount -t functionfs` / `umount` backend.
///
/// Using this requires the caller to run with the privileges needed for mount.
/// Missing tools and non-zero exits are typed; no shell or `sudo` is used.
#[derive(Clone, Debug)]
pub struct CommandFunctionFsMounter {
    mount_program: PathBuf,
    umount_program: PathBuf,
}

impl Default for CommandFunctionFsMounter {
    fn default() -> Self {
        Self::new()
    }
}

impl CommandFunctionFsMounter {
    /// Use the system `mount`/`umount`.
    pub fn new() -> Self {
        Self {
            mount_program: PathBuf::from("mount"),
            umount_program: PathBuf::from("umount"),
        }
    }

    /// Override the executable names (primarily for tests).
    pub fn with_programs(
        mount_program: impl Into<PathBuf>,
        umount_program: impl Into<PathBuf>,
    ) -> Self {
        Self {
            mount_program: mount_program.into(),
            umount_program: umount_program.into(),
        }
    }
}

impl FunctionFsMounter for CommandFunctionFsMounter {
    fn mount(&self, instance: &str, mountpoint: &Path) -> Result<(), UsbError> {
        run_mount_command(
            &self.mount_program,
            &[
                "-t".to_owned(),
                "functionfs".to_owned(),
                instance.to_owned(),
                mountpoint.to_string_lossy().into_owned(),
            ],
        )
    }

    fn unmount(&self, mountpoint: &Path) -> Result<(), UsbError> {
        run_mount_command(
            &self.umount_program,
            &[mountpoint.to_string_lossy().into_owned()],
        )
    }

    fn name(&self) -> &'static str {
        "command"
    }
}

fn run_mount_command(program: &Path, args: &[String]) -> Result<(), UsbError> {
    let rendered = format!("{} {}", program.display(), args.join(" "));
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|source| {
            if source.kind() == io::ErrorKind::NotFound {
                UsbError::MissingCommand {
                    command: program.display().to_string(),
                    remediation: "install mount/umount and run with the privileges required for \
                              FunctionFS"
                        .to_owned(),
                }
            } else {
                UsbError::Spawn {
                    command: rendered.clone(),
                    source,
                }
            }
        })?;
    if output.status.success() {
        Ok(())
    } else {
        Err(UsbError::CommandFailed {
            command: rendered,
            status: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    }
}

#[derive(Debug, Default)]
struct FakeMounterState {
    mounts: AtomicUsize,
    unmounts: AtomicUsize,
    fail_mount: AtomicBool,
    fail_unmount: AtomicBool,
}

/// In-test mounter that creates a temporary mountpoint, `ep0`, and optional
/// `ep<N>` files. It never mounts anything and never needs root.
#[derive(Clone, Debug)]
pub struct FakeFunctionFsMounter {
    state: Arc<FakeMounterState>,
    endpoints: Vec<u8>,
}

impl Default for FakeFunctionFsMounter {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeFunctionFsMounter {
    /// A fake mounter with no data endpoints (only `ep0`).
    pub fn new() -> Self {
        Self {
            state: Arc::new(FakeMounterState::default()),
            endpoints: Vec::new(),
        }
    }

    /// Create `ep<N>` files as well as `ep0` on mount.
    pub fn with_endpoints(mut self, endpoints: impl IntoIterator<Item = u8>) -> Self {
        self.endpoints = endpoints.into_iter().collect();
        self
    }

    /// Make the next/all mount calls fail.
    pub fn set_fail_mount(&self, fail: bool) {
        self.state.fail_mount.store(fail, Ordering::SeqCst);
    }

    /// Make the next/all unmount calls fail.
    pub fn set_fail_unmount(&self, fail: bool) {
        self.state.fail_unmount.store(fail, Ordering::SeqCst);
    }

    /// Number of successful mount calls.
    pub fn mounts(&self) -> usize {
        self.state.mounts.load(Ordering::SeqCst)
    }

    /// Number of successful unmount calls.
    pub fn unmounts(&self) -> usize {
        self.state.unmounts.load(Ordering::SeqCst)
    }
}

impl FunctionFsMounter for FakeFunctionFsMounter {
    fn mount(&self, _instance: &str, mountpoint: &Path) -> Result<(), UsbError> {
        if self.state.fail_mount.load(Ordering::SeqCst) {
            return Err(UsbError::FunctionFsLifecycle {
                operation: "mount",
                path: mountpoint.to_path_buf(),
                reason: "fake mounter was configured to fail".to_owned(),
            });
        }
        fs::create_dir_all(mountpoint).map_err(|source| {
            io_error(
                "create fake FunctionFS mount",
                mountpoint.to_path_buf(),
                source,
            )
        })?;
        fs::write(mountpoint.join("ep0"), b"").map_err(|source| {
            io_error("create fake FunctionFS ep0", mountpoint.join("ep0"), source)
        })?;
        for index in &self.endpoints {
            let path = mountpoint.join(format!("ep{index}"));
            fs::write(&path, b"")
                .map_err(|source| io_error("create fake FunctionFS endpoint", path, source))?;
        }
        self.state.mounts.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn unmount(&self, mountpoint: &Path) -> Result<(), UsbError> {
        if self.state.fail_unmount.load(Ordering::SeqCst) {
            return Err(UsbError::FunctionFsLifecycle {
                operation: "unmount",
                path: mountpoint.to_path_buf(),
                reason: "fake mounter was configured to fail".to_owned(),
            });
        }
        match fs::remove_dir_all(mountpoint) {
            Ok(()) => {}
            Err(source) if source.kind() == io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(io_error(
                    "remove fake FunctionFS mount",
                    mountpoint.to_path_buf(),
                    source,
                ))
            }
        }
        self.state.unmounts.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn name(&self) -> &'static str {
        "fake"
    }
}

/// Clock/sleep seam for readiness polling. Tests use a manual clock so a
/// timeout is deterministic and does not sleep in real time.
pub trait FunctionFsLifecycleClock: Send + Sync {
    /// Milliseconds since an arbitrary monotonic epoch.
    fn now_millis(&self) -> u64;

    /// Sleep for at least `millis`.
    fn sleep(&self, millis: u64);
}

/// Wall-clock implementation.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemFunctionFsLifecycleClock;

impl FunctionFsLifecycleClock for SystemFunctionFsLifecycleClock {
    fn now_millis(&self) -> u64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(0)
    }

    fn sleep(&self, millis: u64) {
        std::thread::sleep(std::time::Duration::from_millis(millis));
    }
}

/// Deterministic clock where `sleep` advances time immediately.
#[derive(Debug, Default)]
pub struct ManualFunctionFsLifecycleClock {
    millis: AtomicU64,
    sleeps: AtomicU64,
}

impl ManualFunctionFsLifecycleClock {
    /// Start at `millis`.
    pub fn new(millis: u64) -> Self {
        Self {
            millis: AtomicU64::new(millis),
            sleeps: AtomicU64::new(0),
        }
    }

    /// Advance the clock without recording a sleep.
    pub fn advance(&self, millis: u64) {
        self.millis.fetch_add(millis, Ordering::SeqCst);
    }

    /// Number of `sleep` calls.
    pub fn sleep_calls(&self) -> u64 {
        self.sleeps.load(Ordering::SeqCst)
    }
}

impl FunctionFsLifecycleClock for ManualFunctionFsLifecycleClock {
    fn now_millis(&self) -> u64 {
        self.millis.load(Ordering::SeqCst)
    }

    fn sleep(&self, millis: u64) {
        self.millis.fetch_add(millis, Ordering::SeqCst);
        self.sleeps.fetch_add(1, Ordering::SeqCst);
    }
}

/// A FunctionFS lifecycle driver.
pub struct FunctionFsLifecycle {
    config: FunctionFsLifecycleConfig,
    controller: ConfigFsUsbController,
    mounter: Option<Box<dyn FunctionFsMounter>>,
    clock: Arc<dyn FunctionFsLifecycleClock>,
    phase: FunctionFsLifecyclePhase,
    mounted: bool,
    mounted_by_us: bool,
    ep0_written: bool,
    strings_written: bool,
}

impl std::fmt::Debug for FunctionFsLifecycle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FunctionFsLifecycle")
            .field("config", &self.config)
            .field("phase", &self.phase)
            .field(
                "mounter",
                &self.mounter.as_ref().map(|mounter| mounter.name()),
            )
            .field("mounted", &self.mounted)
            .field("ep0_written", &self.ep0_written)
            .field("strings_written", &self.strings_written)
            .finish_non_exhaustive()
    }
}

impl FunctionFsLifecycle {
    /// Validate and construct a lifecycle with no mounter and a system clock.
    pub fn new(config: FunctionFsLifecycleConfig) -> Result<Self, UsbError> {
        config.validate()?;
        let controller = ConfigFsUsbController::new(&config.configfs_root);
        Ok(Self {
            config,
            controller,
            mounter: None,
            clock: Arc::new(SystemFunctionFsLifecycleClock),
            phase: FunctionFsLifecyclePhase::Idle,
            mounted: false,
            mounted_by_us: false,
            ep0_written: false,
            strings_written: false,
        })
    }

    /// Inject an explicit mounter.
    pub fn with_mounter<M: FunctionFsMounter + 'static>(mut self, mounter: M) -> Self {
        self.mounter = Some(Box::new(mounter));
        self
    }

    /// Inject a clock/sleeper.
    pub fn with_clock<C: FunctionFsLifecycleClock + 'static>(mut self, clock: C) -> Self {
        self.clock = Arc::new(clock);
        self
    }

    /// Lifecycle configuration.
    pub fn config(&self) -> &FunctionFsLifecycleConfig {
        &self.config
    }

    /// Current local phase.
    pub fn phase(&self) -> FunctionFsLifecyclePhase {
        self.phase
    }

    /// Expected/actual mountpoint.
    pub fn mountpoint(&self) -> PathBuf {
        self.config.expected_mountpoint()
    }

    /// Create the gadget, `functions/ffs.<instance>` directory, and config link.
    ///
    /// This is ordinary ConfigFS directory/attribute/symlink work against the
    /// injected root. It leaves the gadget unbounded.
    pub fn prepare(&mut self) -> Result<FunctionFsLifecycleStatus, UsbError> {
        if !matches!(
            self.phase,
            FunctionFsLifecyclePhase::Idle
                | FunctionFsLifecyclePhase::TornDown
                | FunctionFsLifecyclePhase::Failed
        ) {
            return Err(invalid_state(format!(
                "prepare cannot run in phase {}",
                self.phase.as_str()
            )));
        }
        self.config.validate()?;
        let gadget = self.gadget_config();
        gadget.validate()?;
        self.controller.create_gadget(&gadget)?;

        let function_name = self.config.function_name();
        let gadget_dir = self.controller.gadget_path(&self.config.gadget);
        let function_path = gadget_dir.join("functions").join(&function_name);
        if !function_path.is_dir() {
            let _ = self.controller.remove_gadget(&self.config.gadget);
            return Err(UsbError::MissingFunction {
                gadget: self.config.gadget.clone(),
                function: function_name,
            });
        }
        let config_link = gadget_dir.join("configs").join("c.1").join(&function_name);
        let linked = fs::symlink_metadata(&config_link)
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(false);
        if !linked {
            let _ = self.controller.remove_gadget(&self.config.gadget);
            return Err(invalid_state(format!(
                "ConfigFS function link {} was not created",
                config_link.display()
            )));
        }

        self.phase = FunctionFsLifecyclePhase::Prepared;
        Ok(self.status())
    }

    /// Validate the expected mountpoint or call the injected mounter.
    pub fn mount(&mut self) -> Result<FunctionFsLifecycleStatus, UsbError> {
        if matches!(
            self.phase,
            FunctionFsLifecyclePhase::Idle
                | FunctionFsLifecyclePhase::TornDown
                | FunctionFsLifecyclePhase::Failed
        ) {
            return Err(invalid_state(format!(
                "mount cannot run in phase {}",
                self.phase.as_str()
            )));
        }
        if self.mounted {
            return Ok(self.status());
        }

        let mountpoint = self.mountpoint();
        let mount = FunctionFsMount::new(&mountpoint);
        match mount.validate() {
            Ok(()) => {
                self.mounted = true;
            }
            Err(error)
                if matches!(
                    &error,
                    UsbError::MissingFunctionFsMount(_) | UsbError::MissingFunctionFsEp0(_)
                ) =>
            {
                let Some(mounter) = &self.mounter else {
                    return Err(error);
                };
                mounter.mount(&self.config.function.instance, &mountpoint)?;
                if let Err(error) = mount.validate() {
                    let _ = mounter.unmount(&mountpoint);
                    return Err(error);
                }
                self.mounted = true;
                self.mounted_by_us = true;
            }
            Err(error) => return Err(error),
        }

        if matches!(
            self.phase,
            FunctionFsLifecyclePhase::Prepared | FunctionFsLifecyclePhase::Mounted
        ) {
            self.phase = FunctionFsLifecyclePhase::Mounted;
        }
        Ok(self.status())
    }

    /// Build and write the FunctionFS descriptor blob then string table to
    /// `ep0`.
    pub fn write_descriptors(&mut self) -> Result<FunctionFsLifecycleStatus, UsbError> {
        if !self.mounted {
            return Err(invalid_state(
                "the FunctionFS mountpoint must be mounted before descriptor writes",
            ));
        }
        if self.ep0_written {
            return Err(invalid_state(
                "FunctionFS descriptors were already written by this lifecycle",
            ));
        }
        let descriptors = self.config.function.ep0_descriptors()?;
        FunctionFsMount::new(self.mountpoint()).write_descriptors(&descriptors)?;
        self.ep0_written = true;
        self.strings_written = true;
        if matches!(
            self.phase,
            FunctionFsLifecyclePhase::Prepared | FunctionFsLifecyclePhase::Mounted
        ) {
            self.phase = FunctionFsLifecyclePhase::DescriptorsWritten;
        }
        Ok(self.status())
    }

    /// Wait for the configured daemon marker using the lifecycle timeout.
    pub fn wait_for_ready(&mut self) -> Result<FunctionFsLifecycleStatus, UsbError> {
        self.wait_for_ready_timeout(self.config.readiness_timeout_ms)
    }

    /// Wait for the configured daemon marker with an explicit deadline.
    ///
    /// Polling is deterministic through [`FunctionFsLifecycleClock`]. A missing
    /// marker, missing daemon program, or expired deadline is never treated as
    /// success: the latter returns
    /// [`UsbError::FunctionFsReadinessTimeout`].
    pub fn wait_for_ready_timeout(
        &mut self,
        timeout_ms: u64,
    ) -> Result<FunctionFsLifecycleStatus, UsbError> {
        if !self.ep0_written {
            return Err(invalid_state(
                "descriptors must be written before waiting for daemon readiness",
            ));
        }
        if timeout_ms == 0 {
            return Err(UsbError::InvalidField {
                field: "functionfs.readiness_timeout_ms",
                reason: "readiness timeout must be non-zero".to_owned(),
            });
        }
        if self.config.function.readiness().is_ready() {
            self.phase = FunctionFsLifecyclePhase::Ready;
            return Ok(self.status());
        }

        let poll = self.config.poll_interval_ms.max(1);
        let start = self.clock.now_millis();
        loop {
            let elapsed = self.clock.now_millis().saturating_sub(start);
            if elapsed >= timeout_ms {
                return Err(UsbError::FunctionFsReadinessTimeout {
                    instance: self.config.function.instance.clone(),
                    timeout_ms,
                    readiness: self.config.function.readiness().as_str(),
                });
            }
            let remaining = timeout_ms - elapsed;
            self.clock.sleep(poll.min(remaining));
            if self.config.function.readiness().is_ready() {
                self.phase = FunctionFsLifecyclePhase::Ready;
                return Ok(self.status());
            }
        }
    }

    /// Bind the gadget to its UDC after readiness.
    ///
    /// A bind failure rolls the lifecycle back: the gadget is removed, a mount
    /// created by this lifecycle is unmounted, and the phase becomes
    /// [`FunctionFsLifecyclePhase::Failed`] while the original typed error is
    /// returned.
    pub fn bind(&mut self) -> Result<FunctionFsLifecycleStatus, UsbError> {
        if self.phase != FunctionFsLifecyclePhase::Ready {
            return Err(invalid_state(format!(
                "bind requires readiness; current phase is {}",
                self.phase.as_str()
            )));
        }
        match self.controller.bind(&self.config.gadget, &self.config.udc) {
            Ok(_) => {
                self.phase = FunctionFsLifecyclePhase::Bound;
                Ok(self.status())
            }
            Err(error) => {
                self.rollback();
                self.phase = FunctionFsLifecyclePhase::Failed;
                Err(error)
            }
        }
    }

    /// Clear the UDC binding while keeping the function and mount in place.
    pub fn unbind(&mut self) -> Result<FunctionFsLifecycleStatus, UsbError> {
        self.controller.unbind(&self.config.gadget)?;
        if self.phase == FunctionFsLifecyclePhase::Bound {
            self.phase = FunctionFsLifecyclePhase::Ready;
        }
        Ok(self.status())
    }

    /// Remove the gadget, config link, function directory, and a mount created
    /// by this lifecycle. Idempotent.
    pub fn cleanup(&mut self) -> Result<(), UsbError> {
        let mut first_error: Option<UsbError> = None;
        if !matches!(
            self.phase,
            FunctionFsLifecyclePhase::Idle | FunctionFsLifecyclePhase::TornDown
        ) {
            match self.controller.remove_gadget(&self.config.gadget) {
                Ok(()) => {}
                Err(UsbError::MissingGadget(_)) => {}
                Err(error) => first_error = Some(error),
            }
        }
        if self.mounted_by_us {
            if let Some(mounter) = &self.mounter {
                if let Err(error) = mounter.unmount(&self.mountpoint()) {
                    first_error.get_or_insert(error);
                }
            }
        }
        self.mounted = false;
        self.mounted_by_us = false;
        self.ep0_written = false;
        self.strings_written = false;
        self.phase = FunctionFsLifecyclePhase::TornDown;
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Observable status, including current ConfigFS UDC binding.
    pub fn status(&self) -> FunctionFsLifecycleStatus {
        let gadget_dir = self.controller.gadget_path(&self.config.gadget);
        let function_name = self.config.function_name();
        let bound = self
            .controller
            .status(&self.config.gadget)
            .map(|status| status.bound)
            .unwrap_or(false);
        FunctionFsLifecycleStatus {
            phase: self.phase,
            gadget: self.config.gadget.clone(),
            instance: self.config.function.instance.clone(),
            function: function_name.clone(),
            function_path: gadget_dir.join("functions").join(&function_name),
            config_link: gadget_dir.join("configs").join("c.1").join(&function_name),
            mountpoint: self.mountpoint(),
            bound,
            mounted: self.mounted,
            ep0_written: self.ep0_written,
            strings_written: self.strings_written,
            function_fs: self.config.function.status(),
        }
    }

    fn rollback(&mut self) {
        let _ = self.controller.unbind(&self.config.gadget);
        let _ = self.controller.remove_gadget(&self.config.gadget);
        if self.mounted_by_us {
            if let Some(mounter) = &self.mounter {
                let _ = mounter.unmount(&self.mountpoint());
            }
        }
        self.mounted = false;
        self.mounted_by_us = false;
        self.ep0_written = false;
        self.strings_written = false;
    }

    fn gadget_config(&self) -> UsbGadgetConfig {
        let function = self.config.function.clone();
        let mut gadget = UsbGadgetConfig::new(self.config.gadget.clone(), self.config.udc.clone())
            .with_identity(function.vendor_id, function.product_id)
            .with_strings(
                function.strings.manufacturer.clone(),
                function.strings.product.clone(),
                function.strings.serial_number.clone(),
            )
            .with_function(UsbFunction::vendor_with_config(function.clone()));
        gadget.configuration = function.strings.configuration.clone();
        gadget
    }
}

fn invalid_state(reason: impl Into<String>) -> UsbError {
    UsbError::InvalidFunctionFsState {
        reason: reason.into(),
    }
}
