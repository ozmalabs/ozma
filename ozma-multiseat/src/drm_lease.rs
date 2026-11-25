// SPDX-License-Identifier: AGPL-3.0-only
//! Typed Linux DRM leasing.
//!
//! A DRM lease is a kernel capability handoff: the current DRM master names a
//! set of KMS object IDs (connectors, CRTCs, and optionally planes) and the
//! kernel returns a separate file descriptor that grants only those objects to
//! the lessee. Closing the lessee fd (or revoking the lease from the lessor)
//! releases them.
//!
//! This module is deliberately **lease-only**. Acquiring a lease does not create
//! a compositor, program scanout, move an X output, or isolate the host's
//! desktop. A seat that requests a lease receives an owned lessee fd and a typed
//! status note saying exactly that. The default multi-seat display backend is
//! still `shared`. For an opt-in scanout proof layered on the lease, see
//! [`crate::drm_scanout`]; it proves one static framebuffer, not a compositor.
//!
//! Card selection is conservative. [`DrmCardSelector::AutoVirtual`] only picks
//! a card whose sysfs driver is `vkms`; a real GPU/card is leased only when the
//! operator names its `/dev/dri/cardN` path (or `cardN`). Object IDs are always
//! validated against the card's current resources before the ioctl.
//!
//! The actual kernel ABI is provided by the maintained `drm` crate, so this
//! module contains no `unsafe` code and no hand-rolled ioctl structures.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::os::fd::{BorrowedFd, RawFd};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

use crate::drm::{DrmStatus, DEFAULT_DRM_ROOT};
use crate::error::MultiseatError;

/// Default directory containing primary DRM card nodes.
pub const DEFAULT_DRM_DEV_ROOT: &str = "/dev/dri";
/// Environment variable carrying the inherited lessee fd number.
pub const DRM_LEASE_FD_ENV: &str = "OZMA_DRM_FD";
/// Environment variable carrying the kernel lessee id.
pub const DRM_LEASE_ID_ENV: &str = "OZMA_DRM_LEASE_ID";
/// Environment variable carrying the leased card path.
pub const DRM_LEASE_CARD_ENV: &str = "OZMA_DRM_CARD";
/// Environment variable carrying the leased connector name.
pub const DRM_LEASE_CONNECTOR_ENV: &str = "OZMA_DRM_CONNECTOR";
/// Environment variable carrying the leased connector object id.
pub const DRM_LEASE_CONNECTOR_ID_ENV: &str = "OZMA_DRM_CONNECTOR_ID";
/// Environment variable carrying the leased CRTC object id.
pub const DRM_LEASE_CRTC_ID_ENV: &str = "OZMA_DRM_CRTC_ID";
/// Environment variable carrying an explicitly requested plane object id.
pub const DRM_LEASE_PLANE_ID_ENV: &str = "OZMA_DRM_PLANE_ID";
/// Presence marker (`1`) for an exported lease.
pub const DRM_LEASE_ENV: &str = "OZMA_DRM_LEASE";
/// Driver name treated as a safe virtual card for automatic selection.
pub const VIRTUAL_DRM_DRIVER: &str = "vkms";

/// A DRM object type that can be leased.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DrmObjectKind {
    /// Connector (`DRM_MODE_OBJECT_CONNECTOR`).
    Connector,
    /// CRTC (`DRM_MODE_OBJECT_CRTC`).
    Crtc,
    /// Plane (`DRM_MODE_OBJECT_PLANE`).
    Plane,
}

impl DrmObjectKind {
    /// Stable wire/status name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Connector => "connector",
            Self::Crtc => "crtc",
            Self::Plane => "plane",
        }
    }
}

impl std::fmt::Display for DrmObjectKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Which card a lease targets.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DrmCardSelector {
    /// Fail unless a virtual (`vkms`) card is present. Real GPUs are never
    /// selected implicitly.
    AutoVirtual,
    /// An explicit `/dev/dri/cardN` (or `cardN`) named by the operator.
    Explicit(PathBuf),
}

impl DrmCardSelector {
    /// Operator-facing name used in status and errors.
    pub fn display_name(&self) -> String {
        match self {
            Self::AutoVirtual => "auto".to_owned(),
            Self::Explicit(path) => path.display().to_string(),
        }
    }
}

impl FromStr for DrmCardSelector {
    type Err = DrmLeaseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.trim();
        if value.is_empty() {
            return Err(DrmLeaseError::InvalidRequest(
                value.to_owned(),
                "card selector is empty; use auto or an explicit /dev/dri/cardN".to_owned(),
            ));
        }
        match value {
            "auto" | "vkms" => Ok(Self::AutoVirtual),
            other if other.starts_with('/') => Ok(Self::Explicit(PathBuf::from(other))),
            other if other.starts_with("card") => {
                Ok(Self::Explicit(Path::new(DEFAULT_DRM_DEV_ROOT).join(other)))
            }
            other => Err(DrmLeaseError::InvalidRequest(
                value.to_owned(),
                format!("unknown card selector {other:?}; use auto or an explicit /dev/dri/cardN"),
            )),
        }
    }
}

/// Which CRTC a lease should include.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DrmCrtcSelector {
    /// Use the connector's current CRTC when bound, otherwise the first CRTC
    /// not currently bound to another connector.
    #[default]
    Auto,
    /// Lease exactly this CRTC object id.
    Id(u32),
}

impl FromStr for DrmCrtcSelector {
    type Err = DrmLeaseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.trim();
        if value.is_empty() || value.eq_ignore_ascii_case("auto") {
            return Ok(Self::Auto);
        }
        value.parse::<u32>().map(Self::Id).map_err(|_| {
            DrmLeaseError::InvalidRequest(value.to_owned(), "CRTC id must be a u32".to_owned())
        })
    }
}

/// Which plane a lease should include.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DrmPlaneSelector {
    /// Do not name a plane explicitly. The kernel still reports the planes it
    /// associates with the leased CRTC, and the scanout proof can auto-select a
    /// compatible primary plane for an atomic commit.
    #[default]
    Auto,
    /// Lease exactly this plane object id after validating that it is
    /// compatible with the selected CRTC.
    Id(u32),
}

impl DrmPlaneSelector {
    /// The explicit plane object id, if one was requested.
    pub fn id(self) -> Option<u32> {
        match self {
            Self::Auto => None,
            Self::Id(id) => Some(id),
        }
    }
}

impl FromStr for DrmPlaneSelector {
    type Err = DrmLeaseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.trim();
        if value.is_empty() || value.eq_ignore_ascii_case("auto") {
            return Ok(Self::Auto);
        }
        let id = value.parse::<u32>().map_err(|_| {
            DrmLeaseError::InvalidRequest(value.to_owned(), "plane id must be a u32".to_owned())
        })?;
        if id == 0 {
            return Err(DrmLeaseError::InvalidRequest(
                value.to_owned(),
                "plane id must be non-zero".to_owned(),
            ));
        }
        Ok(Self::Id(id))
    }
}

/// One connector known from sysfs or a card inspection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrmLeaseConnector {
    /// Connector name, for example `Virtual-1` or `HDMI-A-1`.
    pub name: String,
    /// Kernel connector status.
    pub status: DrmStatus,
    /// Advertised modes.
    #[serde(default)]
    pub modes: Vec<String>,
    /// Kernel connector object id, when inspection succeeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object_id: Option<u32>,
    /// Currently bound CRTC object id, when any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crtc_id: Option<u32>,
}

/// One DRM card as reported by sysfs and `/dev/dri`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrmCard {
    /// Kernel card name, for example `card0`.
    pub name: String,
    /// Primary node path, for example `/dev/dri/card0`.
    pub path: PathBuf,
    /// Sysfs card directory.
    pub sysfs_path: PathBuf,
    /// Driver name from sysfs (`vkms`, `amdgpu`, `xe`, ...).
    pub driver: String,
    /// Whether automatic selection may lease this card.
    pub virtual_card: bool,
    /// Connectors reported for this card.
    #[serde(default)]
    pub connectors: Vec<DrmLeaseConnector>,
    /// CRTC object ids, when the card was opened with [`DrmLeaseBackend::inspect`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub crtcs: Vec<u32>,
}

impl DrmCard {
    /// Whether this card is a virtual card safe for automatic selection.
    pub fn is_virtual(&self) -> bool {
        self.virtual_card
    }

    /// Whether the card is the kernel `vkms` virtual display.
    ///
    /// Older kernels report the driver as `vkms`; newer kernels bind vkms
    /// through the generic `faux_driver` bus, so the device identity is also
    /// checked.
    pub fn is_vkms(&self) -> bool {
        is_vkms_card(&self.sysfs_path, &self.driver)
    }

    /// Find a connector by name.
    pub fn connector(&self, name: &str) -> Option<&DrmLeaseConnector> {
        self.connectors
            .iter()
            .find(|connector| connector.name == name)
    }
}

/// A validated lease request.
///
/// The CLI form is `[SEAT=]CARD:CONNECTOR[@CRTC][:PLANE]`, for example
/// `/dev/dri/card3:Virtual-1`, `auto:Virtual-1@31`, or
/// `/dev/dri/card3:Virtual-1@31:44`. A plane is validated against the selected
/// CRTC before the lease ioctl and is reported alongside the connector/CRTC.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrmLeaseRequest {
    /// Card selector.
    pub card: DrmCardSelector,
    /// Connector name.
    pub connector: String,
    /// CRTC selector.
    pub crtc: DrmCrtcSelector,
    /// Optional explicit plane selector.
    #[serde(default)]
    pub plane: DrmPlaneSelector,
}

impl DrmLeaseRequest {
    /// Build a request for a card and connector with automatic CRTC choice.
    pub fn new(card: DrmCardSelector, connector: impl Into<String>) -> Self {
        Self {
            card,
            connector: connector.into(),
            crtc: DrmCrtcSelector::Auto,
            plane: DrmPlaneSelector::Auto,
        }
    }

    /// Require a specific CRTC object id.
    pub fn with_crtc(mut self, crtc: u32) -> Self {
        self.crtc = DrmCrtcSelector::Id(crtc);
        self
    }

    /// Require a specific plane object id.
    pub fn with_plane(mut self, plane: u32) -> Self {
        self.plane = DrmPlaneSelector::Id(plane);
        self
    }

    /// The explicit plane object id, if one was requested.
    pub fn plane_id(&self) -> Option<u32> {
        self.plane.id()
    }
}

impl FromStr for DrmLeaseRequest {
    type Err = DrmLeaseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.trim();
        let (card, rest) = value.split_once(':').ok_or_else(|| {
            DrmLeaseError::InvalidRequest(
                value.to_owned(),
                "expected CARD:CONNECTOR, for example /dev/dri/card3:Virtual-1".to_owned(),
            )
        })?;
        let (rest, plane) = match rest.split_once(':') {
            Some((rest, plane)) => (rest, DrmPlaneSelector::from_str(plane)?),
            None => (rest, DrmPlaneSelector::Auto),
        };
        let (connector, crtc) = match rest.split_once('@') {
            Some((connector, crtc)) => (connector, DrmCrtcSelector::from_str(crtc)?),
            None => (rest, DrmCrtcSelector::Auto),
        };
        let connector = connector.trim();
        if connector.is_empty() {
            return Err(DrmLeaseError::InvalidRequest(
                value.to_owned(),
                "connector name is empty".to_owned(),
            ));
        }
        Ok(Self {
            card: DrmCardSelector::from_str(card)?,
            connector: connector.to_owned(),
            crtc,
            plane,
        })
    }
}

/// Object IDs that the kernel reports for a live lease.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrmLeaseObjects {
    /// Leased connector object ids.
    #[serde(default)]
    pub connectors: Vec<u32>,
    /// Leased CRTC object ids.
    #[serde(default)]
    pub crtcs: Vec<u32>,
    /// Leased plane object ids.
    #[serde(default)]
    pub planes: Vec<u32>,
}

/// An owned, active DRM lease.
///
/// Dropping the handle closes the lessee fd, which releases the lease. Call
/// [`DrmLeaseHandle::revoke`] when the lessor still has the card open and an
/// explicit, observable revoke is required.
pub trait DrmLeaseHandle: Send {
    /// Card path that was leased.
    fn card(&self) -> &Path;
    /// Connector name that was leased.
    fn connector(&self) -> &str;
    /// Kernel lessee id.
    fn lessee_id(&self) -> u32;
    /// Leased connector object id.
    fn connector_id(&self) -> u32;
    /// Leased CRTC object id.
    fn crtc_id(&self) -> u32;
    /// Explicitly requested plane object id, when the request named one.
    fn plane_id(&self) -> Option<u32> {
        None
    }
    /// Leased plane object ids, when the kernel reports them.
    fn plane_ids(&self) -> &[u32];
    /// Lessee fd number while the lease is active.
    fn fd(&self) -> Option<RawFd>;
    /// Borrowed lessee fd while the lease is active.
    ///
    /// This is the safe handle used by KMS operations that must run as the
    /// lessee (for example the opt-in scanout proof). It is `None` after
    /// revoke/drop.
    fn as_fd(&self) -> Option<BorrowedFd<'_>> {
        None
    }
    /// Set or clear `FD_CLOEXEC` for child fd inheritance.
    fn set_inheritable(&self, inheritable: bool) -> Result<(), DrmLeaseError>;
    /// Query the kernel for the lease's object IDs.
    fn validate(&self) -> Result<DrmLeaseObjects, DrmLeaseError>;
    /// List active kernel lessee ids on the leased card (requires the lessor's
    /// master fd, which this handle owns).
    fn list_lease_ids(&self) -> Result<Vec<u32>, DrmLeaseError>;
    /// Revoke the lease and close the lessee fd. Idempotent.
    fn revoke(&mut self) -> Result<(), DrmLeaseError>;
    /// Whether the handle still owns a live lease.
    fn is_active(&self) -> bool;
}

/// A backend that can enumerate cards and create leases.
pub trait DrmLeaseBackend: Send + Sync {
    /// Enumerate cards and connectors from sysfs without opening a card.
    fn enumerate(&self) -> Result<Vec<DrmCard>, DrmLeaseError>;

    /// Open one card and fill connector/CRTC object ids. This requires the
    /// same permission as leasing; callers should treat a failure as typed.
    fn inspect(&self, card: &DrmCard) -> Result<DrmCard, DrmLeaseError>;

    /// Create a lease for the requested connector and CRTC.
    fn acquire(&self, request: &DrmLeaseRequest) -> Result<Box<dyn DrmLeaseHandle>, DrmLeaseError>;

    /// List kernel lessee ids for a card.
    fn list_lease_ids(&self, card: &DrmCardSelector) -> Result<Vec<u32>, DrmLeaseError>;
}

/// Typed DRM lease failures.
#[derive(Debug, thiserror::Error)]
pub enum DrmLeaseError {
    /// No usable DRM card was found.
    #[error("no DRM card available: {reason}")]
    NoDrm {
        /// Why no card was usable.
        reason: String,
    },

    /// The card node exists but cannot be opened by this user.
    #[error("no permission to open DRM card {path}: {source}")]
    NoPermission {
        /// Card path.
        path: String,
        /// Underlying open error.
        #[source]
        source: io::Error,
    },

    /// The driver or kernel does not support DRM leasing.
    #[error("DRM leasing is not supported on {card}: {reason}")]
    Unsupported {
        /// Card path or name.
        card: String,
        /// Kernel/driver detail.
        reason: String,
    },

    /// A real card was requested implicitly.
    #[error(
        "DRM card {card} is not a virtual (vkms) card; name an explicit /dev/dri/cardN to lease \
         a real GPU output"
    )]
    ExplicitCardRequired {
        /// Card path or name.
        card: String,
    },

    /// A named connector, CRTC, or plane object does not exist.
    #[error("unknown DRM {kind} {id} on {card}")]
    UnknownObject {
        /// Card path or name.
        card: String,
        /// Object kind.
        kind: DrmObjectKind,
        /// Object id.
        id: u32,
    },

    /// A connector name does not exist on the card.
    #[error("unknown DRM connector {connector:?} on {card}")]
    UnknownConnector {
        /// Card path or name.
        card: String,
        /// Requested connector name.
        connector: String,
    },

    /// A connector/CRTC/plane is already leased, either by this process or
    /// another lessee.
    #[error("DRM {kind} {id} on {card} is already leased")]
    AlreadyLeased {
        /// Card path or name.
        card: String,
        /// Object kind.
        kind: DrmObjectKind,
        /// Object id.
        id: u32,
    },

    /// The requested plane cannot drive the selected CRTC.
    #[error("DRM plane {plane} on {card} is not compatible with CRTC {crtc}")]
    IncompatiblePlane {
        /// Card path or name.
        card: String,
        /// Plane object id.
        plane: u32,
        /// CRTC object id.
        crtc: u32,
    },

    /// The caller is not DRM master or lacks the capability required to lease.
    #[error("DRM leasing on {card} requires DRM master/CAP_SYS_ADMIN: {reason}")]
    CapabilityRequired {
        /// Card path or name.
        card: String,
        /// Kernel detail.
        reason: String,
    },

    /// The named card does not exist.
    #[error("unknown DRM card {0}")]
    UnknownCard(String),

    /// The request itself is malformed.
    #[error("invalid DRM lease request {0:?}: {1}")]
    InvalidRequest(String, String),

    /// The lease was revoked or its fd was closed.
    #[error("DRM lease {0} is no longer active")]
    Released(u32),

    /// A sysfs, `/dev`, or ioctl operation failed.
    #[error("DRM lease I/O error on {path}: {source}")]
    Io {
        /// Path or card.
        path: String,
        /// Underlying error.
        #[source]
        source: io::Error,
    },

    /// The kernel rejected the lease for a reason other than the typed cases
    /// above.
    #[error("DRM lease failed on {card}: {message}")]
    LeaseFailed {
        /// Card path or name.
        card: String,
        /// Kernel detail.
        message: String,
    },
}

impl DrmLeaseError {
    /// Stable machine-readable kind for status documents.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NoDrm { .. } => "no_drm",
            Self::NoPermission { .. } => "no_permission",
            Self::Unsupported { .. } => "unsupported_lease",
            Self::ExplicitCardRequired { .. } => "explicit_card_required",
            Self::UnknownObject { .. } => "unknown_object",
            Self::UnknownConnector { .. } => "unknown_object",
            Self::AlreadyLeased { .. } => "already_leased",
            Self::IncompatiblePlane { .. } => "incompatible_plane",
            Self::CapabilityRequired { .. } => "capability_required",
            Self::UnknownCard(_) => "unknown_card",
            Self::InvalidRequest(_, _) => "invalid_request",
            Self::Released(_) => "released",
            Self::Io { .. } => "io",
            Self::LeaseFailed { .. } => "lease_failed",
        }
    }
}

impl From<DrmLeaseError> for MultiseatError {
    fn from(error: DrmLeaseError) -> Self {
        MultiseatError::DrmLease(error.to_string())
    }
}

/// Kind of a typed lease status note.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DrmLeaseNoteKind {
    /// A lease only hands object IDs to a lessee; it does not create scanout or
    /// compositor isolation.
    LeaseOnlyNoScanout,
    /// The operator explicitly named a non-virtual card.
    RealGpuNamed,
    /// The lease fd was exported to the seat child through the environment.
    FdExported,
}

impl DrmLeaseNoteKind {
    /// Stable wire/status name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LeaseOnlyNoScanout => "lease_only_no_scanout",
            Self::RealGpuNamed => "real_gpu_named",
            Self::FdExported => "fd_exported",
        }
    }
}

/// One operator-facing note attached to a lease status.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrmLeaseNote {
    /// Note kind.
    pub kind: DrmLeaseNoteKind,
    /// Detail.
    pub message: String,
}

impl DrmLeaseNote {
    /// Build a note.
    pub fn new(kind: DrmLeaseNoteKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

/// Typed lease failure reported in status.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrmLeaseErrorStatus {
    /// Stable `DrmLeaseError::code()` value.
    pub kind: String,
    /// Full error message.
    pub message: String,
}

impl DrmLeaseErrorStatus {
    /// Build from a typed lease error.
    pub fn from_error(error: &DrmLeaseError) -> Self {
        Self {
            kind: error.code().to_owned(),
            message: error.to_string(),
        }
    }
}

/// Per-seat DRM lease state, omitted from status when no lease was requested.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrmLeaseStatus {
    /// Card path.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub card: String,
    /// Connector name.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub connector: String,
    /// Kernel lessee id. Retained for audit after release; check `applied`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lessee_id: Option<u32>,
    /// Connector object id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connector_id: Option<u32>,
    /// CRTC object id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crtc_id: Option<u32>,
    /// Explicitly requested plane object id, when the request named one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plane_id: Option<u32>,
    /// Plane object ids reported by the kernel.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub plane_ids: Vec<u32>,
    /// Inherited lessee fd number while active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fd: Option<i32>,
    /// Whether the lease is active.
    #[serde(default)]
    pub applied: bool,
    /// Whether a previously active lease was released.
    #[serde(default)]
    pub released: bool,
    /// Typed limitation notes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<DrmLeaseNote>,
    /// Most recent typed failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<DrmLeaseErrorStatus>,
}

impl DrmLeaseStatus {
    /// Whether no lease was requested or reported.
    pub fn is_empty(&self) -> bool {
        self.card.is_empty()
            && self.connector.is_empty()
            && self.lessee_id.is_none()
            && self.connector_id.is_none()
            && self.crtc_id.is_none()
            && self.plane_id.is_none()
            && self.plane_ids.is_empty()
            && self.fd.is_none()
            && !self.applied
            && !self.released
            && self.notes.is_empty()
            && self.error.is_none()
    }

    /// A requested-but-not-yet-acquired status for dry-run output.
    pub fn requested(request: &DrmLeaseRequest) -> Self {
        Self {
            card: request.card.display_name(),
            connector: request.connector.clone(),
            plane_id: request.plane_id(),
            ..Self::default()
        }
    }

    /// Build the active status from a live handle.
    pub fn from_handle(handle: &dyn DrmLeaseHandle) -> Self {
        let mut status = Self {
            card: handle.card().display().to_string(),
            connector: handle.connector().to_owned(),
            lessee_id: Some(handle.lessee_id()),
            connector_id: Some(handle.connector_id()),
            crtc_id: Some(handle.crtc_id()),
            plane_id: handle.plane_id(),
            plane_ids: handle.plane_ids().to_vec(),
            fd: handle.fd(),
            applied: true,
            released: false,
            ..Self::default()
        };
        status.push_note(DrmLeaseNote::new(
            DrmLeaseNoteKind::LeaseOnlyNoScanout,
            "DRM lease grants only the named connector/CRTC object IDs to this seat's fd; it does \
             not create a compositor, configure scanout, or isolate the host desktop",
        ));
        if handle.fd().is_some() {
            status.push_note(DrmLeaseNote::new(
                DrmLeaseNoteKind::FdExported,
                format!(
                    "{DRM_LEASE_FD_ENV} carries the inherited lessee fd; {DRM_LEASE_ID_ENV} \
                     and object ids are exported alongside it"
                ),
            ));
        }
        status
    }

    /// Mark the lease released while keeping object ids for audit.
    pub fn mark_released(&mut self) {
        self.applied = false;
        self.released = true;
        self.fd = None;
        self.error = None;
    }

    /// Record a typed failure.
    pub fn mark_error(&mut self, error: &DrmLeaseError) {
        self.applied = false;
        self.fd = None;
        self.error = Some(DrmLeaseErrorStatus::from_error(error));
    }

    /// Add one note, deduplicating identical entries.
    pub fn push_note(&mut self, note: DrmLeaseNote) {
        if !self
            .notes
            .iter()
            .any(|existing| existing.kind == note.kind && existing.message == note.message)
        {
            self.notes.push(note);
        }
    }
}

/// Apply the lease's environment exports to a child launch environment.
pub fn apply_lease_environment(status: &DrmLeaseStatus, env: &mut BTreeMap<String, String>) {
    if status.lessee_id.is_none() {
        return;
    }
    env.insert(DRM_LEASE_ENV.to_owned(), "1".to_owned());
    if let Some(id) = status.lessee_id {
        env.insert(DRM_LEASE_ID_ENV.to_owned(), id.to_string());
    }
    if !status.card.is_empty() {
        env.insert(DRM_LEASE_CARD_ENV.to_owned(), status.card.clone());
    }
    if !status.connector.is_empty() {
        env.insert(DRM_LEASE_CONNECTOR_ENV.to_owned(), status.connector.clone());
    }
    if let Some(id) = status.connector_id {
        env.insert(DRM_LEASE_CONNECTOR_ID_ENV.to_owned(), id.to_string());
    }
    if let Some(id) = status.crtc_id {
        env.insert(DRM_LEASE_CRTC_ID_ENV.to_owned(), id.to_string());
    }
    if let Some(id) = status.plane_id {
        env.insert(DRM_LEASE_PLANE_ID_ENV.to_owned(), id.to_string());
    }
    match status.fd {
        Some(fd) => {
            env.insert(DRM_LEASE_FD_ENV.to_owned(), fd.to_string());
        }
        None => {
            env.remove(DRM_LEASE_FD_ENV);
        }
    }
}

/// Remove every lease-related environment key from a child launch environment.
pub fn clear_lease_environment(env: &mut BTreeMap<String, String>) {
    for key in [
        DRM_LEASE_ENV,
        DRM_LEASE_FD_ENV,
        DRM_LEASE_ID_ENV,
        DRM_LEASE_CARD_ENV,
        DRM_LEASE_CONNECTOR_ENV,
        DRM_LEASE_CONNECTOR_ID_ENV,
        DRM_LEASE_CRTC_ID_ENV,
        DRM_LEASE_PLANE_ID_ENV,
    ] {
        env.remove(key);
    }
}

/// Enumerate cards under the default sysfs and `/dev/dri` roots.
pub fn enumerate() -> Result<Vec<DrmCard>, DrmLeaseError> {
    enumerate_at(Path::new(DEFAULT_DRM_ROOT), Path::new(DEFAULT_DRM_DEV_ROOT))
}

/// Enumerate cards under explicit roots without opening a card node.
pub fn enumerate_at(sysfs_root: &Path, dev_root: &Path) -> Result<Vec<DrmCard>, DrmLeaseError> {
    let entries = fs::read_dir(sysfs_root).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            DrmLeaseError::NoDrm {
                reason: format!("{} does not exist", sysfs_root.display()),
            }
        } else {
            DrmLeaseError::Io {
                path: sysfs_root.display().to_string(),
                source: error,
            }
        }
    })?;

    let all_connectors = match crate::drm::detect_connectors(sysfs_root) {
        Ok(connectors) => connectors,
        Err(error) => {
            return Err(DrmLeaseError::Io {
                path: sysfs_root.display().to_string(),
                source: io::Error::other(error.to_string()),
            })
        }
    };

    let mut cards = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| DrmLeaseError::Io {
            path: sysfs_root.display().to_string(),
            source: error,
        })?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !is_card_name(&name) || !entry.path().is_dir() {
            continue;
        }
        let sysfs_path = entry.path();
        let driver = read_driver(&sysfs_path).unwrap_or_else(|| "unknown".to_owned());
        let virtual_card = is_vkms_card(&sysfs_path, &driver);
        let connectors = all_connectors
            .iter()
            .filter(|connector| connector.card == name)
            .map(|connector| DrmLeaseConnector {
                name: connector.connector.clone(),
                status: connector.status.clone(),
                modes: connector.modes.clone(),
                object_id: None,
                crtc_id: None,
            })
            .collect();
        cards.push(DrmCard {
            path: dev_root.join(&name),
            name,
            sysfs_path,
            virtual_card,
            driver,
            connectors,
            crtcs: Vec::new(),
        });
    }

    cards.sort_by_key(|card| card_sort_key(&card.name));
    Ok(cards)
}

fn is_card_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("card") else {
        return false;
    };
    !rest.is_empty() && rest.chars().all(|character| character.is_ascii_digit())
}

fn card_sort_key(name: &str) -> (u64, String) {
    let number = name
        .strip_prefix("card")
        .and_then(|rest| rest.parse::<u64>().ok())
        .unwrap_or(u64::MAX);
    (number, name.to_owned())
}

/// Whether a driver is the only one selected by `AutoVirtual`.
pub fn is_virtual_driver(driver: &str) -> bool {
    matches!(driver.trim(), "vkms" | "faux_driver")
}

fn is_vkms_card(sysfs_card: &Path, driver: &str) -> bool {
    // `vkms` is the historical driver name. Newer kernels bind vkms through
    // the generic faux bus as `faux_driver`; only vkms currently registers a
    // DRM card there.
    if matches!(driver.trim(), VIRTUAL_DRM_DRIVER | "faux_driver") {
        return true;
    }
    let device = sysfs_card.join("device");
    if let Ok(device) = fs::canonicalize(device) {
        if device.file_name().and_then(|name| name.to_str()) == Some(VIRTUAL_DRM_DRIVER) {
            return true;
        }
    }
    fs::read_to_string(sysfs_card.join("device/uevent"))
        .ok()
        .is_some_and(|uevent| uevent.lines().any(|line| line.trim() == "DRIVER=vkms"))
}

fn read_driver(sysfs_card: &Path) -> Option<String> {
    let link = sysfs_card.join("device/driver");
    if let Ok(target) = fs::read_link(&link) {
        if let Some(name) = target.file_name().and_then(|name| name.to_str()) {
            return Some(name.to_owned());
        }
    }
    let uevent = fs::read_to_string(sysfs_card.join("device/uevent")).ok()?;
    uevent.lines().find_map(|line| {
        line.strip_prefix("DRIVER=")
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    })
}

fn map_io(path: &Path, error: io::Error) -> DrmLeaseError {
    match error.raw_os_error() {
        Some(libc::EACCES) | Some(libc::EPERM) => DrmLeaseError::NoPermission {
            path: path.display().to_string(),
            source: error,
        },
        _ => DrmLeaseError::Io {
            path: path.display().to_string(),
            source: error,
        },
    }
}

/// In-process object registry used to turn a same-process duplicate lease into
/// a deterministic typed error before the kernel ioctl. The kernel remains the
/// authority for leases created by other processes.
fn lease_registry() -> &'static Mutex<BTreeMap<(PathBuf, DrmObjectKind, u32), u32>> {
    static REGISTRY: OnceLock<Mutex<BTreeMap<(PathBuf, DrmObjectKind, u32), u32>>> =
        OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn registry_conflict(card: &Path, kind: DrmObjectKind, id: u32) -> Option<u32> {
    lease_registry()
        .lock()
        .expect("DRM lease registry lock")
        .get(&(card.to_path_buf(), kind, id))
        .copied()
}

fn register_lease(
    card: &Path,
    connector_id: u32,
    crtc_id: u32,
    plane_id: Option<u32>,
    lessee_id: u32,
) {
    let mut registry = lease_registry().lock().expect("DRM lease registry lock");
    registry.insert(
        (card.to_path_buf(), DrmObjectKind::Connector, connector_id),
        lessee_id,
    );
    registry.insert(
        (card.to_path_buf(), DrmObjectKind::Crtc, crtc_id),
        lessee_id,
    );
    if let Some(plane_id) = plane_id {
        registry.insert(
            (card.to_path_buf(), DrmObjectKind::Plane, plane_id),
            lessee_id,
        );
    }
}

fn unregister_lease(card: &Path, connector_id: u32, crtc_id: u32, plane_id: Option<u32>) {
    let mut registry = lease_registry().lock().expect("DRM lease registry lock");
    registry.remove(&(card.to_path_buf(), DrmObjectKind::Connector, connector_id));
    registry.remove(&(card.to_path_buf(), DrmObjectKind::Crtc, crtc_id));
    if let Some(plane_id) = plane_id {
        registry.remove(&(card.to_path_buf(), DrmObjectKind::Plane, plane_id));
    }
}

#[cfg(target_os = "linux")]
mod real {
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::num::NonZeroU32;
    use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
    use std::path::{Path, PathBuf};

    use drm::control::{connector, plane, Device as ControlDevice, RawResourceHandle};

    use super::{
        enumerate_at, is_vkms_card, map_io, register_lease, registry_conflict, unregister_lease,
        DrmCard, DrmCardSelector, DrmCrtcSelector, DrmLeaseBackend, DrmLeaseConnector,
        DrmLeaseError, DrmLeaseHandle, DrmLeaseObjects, DrmLeaseRequest, DrmObjectKind,
        DEFAULT_DRM_DEV_ROOT, DEFAULT_DRM_ROOT,
    };

    /// Production DRM lease backend backed by the maintained `drm` crate.
    ///
    /// The backend never uses a hard-coded object id. It resolves the card,
    /// validates the connector against the current resource handles, chooses a
    /// CRTC, and only then issues `DRM_IOCTL_MODE_CREATE_LEASE`.
    pub struct RealDrmLeaseBackend {
        sysfs_root: PathBuf,
        dev_root: PathBuf,
    }

    impl Default for RealDrmLeaseBackend {
        fn default() -> Self {
            Self::new()
        }
    }

    impl RealDrmLeaseBackend {
        /// Backend using `/sys/class/drm` and `/dev/dri`.
        pub fn new() -> Self {
            Self {
                sysfs_root: PathBuf::from(DEFAULT_DRM_ROOT),
                dev_root: PathBuf::from(DEFAULT_DRM_DEV_ROOT),
            }
        }

        /// Backend using explicit roots (tests and unusual hosts).
        pub fn with_roots(sysfs_root: impl Into<PathBuf>, dev_root: impl Into<PathBuf>) -> Self {
            Self {
                sysfs_root: sysfs_root.into(),
                dev_root: dev_root.into(),
            }
        }

        fn card_for_selector(&self, selector: &DrmCardSelector) -> Result<DrmCard, DrmLeaseError> {
            match selector {
                DrmCardSelector::AutoVirtual => {
                    let cards = self.enumerate()?;
                    cards
                        .into_iter()
                        .find(|card| card.is_virtual())
                        .ok_or_else(|| DrmLeaseError::NoDrm {
                            reason: format!(
                                "no virtual ({VIRTUAL}) DRM card is present; name an explicit \
                                 /dev/dri/cardN to lease a real output",
                                VIRTUAL = super::VIRTUAL_DRM_DRIVER
                            ),
                        })
                }
                DrmCardSelector::Explicit(path) => {
                    let name = path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or_default()
                        .to_owned();
                    if !path.exists() {
                        return Err(DrmLeaseError::UnknownCard(path.display().to_string()));
                    }
                    let sysfs_path = self.sysfs_root.join(&name);
                    let driver =
                        super::read_driver(&sysfs_path).unwrap_or_else(|| "unknown".to_owned());
                    let virtual_card = is_vkms_card(&sysfs_path, &driver);
                    let connectors = super::enumerate_at(&self.sysfs_root, &self.dev_root)
                        .ok()
                        .and_then(|cards| cards.into_iter().find(|card| card.name == name))
                        .map(|card| card.connectors)
                        .unwrap_or_default();
                    Ok(DrmCard {
                        name,
                        path: path.clone(),
                        sysfs_path,
                        virtual_card,
                        driver,
                        connectors,
                        crtcs: Vec::new(),
                    })
                }
            }
        }

        fn open_card(&self, card: &DrmCard) -> Result<Card, DrmLeaseError> {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&card.path)
                .map_err(|error| map_io(&card.path, error))?;
            Ok(Card(file))
        }

        fn inspect_card(&self, card: &DrmCard) -> Result<DrmCard, DrmLeaseError> {
            let card_file = self.open_card(card)?;
            let handles = card_file
                .resource_handles()
                .map_err(|error| map_control_error(&card.path, error))?;
            let mut inspected = card.clone();
            inspected.crtcs = handles
                .crtcs()
                .iter()
                .map(|handle| u32::from(*handle))
                .collect();
            inspected.crtcs.sort_unstable();
            let mut connectors = Vec::with_capacity(handles.connectors().len());
            for handle in handles.connectors() {
                let info = card_file
                    .get_connector(*handle, false)
                    .map_err(|error| map_control_error(&card.path, error))?;
                let crtc_id = info
                    .current_encoder()
                    .and_then(|encoder| card_file.get_encoder(encoder).ok())
                    .and_then(|encoder| encoder.crtc())
                    .map(u32::from);
                let modes = info
                    .modes()
                    .iter()
                    .map(|mode| mode.name().to_string_lossy().into_owned())
                    .collect();
                connectors.push(DrmLeaseConnector {
                    name: info.to_string(),
                    status: drm_status(info.state()),
                    modes,
                    object_id: Some(u32::from(*handle)),
                    crtc_id,
                });
            }
            connectors.sort_by(|left, right| left.name.cmp(&right.name));
            inspected.connectors = connectors;
            Ok(inspected)
        }
    }

    impl DrmLeaseBackend for RealDrmLeaseBackend {
        fn enumerate(&self) -> Result<Vec<DrmCard>, DrmLeaseError> {
            enumerate_at(&self.sysfs_root, &self.dev_root)
        }

        fn inspect(&self, card: &DrmCard) -> Result<DrmCard, DrmLeaseError> {
            self.inspect_card(card)
        }

        fn acquire(
            &self,
            request: &DrmLeaseRequest,
        ) -> Result<Box<dyn DrmLeaseHandle>, DrmLeaseError> {
            let card_info = self.card_for_selector(&request.card)?;
            if matches!(request.card, DrmCardSelector::AutoVirtual) && !card_info.is_virtual() {
                return Err(DrmLeaseError::ExplicitCardRequired {
                    card: card_info.name,
                });
            }
            let card = self.open_card(&card_info)?;
            let handles = card
                .resource_handles()
                .map_err(|error| map_control_error(&card_info.path, error))?;

            let mut connector_id = None;
            let mut connector_crtc = None;
            for handle in handles.connectors() {
                let info = match card.get_connector(*handle, false) {
                    Ok(info) => info,
                    Err(error) => return Err(map_control_error(&card_info.path, error)),
                };
                let name = info.to_string();
                if name == request.connector {
                    connector_id = Some(u32::from(*handle));
                    connector_crtc = info
                        .current_encoder()
                        .and_then(|encoder| card.get_encoder(encoder).ok())
                        .and_then(|encoder| encoder.crtc())
                        .map(u32::from);
                    break;
                }
            }
            let connector_id = connector_id.ok_or_else(|| DrmLeaseError::UnknownConnector {
                card: card_info.path.display().to_string(),
                connector: request.connector.clone(),
            })?;

            let crtc_id = match request.crtc {
                DrmCrtcSelector::Id(id) => {
                    if !handles
                        .crtcs()
                        .iter()
                        .any(|handle| u32::from(*handle) == id)
                    {
                        return Err(DrmLeaseError::UnknownObject {
                            card: card_info.path.display().to_string(),
                            kind: DrmObjectKind::Crtc,
                            id,
                        });
                    }
                    id
                }
                DrmCrtcSelector::Auto => choose_crtc(&card, &card_info, &handles, connector_crtc)?,
            };

            // A named plane must exist on this card and be able to drive the
            // selected CRTC. This is checked against the current resource
            // handles before the lease ioctl, so an incompatible pair is a
            // typed request error rather than a generic kernel EINVAL.
            let plane_id = match request.plane {
                super::DrmPlaneSelector::Auto => None,
                super::DrmPlaneSelector::Id(id) => {
                    let plane_object =
                        NonZeroU32::new(id).ok_or_else(|| DrmLeaseError::UnknownObject {
                            card: card_info.path.display().to_string(),
                            kind: DrmObjectKind::Plane,
                            id,
                        })?;
                    let plane_handle = plane::Handle::from(RawResourceHandle::from(plane_object));
                    let plane_info = card.get_plane(plane_handle).map_err(|error| {
                        if error.raw_os_error() == Some(libc::ENOENT) {
                            DrmLeaseError::UnknownObject {
                                card: card_info.path.display().to_string(),
                                kind: DrmObjectKind::Plane,
                                id,
                            }
                        } else {
                            map_control_error(&card_info.path, error)
                        }
                    })?;
                    let compatible = handles
                        .filter_crtcs(plane_info.possible_crtcs())
                        .iter()
                        .any(|handle| u32::from(*handle) == crtc_id);
                    if !compatible {
                        return Err(DrmLeaseError::IncompatiblePlane {
                            card: card_info.path.display().to_string(),
                            plane: id,
                            crtc: crtc_id,
                        });
                    }
                    Some(id)
                }
            };

            // Reject a same-process duplicate against our own active leases
            // before attempting SET_MASTER. Cross-process duplicates are still
            // reported by the kernel as EBUSY.
            if registry_conflict(&card_info.path, DrmObjectKind::Connector, connector_id).is_some()
            {
                return Err(DrmLeaseError::AlreadyLeased {
                    card: card_info.path.display().to_string(),
                    kind: DrmObjectKind::Connector,
                    id: connector_id,
                });
            }
            if registry_conflict(&card_info.path, DrmObjectKind::Crtc, crtc_id).is_some() {
                return Err(DrmLeaseError::AlreadyLeased {
                    card: card_info.path.display().to_string(),
                    kind: DrmObjectKind::Crtc,
                    id: crtc_id,
                });
            }
            if let Some(plane_id) = plane_id {
                if registry_conflict(&card_info.path, DrmObjectKind::Plane, plane_id).is_some() {
                    return Err(DrmLeaseError::AlreadyLeased {
                        card: card_info.path.display().to_string(),
                        kind: DrmObjectKind::Plane,
                        id: plane_id,
                    });
                }
            }

            // Setting DRM master is only safe/automatic for the virtual vkms
            // card. A real GPU usually already has a compositor as master;
            // stealing it would change the operator's desktop modes. For real
            // cards the lease therefore requires the caller to already be
            // master (or root with no other master) and otherwise fails with
            // the typed capability error below.
            if card_info.is_virtual() {
                drm::Device::acquire_master_lock(&card)
                    .map_err(|error| map_master_error(&card_info.path, error))?;
            }

            // The kernel only inserts an explicitly requested plane without
            // also auto-adding the CRTC's primary/cursor planes when the
            // lessor has universal planes enabled. Without this capability an
            // explicit primary plane collides with the auto-added one and the
            // kernel reports ENOSPC from its object idr. With no explicit
            // plane we keep the legacy connector+CRTC lease shape, where the
            // kernel adds the CRTC planes for us.
            if plane_id.is_some() {
                drm::Device::set_client_capability(
                    &card,
                    drm::ClientCapability::UniversalPlanes,
                    true,
                )
                .map_err(|error| map_control_error(&card_info.path, error))?;
            }

            let connector_object =
                NonZeroU32::new(connector_id).ok_or_else(|| DrmLeaseError::LeaseFailed {
                    card: card_info.path.display().to_string(),
                    message: "connector object id was zero".to_owned(),
                })?;
            let crtc_object =
                NonZeroU32::new(crtc_id).ok_or_else(|| DrmLeaseError::LeaseFailed {
                    card: card_info.path.display().to_string(),
                    message: "CRTC object id was zero".to_owned(),
                })?;

            let mut lease_objects = vec![
                RawResourceHandle::from(connector_object),
                RawResourceHandle::from(crtc_object),
            ];
            if let Some(plane_id) = plane_id {
                let plane_object =
                    NonZeroU32::new(plane_id).ok_or_else(|| DrmLeaseError::LeaseFailed {
                        card: card_info.path.display().to_string(),
                        message: "plane object id was zero".to_owned(),
                    })?;
                let raw = RawResourceHandle::from(plane_object);
                if !lease_objects.contains(&raw) {
                    lease_objects.push(raw);
                }
            }

            let (lessee_id, fd) = card.create_lease(&lease_objects, 0).map_err(|error| {
                map_lease_error(&card_info.path, connector_id, crtc_id, plane_id, error)
            })?;

            let objects = match drm::control::get_lease(&fd) {
                Ok(objects) => objects,
                Err(error) => {
                    drop(fd);
                    return Err(DrmLeaseError::LeaseFailed {
                        card: card_info.path.display().to_string(),
                        message: format!("querying the new lease failed: {error}"),
                    });
                }
            };

            let plane_ids = objects
                .planes
                .iter()
                .map(|handle| u32::from(*handle))
                .collect::<Vec<_>>();
            register_lease(
                &card_info.path,
                connector_id,
                crtc_id,
                plane_id,
                lessee_id.get(),
            );

            Ok(Box::new(RealDrmLease {
                card: Some(card),
                card_path: card_info.path,
                connector: request.connector.clone(),
                lessee_id: lessee_id.get(),
                connector_id,
                crtc_id,
                plane_id,
                plane_ids,
                fd: Some(fd),
                active: true,
            }))
        }

        fn list_lease_ids(&self, selector: &DrmCardSelector) -> Result<Vec<u32>, DrmLeaseError> {
            let card_info = self.card_for_selector(selector)?;
            let card = self.open_card(&card_info)?;
            if card_info.is_virtual() {
                drm::Device::acquire_master_lock(&card)
                    .map_err(|error| map_master_error(&card_info.path, error))?;
            }
            let result = card
                .list_lessees()
                .map(|mut lessees| {
                    lessees.sort_unstable();
                    lessees.into_iter().map(|id| id.get()).collect()
                })
                .map_err(|error| map_control_error(&card_info.path, error));
            if card_info.is_virtual() {
                let _ = drm::Device::release_master_lock(&card);
            }
            result
        }
    }

    fn choose_crtc(
        card: &Card,
        card_info: &DrmCard,
        handles: &drm::control::ResourceHandles,
        connector_crtc: Option<u32>,
    ) -> Result<u32, DrmLeaseError> {
        if let Some(id) = connector_crtc {
            return Ok(id);
        }
        let mut used = std::collections::BTreeSet::new();
        for handle in handles.connectors() {
            let Ok(info) = card.get_connector(*handle, false) else {
                continue;
            };
            if let Some(encoder) = info.current_encoder() {
                if let Ok(encoder) = card.get_encoder(encoder) {
                    if let Some(crtc) = encoder.crtc() {
                        used.insert(u32::from(crtc));
                    }
                }
            }
        }
        handles
            .crtcs()
            .iter()
            .map(|handle| u32::from(*handle))
            .find(|id| !used.contains(id))
            .or_else(|| handles.crtcs().first().map(|handle| u32::from(*handle)))
            .ok_or_else(|| DrmLeaseError::UnknownObject {
                card: card_info.path.display().to_string(),
                kind: DrmObjectKind::Crtc,
                id: 0,
            })
    }

    fn map_control_error(card: &Path, error: io::Error) -> DrmLeaseError {
        match error.raw_os_error() {
            Some(libc::EACCES) | Some(libc::EPERM) => DrmLeaseError::CapabilityRequired {
                card: card.display().to_string(),
                reason: error.to_string(),
            },
            Some(libc::ENOTTY) | Some(libc::EOPNOTSUPP) | Some(libc::ENOSYS) => {
                DrmLeaseError::Unsupported {
                    card: card.display().to_string(),
                    reason: error.to_string(),
                }
            }
            _ => map_io(card, error),
        }
    }

    fn map_master_error(card: &Path, error: io::Error) -> DrmLeaseError {
        match error.raw_os_error() {
            Some(libc::EACCES) | Some(libc::EPERM) | Some(libc::EBUSY) => {
                DrmLeaseError::CapabilityRequired {
                    card: card.display().to_string(),
                    reason: format!(
                        "SET_MASTER failed ({error}); another compositor may hold DRM master, or \
                         the process lacks CAP_SYS_ADMIN"
                    ),
                }
            }
            Some(libc::ENOTTY) | Some(libc::EOPNOTSUPP) | Some(libc::ENOSYS) => {
                DrmLeaseError::Unsupported {
                    card: card.display().to_string(),
                    reason: error.to_string(),
                }
            }
            _ => map_io(card, error),
        }
    }

    fn map_lease_error(
        card: &Path,
        connector_id: u32,
        crtc_id: u32,
        plane_id: Option<u32>,
        error: io::Error,
    ) -> DrmLeaseError {
        match error.raw_os_error() {
            Some(libc::EACCES) | Some(libc::EPERM) => DrmLeaseError::CapabilityRequired {
                card: card.display().to_string(),
                reason: error.to_string(),
            },
            Some(libc::ENOTTY) | Some(libc::EOPNOTSUPP) | Some(libc::ENOSYS) => {
                DrmLeaseError::Unsupported {
                    card: card.display().to_string(),
                    reason: error.to_string(),
                }
            }
            Some(libc::ENOENT) => DrmLeaseError::UnknownObject {
                card: card.display().to_string(),
                kind: DrmObjectKind::Connector,
                id: connector_id,
            },
            Some(libc::EBUSY) => DrmLeaseError::AlreadyLeased {
                card: card.display().to_string(),
                kind: DrmObjectKind::Connector,
                id: connector_id,
            },
            Some(libc::EINVAL) => {
                // The kernel returns EINVAL for invalid combinations and for an
                // object already leased by another lessee. An explicitly named
                // plane is the most likely invalid combination, then the CRTC.
                if let Some(plane_id) = plane_id {
                    DrmLeaseError::IncompatiblePlane {
                        card: card.display().to_string(),
                        plane: plane_id,
                        crtc: crtc_id,
                    }
                } else if crtc_id != 0 {
                    DrmLeaseError::AlreadyLeased {
                        card: card.display().to_string(),
                        kind: DrmObjectKind::Crtc,
                        id: crtc_id,
                    }
                } else {
                    DrmLeaseError::LeaseFailed {
                        card: card.display().to_string(),
                        message: error.to_string(),
                    }
                }
            }
            _ => DrmLeaseError::LeaseFailed {
                card: card.display().to_string(),
                message: error.to_string(),
            },
        }
    }

    fn drm_status(state: connector::State) -> crate::drm::DrmStatus {
        match state {
            connector::State::Connected => crate::drm::DrmStatus::Connected,
            connector::State::Disconnected => crate::drm::DrmStatus::Disconnected,
            connector::State::Unknown => crate::drm::DrmStatus::Unknown("unknown".to_owned()),
        }
    }

    #[derive(Debug)]
    struct Card(File);

    impl AsFd for Card {
        fn as_fd(&self) -> BorrowedFd<'_> {
            self.0.as_fd()
        }
    }

    impl drm::Device for Card {}
    impl ControlDevice for Card {}

    struct RealDrmLease {
        card: Option<Card>,
        card_path: PathBuf,
        connector: String,
        lessee_id: u32,
        connector_id: u32,
        crtc_id: u32,
        plane_id: Option<u32>,
        plane_ids: Vec<u32>,
        fd: Option<OwnedFd>,
        active: bool,
    }

    impl DrmLeaseHandle for RealDrmLease {
        fn card(&self) -> &Path {
            &self.card_path
        }

        fn connector(&self) -> &str {
            &self.connector
        }

        fn lessee_id(&self) -> u32 {
            self.lessee_id
        }

        fn connector_id(&self) -> u32 {
            self.connector_id
        }

        fn crtc_id(&self) -> u32 {
            self.crtc_id
        }

        fn plane_id(&self) -> Option<u32> {
            self.plane_id
        }

        fn plane_ids(&self) -> &[u32] {
            &self.plane_ids
        }

        fn fd(&self) -> Option<RawFd> {
            self.fd.as_ref().map(AsRawFd::as_raw_fd)
        }

        fn as_fd(&self) -> Option<BorrowedFd<'_>> {
            self.fd.as_ref().map(AsFd::as_fd)
        }

        fn set_inheritable(&self, inheritable: bool) -> Result<(), DrmLeaseError> {
            let Some(fd) = self.fd.as_ref() else {
                return Err(DrmLeaseError::Released(self.lessee_id));
            };
            set_cloexec(fd, !inheritable).map_err(|error| DrmLeaseError::LeaseFailed {
                card: self.card_path.display().to_string(),
                message: format!("updating FD_CLOEXEC on the lessee fd: {error}"),
            })
        }

        fn validate(&self) -> Result<DrmLeaseObjects, DrmLeaseError> {
            if !self.active {
                return Err(DrmLeaseError::Released(self.lessee_id));
            }
            let Some(fd) = self.fd.as_ref() else {
                return Err(DrmLeaseError::Released(self.lessee_id));
            };
            let objects =
                drm::control::get_lease(fd).map_err(|error| DrmLeaseError::LeaseFailed {
                    card: self.card_path.display().to_string(),
                    message: error.to_string(),
                })?;
            Ok(DrmLeaseObjects {
                connectors: objects
                    .connectors
                    .iter()
                    .map(|handle| u32::from(*handle))
                    .collect(),
                crtcs: objects
                    .crtcs
                    .iter()
                    .map(|handle| u32::from(*handle))
                    .collect(),
                planes: objects
                    .planes
                    .iter()
                    .map(|handle| u32::from(*handle))
                    .collect(),
            })
        }

        fn list_lease_ids(&self) -> Result<Vec<u32>, DrmLeaseError> {
            if !self.active {
                return Err(DrmLeaseError::Released(self.lessee_id));
            }
            let Some(card) = self.card.as_ref() else {
                return Err(DrmLeaseError::Released(self.lessee_id));
            };
            let mut lessees = card
                .list_lessees()
                .map_err(|error| DrmLeaseError::LeaseFailed {
                    card: self.card_path.display().to_string(),
                    message: format!("listing lessees: {error}"),
                })?;
            lessees.sort_unstable();
            Ok(lessees.into_iter().map(|id| id.get()).collect())
        }

        fn revoke(&mut self) -> Result<(), DrmLeaseError> {
            if !self.active {
                return Ok(());
            }
            let Some(card) = self.card.as_ref() else {
                self.active = false;
                self.fd = None;
                return Ok(());
            };
            let Some(lessee) = NonZeroU32::new(self.lessee_id) else {
                self.active = false;
                self.fd = None;
                return Ok(());
            };
            let result = card
                .revoke_lease(lessee)
                .map_err(|error| DrmLeaseError::LeaseFailed {
                    card: self.card_path.display().to_string(),
                    message: format!("revoking lease {}: {error}", self.lessee_id),
                });
            match result {
                Ok(()) => {
                    let _ = drm::Device::release_master_lock(card);
                    unregister_lease(
                        &self.card_path,
                        self.connector_id,
                        self.crtc_id,
                        self.plane_id,
                    );
                    self.active = false;
                    self.fd = None;
                    Ok(())
                }
                Err(error) => Err(error),
            }
        }

        fn is_active(&self) -> bool {
            self.active && self.fd.is_some()
        }
    }

    impl Drop for RealDrmLease {
        fn drop(&mut self) {
            // The kernel also releases a lease when the lessee fd closes; the
            // explicit revoke makes the lessor-observable transition immediate
            // when the card fd is still available.
            let _ = self.revoke();
            if let Some(card) = self.card.as_ref() {
                let _ = drm::Device::release_master_lock(card);
            }
            unregister_lease(
                &self.card_path,
                self.connector_id,
                self.crtc_id,
                self.plane_id,
            );
        }
    }

    fn set_cloexec(fd: &impl AsFd, cloexec: bool) -> Result<(), nix::errno::Errno> {
        use nix::fcntl::{fcntl, FcntlArg, FdFlag};
        let current = FdFlag::from_bits_truncate(fcntl(fd, FcntlArg::F_GETFD)?);
        let updated = if cloexec {
            current | FdFlag::FD_CLOEXEC
        } else {
            current & !FdFlag::FD_CLOEXEC
        };
        fcntl(fd, FcntlArg::F_SETFD(updated))?;
        Ok(())
    }
}

#[cfg(target_os = "linux")]
pub use real::RealDrmLeaseBackend;
