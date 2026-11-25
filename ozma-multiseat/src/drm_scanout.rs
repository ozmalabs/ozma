// SPDX-License-Identifier: AGPL-3.0-only
//! Opt-in KMS scanout proof over a DRM lease.
//!
//! A lease alone only hands object IDs to a lessee. This module goes one step
//! further and proves the leased objects are actually usable for scanout:
//!
//! 1. acquire a lease for one connector, one CRTC, and optionally one validated
//!    plane;
//! 2. choose one of the connector's advertised modes;
//! 3. allocate a dumb buffer, fill a known pattern, and add a framebuffer;
//! 4. program the connector/CRTC/plane with an atomic property commit
//!    (`set_plane` + `set_crtc` state) and a page-flip event, falling back to
//!    legacy `set_crtc`/`page_flip` only when atomic KMS is unsupported;
//! 5. re-read CRTC/plane/connector/framebuffer state and compare it with what
//!    was requested;
//! 6. clear the plane/CRTC, destroy the framebuffer and dumb buffer, and revoke
//!    the lease.
//!
//! This is a **scanout proof**, not a compositor. It renders one static,
//! generated pattern directly to one leased output. It does not accept client
//! surfaces, composite windows, perform damage tracking, provide a Wayland/X
//! server, or isolate the host desktop. Status always carries a
//! [`DrmScanoutNoteKind::ScanoutProofOnly`] note saying exactly that.
//!
//! Card selection is conservative: `auto`/`vkms` only picks a sysfs `vkms`
//! card, and a real GPU output is only attempted when the operator names its
//! `/dev/dri/cardN` explicitly. The kernel ABI is provided by the `drm` crate;
//! this module adds no `unsafe` code.

use std::io;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::drm_lease::{DrmCard, DrmCardSelector, DrmLeaseError, DrmLeaseRequest};

/// Stable name of the generated scanout pattern.
pub const SCANOUT_PATTERN: &str = "quadrants-border-diagonal-xrgb8888";
/// Pixel format used by the proof.
pub const SCANOUT_FOURCC: &str = "XR24";
/// Dumb-buffer bits per pixel.
pub const SCANOUT_BPP: u32 = 32;
/// Framebuffer depth reported to KMS.
pub const SCANOUT_DEPTH: u32 = 24;
/// Border thickness in pixels.
const BORDER_PX: u32 = 8;
/// Diagonal thickness in pixels.
const DIAGONAL_PX: u32 = 4;
/// Upper bound on properties enumerated from one KMS object. A driver that
/// reports more is treated as a typed failure rather than looping unbounded.
const MAX_ATOMIC_PROPERTIES: usize = 256;

/// Which KMS commit API actually programmed the scanout.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DrmScanoutCommit {
    /// Atomic `set_plane` + `set_crtc` property commit with a page-flip event.
    Atomic,
    /// Legacy `set_crtc` followed by `page_flip`.
    #[default]
    Legacy,
}

impl DrmScanoutCommit {
    /// Stable wire/status name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Atomic => "atomic",
            Self::Legacy => "legacy",
        }
    }
}

/// A validated scanout request.
///
/// The CLI form is the same `[SEAT=]CARD:CONNECTOR[@CRTC][:PLANE]` selector as
/// [`DrmLeaseRequest`], with an optional explicit mode name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrmScanoutRequest {
    /// Lease selector (card, connector, optional CRTC, optional plane).
    pub lease: DrmLeaseRequest,
    /// Exact connector mode name. `None` selects the connector's first
    /// advertised (normally preferred) mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Force the legacy `set_crtc` proof even when atomic KMS is available.
    /// Used by the live test to establish a legacy baseline when vkms exposes
    /// no leaseable plane.
    #[serde(default, skip_serializing_if = "is_false")]
    pub force_legacy: bool,
}

impl DrmScanoutRequest {
    /// Build a request with automatic mode selection.
    pub fn new(lease: DrmLeaseRequest) -> Self {
        Self {
            lease,
            mode: None,
            force_legacy: false,
        }
    }

    /// Require an exact mode name.
    pub fn with_mode(mut self, mode: impl Into<String>) -> Self {
        self.mode = Some(mode.into());
        self
    }

    /// Request an explicit plane object id in the underlying lease.
    pub fn with_plane(mut self, plane: u32) -> Self {
        self.lease = self.lease.with_plane(plane);
        self
    }

    /// Force the legacy `set_crtc` path for a deliberate legacy baseline.
    pub fn with_legacy_commit(mut self) -> Self {
        self.force_legacy = true;
        self
    }
}

impl FromStr for DrmScanoutRequest {
    type Err = DrmLeaseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(Self::new(value.parse::<DrmLeaseRequest>()?))
    }
}

/// Kind of a typed scanout status note.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DrmScanoutNoteKind {
    /// The module programs one static framebuffer; it is not a compositor.
    ScanoutProofOnly,
    /// The proof used the lease's lessee fd rather than the lessor master fd.
    LesseeFd,
    /// The operator explicitly named a non-virtual card.
    RealGpuNamed,
    /// A page flip could not be queued; the `set_crtc` commit was still made.
    PageFlipUnavailable,
    /// Atomic KMS was unsupported, so the proof used the legacy commit path.
    LegacyCommitFallback,
    /// The proof deliberately used the legacy commit path as a baseline.
    LegacyCommitForced,
    /// The lease includes an explicitly validated plane.
    PlaneLeased,
}

impl DrmScanoutNoteKind {
    /// Stable wire/status name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ScanoutProofOnly => "scanout_proof_only",
            Self::LesseeFd => "lessee_fd",
            Self::RealGpuNamed => "real_gpu_named",
            Self::PageFlipUnavailable => "page_flip_unavailable",
            Self::LegacyCommitFallback => "legacy_commit_fallback",
            Self::LegacyCommitForced => "legacy_commit_forced",
            Self::PlaneLeased => "plane_leased",
        }
    }
}

/// One operator-facing scanout note.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrmScanoutNote {
    /// Note kind.
    pub kind: DrmScanoutNoteKind,
    /// Detail.
    pub message: String,
}

impl DrmScanoutNote {
    /// Build a note.
    pub fn new(kind: DrmScanoutNoteKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

/// Typed scanout failure reported in status.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrmScanoutErrorStatus {
    /// Stable `DrmScanoutError::code()` value.
    pub kind: String,
    /// Full error message.
    pub message: String,
}

impl DrmScanoutErrorStatus {
    /// Build from a typed scanout error.
    pub fn from_error(error: &DrmScanoutError) -> Self {
        Self {
            kind: error.code().to_owned(),
            message: error.to_string(),
        }
    }
}

/// Evidence gathered by [`DrmScanoutHandle::verify`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrmScanoutVerification {
    /// CRTC object id.
    pub crtc_id: u32,
    /// Mode programmed on the CRTC, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crtc_mode: Option<String>,
    /// Framebuffer attached to the CRTC, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crtc_framebuffer: Option<u32>,
    /// CRTC position.
    pub crtc_position: (u32, u32),
    /// Connector state (`connected`, `disconnected`, `unknown`).
    pub connector_state: String,
    /// CRTC currently bound through the connector's encoder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connector_crtc: Option<u32>,
    /// Plane object id used by the proof, when one was selected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plane_id: Option<u32>,
    /// CRTC reported by the plane, when a plane was used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plane_crtc: Option<u32>,
    /// Framebuffer reported by the plane, when a plane was used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plane_framebuffer: Option<u32>,
    /// Framebuffer object id.
    pub framebuffer_id: u32,
    /// Framebuffer width/height.
    pub framebuffer_size: (u32, u32),
    /// Framebuffer pitch.
    pub framebuffer_pitch: u32,
    /// Framebuffer bits per pixel.
    pub framebuffer_bpp: u32,
    /// Framebuffer depth.
    pub framebuffer_depth: u32,
    /// GEM buffer handle attached to the framebuffer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub framebuffer_buffer_handle: Option<u32>,
    /// Dumb-buffer width/height.
    pub buffer_size: (u32, u32),
    /// Dumb-buffer pitch.
    pub buffer_pitch: u32,
    /// Dumb-buffer GEM handle.
    pub buffer_handle: u32,
    /// FNV-1a checksum of the pattern written into the mapping.
    pub pattern_checksum: u32,
    /// Whether every checked field matched the request.
    pub ok: bool,
    /// First mismatch, when `ok` is false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mismatch: Option<String>,
}

/// Per-seat KMS scanout state, omitted from status when no proof was requested.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrmScanoutStatus {
    /// Card path.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub card: String,
    /// Connector name.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub connector: String,
    /// Connector object id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connector_id: Option<u32>,
    /// CRTC object id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crtc_id: Option<u32>,
    /// Plane object id used by the proof, when one was selected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plane_id: Option<u32>,
    /// Which commit API programmed scanout (`atomic` or `legacy`).
    #[serde(default)]
    pub commit: DrmScanoutCommit,
    /// Lessee id from the underlying lease.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lessee_id: Option<u32>,
    /// Lessee fd number while the proof is active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lessee_fd: Option<i32>,
    /// Programmed mode name.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub mode: String,
    /// Framebuffer width.
    #[serde(default)]
    pub width: u32,
    /// Framebuffer height.
    #[serde(default)]
    pub height: u32,
    /// Dumb-buffer pitch.
    #[serde(default)]
    pub pitch: u32,
    /// Framebuffer object id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub framebuffer_id: Option<u32>,
    /// GEM buffer handle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buffer_handle: Option<u32>,
    /// Pattern name.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub pattern: String,
    /// Pattern checksum.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern_checksum: Option<u32>,
    /// Whether a page flip was accepted after the modeset.
    #[serde(default)]
    pub page_flipped: bool,
    /// Verification evidence, when `verify` ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification: Option<DrmScanoutVerification>,
    /// Whether the proof is currently scanning out.
    #[serde(default)]
    pub applied: bool,
    /// Whether teardown completed.
    #[serde(default)]
    pub released: bool,
    /// Typed limitation notes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<DrmScanoutNote>,
    /// Most recent typed failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<DrmScanoutErrorStatus>,
}

impl DrmScanoutStatus {
    /// Whether no proof was requested or reported.
    pub fn is_empty(&self) -> bool {
        self.card.is_empty()
            && self.connector.is_empty()
            && self.connector_id.is_none()
            && self.crtc_id.is_none()
            && self.plane_id.is_none()
            && self.lessee_id.is_none()
            && self.lessee_fd.is_none()
            && self.mode.is_empty()
            && self.width == 0
            && self.height == 0
            && self.pitch == 0
            && self.framebuffer_id.is_none()
            && self.buffer_handle.is_none()
            && self.pattern.is_empty()
            && self.pattern_checksum.is_none()
            && !self.page_flipped
            && self.verification.is_none()
            && !self.applied
            && !self.released
            && self.notes.is_empty()
            && self.error.is_none()
    }

    /// A requested-but-not-yet-applied status for plan/dry-run output.
    pub fn requested(request: &DrmScanoutRequest) -> Self {
        Self {
            card: request.lease.card.display_name(),
            connector: request.lease.connector.clone(),
            plane_id: request.lease.plane_id(),
            mode: request.mode.clone().unwrap_or_default(),
            pattern: SCANOUT_PATTERN.to_owned(),
            ..Self::default()
        }
    }

    /// Mark the proof released while keeping ids for audit.
    pub fn mark_released(&mut self) {
        self.applied = false;
        self.released = true;
        self.lessee_fd = None;
        self.error = None;
    }

    /// Record a typed failure.
    pub fn mark_error(&mut self, error: &DrmScanoutError) {
        self.applied = false;
        self.lessee_fd = None;
        self.error = Some(DrmScanoutErrorStatus::from_error(error));
    }

    /// Add one note, deduplicating identical entries.
    pub fn push_note(&mut self, note: DrmScanoutNote) {
        if !self
            .notes
            .iter()
            .any(|existing| existing.kind == note.kind && existing.message == note.message)
        {
            self.notes.push(note);
        }
    }

    /// Attach the always-on proof-only note.
    pub fn push_proof_note(&mut self) {
        self.push_note(DrmScanoutNote::new(
            DrmScanoutNoteKind::ScanoutProofOnly,
            "one static dumb-buffer pattern is programmed on one leased connector/CRTC; this is \
             not a compositor, window manager, client renderer, or host display isolation",
        ));
    }
}

/// An owned, active scanout proof.
///
/// Dropping the handle clears the CRTC and destroys framebuffer/buffer as a
/// best effort before revoking the lease.
pub trait DrmScanoutHandle: Send {
    /// Current scanout status.
    fn status(&self) -> &DrmScanoutStatus;
    /// Re-read KMS state and compare it with the requested mode/framebuffer.
    fn verify(&mut self) -> Result<DrmScanoutVerification, DrmScanoutError>;
    /// Clear scanout, destroy the framebuffer and dumb buffer, and revoke the
    /// lease. Idempotent.
    fn teardown(&mut self) -> Result<(), DrmScanoutError>;
    /// Whether the proof is still scanning out.
    fn is_active(&self) -> bool;
}

/// A backend that can create a scanout proof.
pub trait DrmScanoutBackend: Send + Sync {
    /// Acquire a lease, allocate/fill a buffer, program the CRTC, verify the
    /// result, and return an owned handle.
    fn start(
        &self,
        request: &DrmScanoutRequest,
    ) -> Result<Box<dyn DrmScanoutHandle>, DrmScanoutError>;
}

/// Typed scanout failures.
#[derive(Debug, thiserror::Error)]
pub enum DrmScanoutError {
    /// No usable DRM card was found.
    #[error("no DRM card available for scanout: {reason}")]
    NoDrm {
        /// Why no card was usable.
        reason: String,
    },

    /// The card node exists but cannot be opened.
    #[error("no permission to open DRM card {path}: {source}")]
    NoPermission {
        /// Card path.
        path: String,
        /// Underlying open error.
        #[source]
        source: io::Error,
    },

    /// The caller is not DRM master and the kernel refused it.
    #[error("scanout on {card} requires DRM master/CAP_SYS_ADMIN: {reason}")]
    CapabilityRequired {
        /// Card path or name.
        card: String,
        /// Kernel detail.
        reason: String,
    },

    /// The selected card is not a vkms virtual card.
    #[error("DRM card {card} is not the virtual vkms backend (driver {driver})")]
    UnsupportedVkms {
        /// Card path or name.
        card: String,
        /// Driver name.
        driver: String,
    },

    /// A real card was selected without an explicit card path.
    #[error(
        "refusing scanout on real DRM card {card} ({driver}) without an explicit /dev/dri/cardN"
    )]
    RealCardRefused {
        /// Card path or name.
        card: String,
        /// Driver name.
        driver: String,
    },

    /// The connector is missing, disconnected, or advertises no usable mode.
    #[error("unsupported DRM connector {connector:?} on {card}: {reason}")]
    UnsupportedConnector {
        /// Card path or name.
        card: String,
        /// Connector name.
        connector: String,
        /// Detail.
        reason: String,
    },

    /// Atomic KMS is not supported by the driver or kernel.
    #[error("atomic KMS is not supported on {card}: {reason}")]
    AtomicUnsupported {
        /// Card path or name.
        card: String,
        /// Kernel detail.
        reason: String,
    },

    /// No leaseable plane can drive the selected CRTC.
    #[error("no usable DRM plane for CRTC {crtc} on {card}: {reason}")]
    UnsupportedPlane {
        /// Card path or name.
        card: String,
        /// CRTC object id.
        crtc: u32,
        /// Detail.
        reason: String,
    },

    /// A plane does not support the proof's pixel format.
    #[error("DRM plane {plane} on {card} does not support format {format}: {reason}")]
    InvalidFormat {
        /// Card path or name.
        card: String,
        /// Plane object id.
        plane: u32,
        /// Required pixel format.
        format: String,
        /// Detail.
        reason: String,
    },

    /// A required atomic property was absent from an object.
    #[error("DRM {object} {id} on {card} has no atomic property {property:?}")]
    MissingAtomicProperty {
        /// Card path or name.
        card: String,
        /// Object kind.
        object: String,
        /// Object id.
        id: u32,
        /// Property name.
        property: String,
    },

    /// Property enumeration exceeded the proof's fixed bound.
    #[error("DRM {object} {id} on {card} reports more than {limit} properties")]
    PropertyLimitExceeded {
        /// Card path or name.
        card: String,
        /// Object kind.
        object: String,
        /// Object id.
        id: u32,
        /// Bound.
        limit: usize,
    },

    /// The requested mode is not advertised, or the kernel rejected the modeset.
    #[error("failed to set mode {mode:?} on {card}:{connector}: {reason}")]
    ModeFailed {
        /// Card path or name.
        card: String,
        /// Connector name.
        connector: String,
        /// Requested mode (or `auto`).
        mode: String,
        /// Kernel/validation detail.
        reason: String,
    },

    /// Dumb-buffer allocation or mapping failed.
    #[error("failed to allocate/fill a dumb buffer on {card}: {reason}")]
    BufferFailed {
        /// Card path or name.
        card: String,
        /// Kernel detail.
        reason: String,
    },

    /// Framebuffer creation failed.
    #[error("failed to add a framebuffer on {card}: {reason}")]
    FramebufferFailed {
        /// Card path or name.
        card: String,
        /// Kernel detail.
        reason: String,
    },

    /// Post-modeset verification did not match the request.
    #[error("scanout verification failed on {card}:{connector}: {message}")]
    VerificationFailed {
        /// Card path or name.
        card: String,
        /// Connector name.
        connector: String,
        /// First mismatch.
        message: String,
    },

    /// The proof was already torn down.
    #[error("scanout proof on {0} is no longer active")]
    Released(String),

    /// The request itself is malformed.
    #[error("invalid DRM scanout request {0:?}: {1}")]
    InvalidRequest(String, String),

    /// The underlying lease could not be created or validated.
    #[error(transparent)]
    Lease(#[from] DrmLeaseError),

    /// A card operation failed for another reason.
    #[error("DRM scanout I/O error on {path}: {source}")]
    Io {
        /// Path or card.
        path: String,
        /// Underlying error.
        #[source]
        source: io::Error,
    },
}

impl DrmScanoutError {
    /// Stable machine-readable kind for status documents.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NoDrm { .. } => "no_drm",
            Self::NoPermission { .. } => "no_permission",
            Self::CapabilityRequired { .. } => "capability_required",
            Self::UnsupportedVkms { .. } => "unsupported_vkms",
            Self::RealCardRefused { .. } => "real_card_refused",
            Self::UnsupportedConnector { .. } => "unsupported_connector",
            Self::AtomicUnsupported { .. } => "atomic_unsupported",
            Self::UnsupportedPlane { .. } => "unsupported_plane",
            Self::InvalidFormat { .. } => "invalid_format",
            Self::MissingAtomicProperty { .. } => "missing_atomic_property",
            Self::PropertyLimitExceeded { .. } => "property_limit_exceeded",
            Self::ModeFailed { .. } => "mode_failed",
            Self::BufferFailed { .. } => "buffer_failed",
            Self::FramebufferFailed { .. } => "framebuffer_failed",
            Self::VerificationFailed { .. } => "verification_failed",
            Self::Released(_) => "released",
            Self::InvalidRequest(_, _) => "invalid_request",
            Self::Lease(error) => error.code(),
            Self::Io { .. } => "io",
        }
    }
}

impl From<DrmScanoutError> for crate::error::MultiseatError {
    fn from(error: DrmScanoutError) -> Self {
        Self::DrmScanout(error.to_string())
    }
}

/// Enforce the card policy shared by every scanout backend.
///
/// `AutoVirtual` never accepts a real card; an explicit `/dev/dri/cardN` may be
/// real but is reported as such in status. This function is public so the
/// policy can be unit-tested without opening a device.
pub fn ensure_scanout_card(
    card: &DrmCard,
    selector: &DrmCardSelector,
) -> Result<(), DrmScanoutError> {
    if card.is_virtual() {
        return Ok(());
    }
    match selector {
        DrmCardSelector::AutoVirtual => Err(DrmScanoutError::RealCardRefused {
            card: card.path.display().to_string(),
            driver: card.driver.clone(),
        }),
        DrmCardSelector::Explicit(_) => Ok(()),
    }
}

#[cfg(target_os = "linux")]
fn drm_status_name(state: drm::control::connector::State) -> String {
    match state {
        drm::control::connector::State::Connected => "connected".to_owned(),
        drm::control::connector::State::Disconnected => "disconnected".to_owned(),
        drm::control::connector::State::Unknown => "unknown".to_owned(),
    }
}

#[cfg(target_os = "linux")]
mod real {
    use std::collections::BTreeMap;
    use std::num::NonZeroU32;
    use std::os::fd::{AsFd, BorrowedFd};
    use std::path::Path;

    use drm::buffer::{Buffer, DrmFourcc};
    use drm::control::{
        atomic::AtomicModeReq, connector, crtc, framebuffer, plane, property, AtomicCommitFlags,
        Device as ControlDevice, Mode, PageFlipFlags, RawResourceHandle, ResourceHandle,
    };
    use drm::ClientCapability;
    use drm::Device;

    use super::{
        drm_status_name, ensure_scanout_card, fill_pattern, pattern_checksum, DrmScanoutBackend,
        DrmScanoutCommit, DrmScanoutError, DrmScanoutHandle, DrmScanoutNote, DrmScanoutNoteKind,
        DrmScanoutRequest, DrmScanoutStatus, DrmScanoutVerification, BORDER_PX, DIAGONAL_PX,
        MAX_ATOMIC_PROPERTIES, SCANOUT_BPP, SCANOUT_DEPTH, SCANOUT_FOURCC, SCANOUT_PATTERN,
    };
    use crate::drm_lease::{
        DrmCard, DrmCardSelector, DrmLeaseBackend, DrmLeaseHandle, RealDrmLeaseBackend,
    };

    /// Production scanout proof over [`RealDrmLeaseBackend`].
    pub struct RealDrmScanoutBackend {
        lease: RealDrmLeaseBackend,
    }

    impl Default for RealDrmScanoutBackend {
        fn default() -> Self {
            Self::new()
        }
    }

    impl RealDrmScanoutBackend {
        /// Backend using `/sys/class/drm` and `/dev/dri`.
        pub fn new() -> Self {
            Self {
                lease: RealDrmLeaseBackend::new(),
            }
        }

        /// Backend using explicit roots (tests and unusual hosts).
        pub fn with_roots(
            sysfs_root: impl Into<std::path::PathBuf>,
            dev_root: impl Into<std::path::PathBuf>,
        ) -> Self {
            Self {
                lease: RealDrmLeaseBackend::with_roots(sysfs_root, dev_root),
            }
        }

        fn resolve_card(&self, selector: &DrmCardSelector) -> Result<DrmCard, DrmScanoutError> {
            let cards = self.lease.enumerate()?;
            match selector {
                DrmCardSelector::AutoVirtual => cards
                    .into_iter()
                    .find(|card| card.is_virtual())
                    .ok_or_else(|| DrmScanoutError::NoDrm {
                        reason: "no virtual (vkms) DRM card is present; name an explicit \
                                 /dev/dri/cardN to scan out on a real output"
                            .to_owned(),
                    }),
                DrmCardSelector::Explicit(path) => cards
                    .into_iter()
                    .find(|card| card.path == *path)
                    .ok_or_else(|| DrmScanoutError::NoDrm {
                        reason: format!("unknown DRM card {}", path.display()),
                    }),
            }
        }
    }

    impl DrmScanoutBackend for RealDrmScanoutBackend {
        fn start(
            &self,
            request: &DrmScanoutRequest,
        ) -> Result<Box<dyn DrmScanoutHandle>, DrmScanoutError> {
            let card = self.resolve_card(&request.lease.card)?;
            ensure_scanout_card(&card, &request.lease.card)?;

            let lease = self.lease.acquire(&request.lease)?;
            let connector_id = lease.connector_id();
            let crtc_id = lease.crtc_id();
            let Some(fd) = lease.as_fd() else {
                return Err(DrmScanoutError::UnsupportedVkms {
                    card: card.path.display().to_string(),
                    driver: card.driver.clone(),
                });
            };
            let device = LeaseDevice { fd };

            let connector_handle = connector_handle(connector_id, &card.path)?;
            let crtc_handle = crtc_handle(crtc_id, &card.path)?;

            let connector_info = device
                .get_connector(connector_handle, false)
                .map_err(|error| map_io(&card.path, error))?;
            if !matches!(connector_info.state(), connector::State::Connected) {
                return Err(DrmScanoutError::UnsupportedConnector {
                    card: card.path.display().to_string(),
                    connector: request.lease.connector.clone(),
                    reason: format!(
                        "connector state is {:?}, not connected",
                        connector_info.state()
                    ),
                });
            }
            let mode =
                choose_mode(connector_info.modes(), request.mode.as_deref()).ok_or_else(|| {
                    DrmScanoutError::UnsupportedConnector {
                        card: card.path.display().to_string(),
                        connector: request.lease.connector.clone(),
                        reason: match request.mode.as_deref() {
                            Some(mode) => format!("requested mode {mode:?} is not advertised"),
                            None => "connector advertises no modes".to_owned(),
                        },
                    }
                })?;
            let mode_name = mode_name(&mode);
            let (width, height) = mode.size();
            let (width, height) = (u32::from(width), u32::from(height));

            let mut buffer = device
                .create_dumb_buffer((width, height), DrmFourcc::Xrgb8888, SCANOUT_BPP)
                .map_err(|error| DrmScanoutError::BufferFailed {
                    card: card.path.display().to_string(),
                    reason: error.to_string(),
                })?;
            let pitch = buffer.pitch();
            let buffer_handle = u32::from(buffer.handle());
            let checksum = {
                let mut mapping = device.map_dumb_buffer(&mut buffer).map_err(|error| {
                    DrmScanoutError::BufferFailed {
                        card: card.path.display().to_string(),
                        reason: error.to_string(),
                    }
                })?;
                fill_pattern(&mut mapping, width, height, pitch)
            };

            let framebuffer = device
                .add_framebuffer(&buffer, SCANOUT_DEPTH, SCANOUT_BPP)
                .map_err(|error| DrmScanoutError::FramebufferFailed {
                    card: card.path.display().to_string(),
                    reason: error.to_string(),
                })?;

            let plane_ids = lease.plane_ids().to_vec();
            let requested_plane = request.lease.plane_id();
            let mut fallback_reason = None;
            let mut forced_legacy = false;
            let (commit, plane_id, page_flipped) = if request.force_legacy {
                forced_legacy = true;
                match legacy_commit(
                    &device,
                    &card.path,
                    &request.lease.connector,
                    crtc_handle,
                    connector_handle,
                    framebuffer,
                    mode,
                    &mode_name,
                ) {
                    Ok(page_flipped) => (DrmScanoutCommit::Legacy, requested_plane, page_flipped),
                    Err(error) => {
                        let _ = device.destroy_framebuffer(framebuffer);
                        let _ = device.destroy_dumb_buffer(buffer);
                        return Err(error);
                    }
                }
            } else {
                match try_atomic_commit(
                    &device,
                    &card.path,
                    &request.lease.connector,
                    connector_handle,
                    connector_id,
                    crtc_handle,
                    crtc_id,
                    framebuffer,
                    mode,
                    &mode_name,
                    width,
                    height,
                    requested_plane,
                    &plane_ids,
                ) {
                    Ok(outcome) => (
                        DrmScanoutCommit::Atomic,
                        Some(outcome.plane_id),
                        outcome.page_flipped,
                    ),
                    Err(AtomicAttempt::Unsupported(reason)) => {
                        fallback_reason = Some(reason);
                        match legacy_commit(
                            &device,
                            &card.path,
                            &request.lease.connector,
                            crtc_handle,
                            connector_handle,
                            framebuffer,
                            mode,
                            &mode_name,
                        ) {
                            Ok(page_flipped) => {
                                (DrmScanoutCommit::Legacy, requested_plane, page_flipped)
                            }
                            Err(error) => {
                                let _ = device.destroy_framebuffer(framebuffer);
                                let _ = device.destroy_dumb_buffer(buffer);
                                return Err(error);
                            }
                        }
                    }
                    Err(AtomicAttempt::Failed(error)) => {
                        let _ = device.destroy_framebuffer(framebuffer);
                        let _ = device.destroy_dumb_buffer(buffer);
                        return Err(error);
                    }
                }
            };

            let mut status = DrmScanoutStatus {
                card: card.path.display().to_string(),
                connector: request.lease.connector.clone(),
                connector_id: Some(connector_id),
                crtc_id: Some(crtc_id),
                plane_id,
                commit,
                lessee_id: Some(lease.lessee_id()),
                lessee_fd: lease.fd(),
                mode: mode_name,
                width,
                height,
                pitch,
                framebuffer_id: Some(u32::from(framebuffer)),
                buffer_handle: Some(buffer_handle),
                pattern: SCANOUT_PATTERN.to_owned(),
                pattern_checksum: Some(checksum),
                page_flipped,
                verification: None,
                applied: true,
                released: false,
                notes: Vec::new(),
                error: None,
            };
            status.push_proof_note();
            status.push_note(DrmScanoutNote::new(
                DrmScanoutNoteKind::LesseeFd,
                format!(
                    "KMS operations use the lease's lessee fd {}; format={SCANOUT_FOURCC} \
                     bpp={SCANOUT_BPP} depth={SCANOUT_DEPTH} border={BORDER_PX}px \
                     diagonal={DIAGONAL_PX}px",
                    lease.fd().unwrap_or(-1)
                ),
            ));
            if let Some(plane_id) = plane_id {
                status.push_note(DrmScanoutNote::new(
                    DrmScanoutNoteKind::PlaneLeased,
                    format!(
                        "plane {plane_id} was validated against CRTC {crtc_id} and committed \
                         through {} KMS",
                        commit.as_str()
                    ),
                ));
            }
            if !card.is_virtual() {
                status.push_note(DrmScanoutNote::new(
                    DrmScanoutNoteKind::RealGpuNamed,
                    format!(
                        "operator explicitly named real card {} ({}); only the leased \
                         connector/CRTC/plane was programmed",
                        card.path.display(),
                        card.driver
                    ),
                ));
            }
            if forced_legacy {
                status.push_note(DrmScanoutNote::new(
                    DrmScanoutNoteKind::LegacyCommitForced,
                    "legacy set_crtc/page_flip was requested explicitly as a baseline",
                ));
            }
            if let Some(reason) = fallback_reason {
                status.push_note(DrmScanoutNote::new(
                    DrmScanoutNoteKind::LegacyCommitFallback,
                    format!(
                        "atomic KMS was unsupported ({reason}); the proof fell back to legacy \
                         set_crtc/page_flip"
                    ),
                ));
            }
            if !page_flipped {
                status.push_note(DrmScanoutNote::new(
                    DrmScanoutNoteKind::PageFlipUnavailable,
                    format!(
                        "page flip was rejected; the {} commit still programmed scanout",
                        commit.as_str()
                    ),
                ));
            }

            Ok(Box::new(RealDrmScanout {
                lease: Some(lease),
                card: card.path,
                connector_name: request.lease.connector.clone(),
                connector_id,
                crtc_id,
                plane_id,
                status,
                framebuffer: Some(framebuffer),
                buffer: Some(buffer),
                active: true,
            }))
        }
    }

    struct RealDrmScanout {
        lease: Option<Box<dyn DrmLeaseHandle>>,
        card: std::path::PathBuf,
        connector_name: String,
        connector_id: u32,
        crtc_id: u32,
        plane_id: Option<u32>,
        status: DrmScanoutStatus,
        framebuffer: Option<framebuffer::Handle>,
        buffer: Option<drm::control::dumbbuffer::DumbBuffer>,
        active: bool,
    }

    impl RealDrmScanout {
        fn status_error(&mut self, error: &DrmScanoutError) {
            self.status.mark_error(error);
        }
    }

    impl DrmScanoutHandle for RealDrmScanout {
        fn status(&self) -> &DrmScanoutStatus {
            &self.status
        }

        fn verify(&mut self) -> Result<DrmScanoutVerification, DrmScanoutError> {
            if !self.active {
                return Err(DrmScanoutError::Released(self.card.display().to_string()));
            }
            let Some(lease) = self.lease.as_ref() else {
                return Err(DrmScanoutError::Released(self.card.display().to_string()));
            };
            let Some(fd) = lease.as_fd() else {
                return Err(DrmScanoutError::Released(self.card.display().to_string()));
            };
            let device = LeaseDevice { fd };
            let Some(framebuffer) = self.framebuffer else {
                return Err(DrmScanoutError::Released(self.card.display().to_string()));
            };
            let (buffer_handle, size, pitch) = {
                let Some(buffer) = self.buffer.as_ref() else {
                    return Err(DrmScanoutError::Released(self.card.display().to_string()));
                };
                (u32::from(buffer.handle()), buffer.size(), buffer.pitch())
            };
            let connector_handle = connector_handle(self.connector_id, &self.card)?;
            let crtc_handle = crtc_handle(self.crtc_id, &self.card)?;

            let crtc_info = device
                .get_crtc(crtc_handle)
                .map_err(|error| map_io(&self.card, error))?;
            let connector_info = device
                .get_connector(connector_handle, false)
                .map_err(|error| map_io(&self.card, error))?;
            let framebuffer_info = device
                .get_framebuffer(framebuffer)
                .map_err(|error| map_io(&self.card, error))?;
            let connected_crtc = connector_info
                .current_encoder()
                .and_then(|encoder| device.get_encoder(encoder).ok())
                .and_then(|encoder| encoder.crtc())
                .map(u32::from);

            let (plane_id, plane_crtc, plane_framebuffer) = match self.plane_id {
                Some(id) => {
                    let handle = plane_handle(id, &self.card)?;
                    let info = device
                        .get_plane(handle)
                        .map_err(|error| map_io(&self.card, error))?;
                    (
                        Some(id),
                        info.crtc().map(u32::from),
                        info.framebuffer().map(u32::from),
                    )
                }
                None => (None, None, None),
            };

            let crtc_mode = crtc_info.mode().map(|mode| mode_name(&mode));
            let crtc_framebuffer = crtc_info.framebuffer().map(u32::from);
            let framebuffer_buffer = framebuffer_info.buffer().map(u32::from);
            let expected_checksum = self.status.pattern_checksum.unwrap_or(0);
            // Re-map the dumb buffer and checksum the bytes that are still
            // being scanned out. This is stronger than trusting the fill-time
            // checksum stored in status.
            let actual_checksum = {
                let Some(buffer) = self.buffer.as_mut() else {
                    return Err(DrmScanoutError::Released(self.card.display().to_string()));
                };
                let mapping = device.map_dumb_buffer(buffer).map_err(|error| {
                    DrmScanoutError::BufferFailed {
                        card: self.card.display().to_string(),
                        reason: format!("re-mapping the dumb buffer for verification: {error}"),
                    }
                })?;
                pattern_checksum(&mapping)
            };

            let mut mismatch = None;
            if crtc_framebuffer != Some(u32::from(framebuffer)) {
                mismatch = Some(format!(
                    "CRTC {} reports framebuffer {:?}, expected {}",
                    self.crtc_id,
                    crtc_framebuffer,
                    u32::from(framebuffer)
                ));
            } else if crtc_mode.as_deref() != Some(self.status.mode.as_str()) {
                mismatch = Some(format!(
                    "CRTC {} reports mode {:?}, expected {:?}",
                    self.crtc_id, crtc_mode, self.status.mode
                ));
            } else if framebuffer_info.size() != size {
                mismatch = Some(format!(
                    "framebuffer size {:?} does not match buffer size {:?}",
                    framebuffer_info.size(),
                    size
                ));
            } else if framebuffer_info.pitch() != pitch {
                mismatch = Some(format!(
                    "framebuffer pitch {} does not match buffer pitch {}",
                    framebuffer_info.pitch(),
                    pitch
                ));
            } else if framebuffer_buffer.is_none() {
                mismatch = Some(format!(
                    "framebuffer {} reports no backing buffer handle",
                    u32::from(framebuffer)
                ));
            } else if actual_checksum != expected_checksum {
                mismatch = Some(format!(
                    "dumb-buffer checksum {actual_checksum} does not match the filled pattern \
                     {expected_checksum}"
                ));
            } else if connected_crtc != Some(self.crtc_id) {
                mismatch = Some(format!(
                    "connector encoder binds CRTC {:?}, expected {}",
                    connected_crtc, self.crtc_id
                ));
            } else if let Some(plane_id) = plane_id {
                if plane_crtc != Some(self.crtc_id) {
                    mismatch = Some(format!(
                        "plane {plane_id} reports CRTC {:?}, expected {}",
                        plane_crtc, self.crtc_id
                    ));
                } else if plane_framebuffer != Some(u32::from(framebuffer)) {
                    mismatch = Some(format!(
                        "plane {plane_id} reports framebuffer {:?}, expected {}",
                        plane_framebuffer,
                        u32::from(framebuffer)
                    ));
                }
            }

            let verification = DrmScanoutVerification {
                crtc_id: self.crtc_id,
                crtc_mode,
                crtc_framebuffer,
                crtc_position: crtc_info.position(),
                connector_state: drm_status_name(connector_info.state()),
                connector_crtc: connected_crtc,
                plane_id,
                plane_crtc,
                plane_framebuffer,
                framebuffer_id: u32::from(framebuffer),
                framebuffer_size: framebuffer_info.size(),
                framebuffer_pitch: framebuffer_info.pitch(),
                framebuffer_bpp: framebuffer_info.bpp(),
                framebuffer_depth: framebuffer_info.depth(),
                framebuffer_buffer_handle: framebuffer_buffer,
                buffer_size: size,
                buffer_pitch: pitch,
                buffer_handle,
                pattern_checksum: actual_checksum,
                ok: mismatch.is_none(),
                mismatch: mismatch.clone(),
            };
            self.status.verification = Some(verification.clone());
            // GETFB returns a *new* GEM handle in this file for the framebuffer
            // object (drm_gem_fb_create_handle). Close it so repeated verify
            // calls do not accumulate handles.
            if let Some(handle) = framebuffer_info.buffer() {
                let _ = device.close_buffer(handle);
            }
            if let Some(mismatch) = mismatch {
                let error = DrmScanoutError::VerificationFailed {
                    card: self.card.display().to_string(),
                    connector: self.connector_name.clone(),
                    message: mismatch,
                };
                self.status_error(&error);
                return Err(error);
            }
            Ok(verification)
        }

        fn teardown(&mut self) -> Result<(), DrmScanoutError> {
            if !self.active && self.lease.is_none() {
                return Ok(());
            }
            let card = self.card.clone();

            let mut first_error: Option<DrmScanoutError> = None;
            if let Some(lease) = self.lease.as_ref() {
                if let Some(fd) = lease.as_fd() {
                    let device = LeaseDevice { fd };
                    let clear = match crtc_handle(self.crtc_id, &card) {
                        Ok(crtc_handle) => {
                            if self.status.commit == DrmScanoutCommit::Atomic {
                                if let Some(plane_id) = self.plane_id {
                                    match try_atomic_disable(
                                        &device,
                                        &card,
                                        self.connector_id,
                                        self.crtc_id,
                                        plane_id,
                                    ) {
                                        Ok(()) => Ok(()),
                                        Err(AtomicAttempt::Unsupported(_)) => legacy_clear(
                                            &device,
                                            &card,
                                            &self.connector_name,
                                            crtc_handle,
                                            &self.status.mode,
                                        ),
                                        Err(AtomicAttempt::Failed(error)) => Err(error),
                                    }
                                } else {
                                    legacy_clear(
                                        &device,
                                        &card,
                                        &self.connector_name,
                                        crtc_handle,
                                        &self.status.mode,
                                    )
                                }
                            } else {
                                legacy_clear(
                                    &device,
                                    &card,
                                    &self.connector_name,
                                    crtc_handle,
                                    &self.status.mode,
                                )
                            }
                        }
                        Err(error) => Err(error),
                    };
                    if let Err(error) = clear {
                        first_error = Some(error);
                    }
                    if let Some(framebuffer) = self.framebuffer {
                        if let Err(error) = device.destroy_framebuffer(framebuffer) {
                            first_error.get_or_insert(DrmScanoutError::FramebufferFailed {
                                card: card.display().to_string(),
                                reason: format!("destroying framebuffer {framebuffer:?}: {error}"),
                            });
                        } else {
                            self.framebuffer = None;
                        }
                    }
                    if let Some(buffer) = self.buffer {
                        if let Err(error) = device.destroy_dumb_buffer(buffer) {
                            first_error.get_or_insert(DrmScanoutError::BufferFailed {
                                card: card.display().to_string(),
                                reason: format!("destroying dumb buffer: {error}"),
                            });
                        } else {
                            self.buffer = None;
                        }
                    }
                }
            }

            // Revoke even when a clear/destroy step failed, so the lease and
            // lessee fd cannot leak. A second teardown then only retries the
            // object cleanup that did not complete.
            if let Some(mut lease) = self.lease.take() {
                if let Err(error) = lease.revoke() {
                    self.lease = Some(lease);
                    first_error.get_or_insert(error.into());
                }
            }

            match first_error {
                Some(error) => {
                    self.status_error(&error);
                    Err(error)
                }
                None => {
                    self.active = false;
                    self.lease = None;
                    self.status.mark_released();
                    Ok(())
                }
            }
        }

        fn is_active(&self) -> bool {
            self.active
                && self.lease.as_ref().is_some_and(|lease| lease.is_active())
                && self.framebuffer.is_some()
        }
    }

    impl Drop for RealDrmScanout {
        fn drop(&mut self) {
            let _ = self.teardown();
        }
    }

    struct LeaseDevice<'a> {
        fd: BorrowedFd<'a>,
    }

    impl AsFd for LeaseDevice<'_> {
        fn as_fd(&self) -> BorrowedFd<'_> {
            self.fd
        }
    }

    impl drm::Device for LeaseDevice<'_> {}
    impl ControlDevice for LeaseDevice<'_> {}

    fn connector_handle(id: u32, card: &Path) -> Result<connector::Handle, DrmScanoutError> {
        let raw = NonZeroU32::new(id).ok_or_else(|| DrmScanoutError::UnsupportedConnector {
            card: card.display().to_string(),
            connector: id.to_string(),
            reason: "connector object id is zero".to_owned(),
        })?;
        Ok(connector::Handle::from(RawResourceHandle::from(raw)))
    }

    fn crtc_handle(id: u32, card: &Path) -> Result<crtc::Handle, DrmScanoutError> {
        let raw = NonZeroU32::new(id).ok_or_else(|| DrmScanoutError::Io {
            path: card.display().to_string(),
            source: std::io::Error::other("CRTC object id is zero"),
        })?;
        Ok(crtc::Handle::from(RawResourceHandle::from(raw)))
    }

    fn plane_handle(id: u32, card: &Path) -> Result<plane::Handle, DrmScanoutError> {
        let raw = NonZeroU32::new(id).ok_or_else(|| DrmScanoutError::UnsupportedPlane {
            card: card.display().to_string(),
            crtc: 0,
            reason: "plane object id is zero".to_owned(),
        })?;
        Ok(plane::Handle::from(RawResourceHandle::from(raw)))
    }

    /// Bounded property name lookup for one KMS object.
    struct AtomicProperties {
        values: BTreeMap<String, (property::Handle, property::RawValue)>,
    }

    impl AtomicProperties {
        fn read<T: ResourceHandle + Copy>(
            device: &LeaseDevice<'_>,
            handle: T,
            card: &Path,
            object: &str,
            id: u32,
        ) -> Result<Self, DrmScanoutError> {
            let set = device
                .get_properties(handle)
                .map_err(|error| map_io(card, error))?;
            let (prop_ids, prop_values) = set.as_props_and_values();
            if prop_ids.len() > MAX_ATOMIC_PROPERTIES {
                return Err(DrmScanoutError::PropertyLimitExceeded {
                    card: card.display().to_string(),
                    object: object.to_owned(),
                    id,
                    limit: MAX_ATOMIC_PROPERTIES,
                });
            }
            let mut values = BTreeMap::new();
            for (prop, value) in prop_ids.iter().zip(prop_values.iter()) {
                let info = device
                    .get_property(*prop)
                    .map_err(|error| map_io(card, error))?;
                let name = info.name().to_string_lossy().into_owned();
                values.insert(name, (*prop, *value));
            }
            Ok(Self { values })
        }

        fn require(
            &self,
            card: &Path,
            object: &str,
            id: u32,
            property: &str,
        ) -> Result<property::Handle, DrmScanoutError> {
            self.values
                .get(property)
                .map(|(handle, _)| *handle)
                .ok_or_else(|| DrmScanoutError::MissingAtomicProperty {
                    card: card.display().to_string(),
                    object: object.to_owned(),
                    id,
                    property: property.to_owned(),
                })
        }

        fn enum_name(
            &self,
            device: &LeaseDevice<'_>,
            card: &Path,
            property: &str,
        ) -> Result<Option<String>, DrmScanoutError> {
            let Some((handle, raw)) = self.values.get(property) else {
                return Ok(None);
            };
            let info = device
                .get_property(*handle)
                .map_err(|error| map_io(card, error))?;
            let value = info.value_type().convert_value(*raw);
            Ok(value
                .as_enum()
                .map(|value| value.name().to_string_lossy().into_owned()))
        }
    }

    struct AtomicOutcome {
        plane_id: u32,
        page_flipped: bool,
    }

    enum AtomicAttempt {
        Unsupported(String),
        Failed(DrmScanoutError),
    }

    fn is_unsupported_error(error: &std::io::Error) -> bool {
        matches!(
            error.raw_os_error(),
            Some(libc::EINVAL) | Some(libc::EOPNOTSUPP) | Some(libc::ENOSYS) | Some(libc::ENOTTY)
        )
    }

    fn enable_atomic(device: &LeaseDevice<'_>, card: &Path) -> Result<(), AtomicAttempt> {
        let _ = device.set_client_capability(ClientCapability::UniversalPlanes, true);
        match device.set_client_capability(ClientCapability::Atomic, true) {
            Ok(()) => Ok(()),
            Err(error) if is_unsupported_error(&error) => {
                Err(AtomicAttempt::Unsupported(error.to_string()))
            }
            Err(error) => Err(AtomicAttempt::Failed(map_io(card, error))),
        }
    }

    /// Choose the explicit plane or the best compatible primary plane reported
    /// by the lease. Format and CRTC compatibility are checked before any
    /// property is written.
    fn choose_plane(
        device: &LeaseDevice<'_>,
        card: &Path,
        crtc_handle: crtc::Handle,
        crtc_id: u32,
        requested: Option<u32>,
        lease_plane_ids: &[u32],
    ) -> Result<plane::Handle, DrmScanoutError> {
        let mut candidates: Vec<u32> = if let Some(id) = requested {
            vec![id]
        } else if !lease_plane_ids.is_empty() {
            lease_plane_ids.to_vec()
        } else {
            device
                .plane_handles()
                .map_err(|error| map_io(card, error))?
                .into_iter()
                .map(u32::from)
                .collect()
        };
        candidates.sort_unstable();
        candidates.dedup();
        if candidates.is_empty() {
            return Err(DrmScanoutError::UnsupportedPlane {
                card: card.display().to_string(),
                crtc: crtc_id,
                reason: "the lease reports no plane and the card exposes none".to_owned(),
            });
        }

        let handles = device
            .resource_handles()
            .map_err(|error| map_io(card, error))?;
        let format = DrmFourcc::Xrgb8888 as u32;
        let mut best: Option<(u8, plane::Handle)> = None;
        for id in candidates {
            let handle = plane_handle(id, card)?;
            let info = match device.get_plane(handle) {
                Ok(info) => info,
                Err(error) => {
                    if requested == Some(id) {
                        return Err(DrmScanoutError::UnsupportedPlane {
                            card: card.display().to_string(),
                            crtc: crtc_id,
                            reason: format!("GET_PLANE failed: {error}"),
                        });
                    }
                    continue;
                }
            };
            let compatible = handles
                .filter_crtcs(info.possible_crtcs())
                .contains(&crtc_handle);
            if !compatible {
                if requested == Some(id) {
                    return Err(DrmScanoutError::UnsupportedPlane {
                        card: card.display().to_string(),
                        crtc: crtc_id,
                        reason: format!("plane {id} cannot drive CRTC {crtc_id}"),
                    });
                }
                continue;
            }
            if !info.formats().contains(&format) {
                if requested == Some(id) {
                    return Err(DrmScanoutError::InvalidFormat {
                        card: card.display().to_string(),
                        plane: id,
                        format: SCANOUT_FOURCC.to_owned(),
                        reason: "the plane advertises a different format set".to_owned(),
                    });
                }
                continue;
            }
            if requested == Some(id) {
                return Ok(handle);
            }
            let rank = match AtomicProperties::read(device, handle, card, "plane", id)
                .ok()
                .and_then(|properties| properties.enum_name(device, card, "type").ok())
                .flatten()
                .as_deref()
            {
                Some("Primary") => 0,
                Some("Cursor") => 2,
                _ => 1,
            };
            if best.as_ref().is_none_or(|(best_rank, _)| rank < *best_rank) {
                best = Some((rank, handle));
            }
        }
        best.map(|(_, handle)| handle)
            .ok_or_else(|| DrmScanoutError::UnsupportedPlane {
                card: card.display().to_string(),
                crtc: crtc_id,
                reason: "no leaseable plane advertises the XRGB8888 format for this CRTC"
                    .to_owned(),
            })
    }

    #[allow(clippy::too_many_arguments)]
    fn try_atomic_commit(
        device: &LeaseDevice<'_>,
        card: &Path,
        connector_name: &str,
        connector_handle: connector::Handle,
        connector_id: u32,
        crtc_handle: crtc::Handle,
        crtc_id: u32,
        framebuffer: framebuffer::Handle,
        mode: Mode,
        mode_name: &str,
        width: u32,
        height: u32,
        requested_plane: Option<u32>,
        lease_plane_ids: &[u32],
    ) -> Result<AtomicOutcome, AtomicAttempt> {
        enable_atomic(device, card)?;
        let plane_handle = choose_plane(
            device,
            card,
            crtc_handle,
            crtc_id,
            requested_plane,
            lease_plane_ids,
        )
        .map_err(AtomicAttempt::Failed)?;
        let plane_id = u32::from(plane_handle);

        let connector_props =
            AtomicProperties::read(device, connector_handle, card, "connector", connector_id)
                .map_err(AtomicAttempt::Failed)?;
        let crtc_props = AtomicProperties::read(device, crtc_handle, card, "crtc", crtc_id)
            .map_err(AtomicAttempt::Failed)?;
        let plane_props = AtomicProperties::read(device, plane_handle, card, "plane", plane_id)
            .map_err(AtomicAttempt::Failed)?;

        let blob = device
            .create_property_blob(&mode)
            .map_err(|error| AtomicAttempt::Failed(map_io(card, error)))?;
        let blob_id = match blob {
            property::Value::Blob(id) => id,
            _ => {
                return Err(AtomicAttempt::Failed(DrmScanoutError::Io {
                    path: card.display().to_string(),
                    source: std::io::Error::other("MODE_ID property blob was not created"),
                }))
            }
        };

        let mut request = AtomicModeReq::new();
        let connector_raw = RawResourceHandle::from(connector_handle);
        let crtc_raw = RawResourceHandle::from(crtc_handle);
        let plane_raw = RawResourceHandle::from(plane_handle);
        let mode = (|| -> Result<(), DrmScanoutError> {
            request.add_raw_property(
                connector_raw,
                connector_props.require(card, "connector", connector_id, "CRTC_ID")?,
                u64::from(crtc_id),
            );
            request.add_raw_property(
                crtc_raw,
                crtc_props.require(card, "crtc", crtc_id, "ACTIVE")?,
                1,
            );
            request.add_raw_property(
                crtc_raw,
                crtc_props.require(card, "crtc", crtc_id, "MODE_ID")?,
                blob_id,
            );
            request.add_raw_property(
                plane_raw,
                plane_props.require(card, "plane", plane_id, "FB_ID")?,
                u64::from(u32::from(framebuffer)),
            );
            request.add_raw_property(
                plane_raw,
                plane_props.require(card, "plane", plane_id, "CRTC_ID")?,
                u64::from(crtc_id),
            );
            for (name, value) in [
                ("SRC_X", 0),
                ("SRC_Y", 0),
                ("SRC_W", u64::from(width) << 16),
                ("SRC_H", u64::from(height) << 16),
                ("CRTC_X", 0),
                ("CRTC_Y", 0),
                ("CRTC_W", u64::from(width)),
                ("CRTC_H", u64::from(height)),
            ] {
                request.add_raw_property(
                    plane_raw,
                    plane_props.require(card, "plane", plane_id, name)?,
                    value,
                );
            }
            Ok(())
        })();
        if let Err(error) = mode {
            let _ = device.destroy_property_blob(blob_id);
            return Err(AtomicAttempt::Failed(error));
        }

        let result = device.atomic_commit(
            AtomicCommitFlags::ALLOW_MODESET | AtomicCommitFlags::PAGE_FLIP_EVENT,
            request,
        );
        let _ = device.destroy_property_blob(blob_id);
        match result {
            Ok(()) => Ok(AtomicOutcome {
                plane_id,
                page_flipped: true,
            }),
            Err(error)
                if matches!(
                    error.raw_os_error(),
                    Some(libc::EOPNOTSUPP) | Some(libc::ENOSYS) | Some(libc::ENOTTY)
                ) =>
            {
                Err(AtomicAttempt::Unsupported(error.to_string()))
            }
            Err(error) => Err(AtomicAttempt::Failed(DrmScanoutError::ModeFailed {
                card: card.display().to_string(),
                connector: connector_name.to_owned(),
                mode: mode_name.to_owned(),
                reason: format!("atomic plane/CRTC commit: {error}"),
            })),
        }
    }

    fn try_atomic_disable(
        device: &LeaseDevice<'_>,
        card: &Path,
        connector_id: u32,
        crtc_id: u32,
        plane_id: u32,
    ) -> Result<(), AtomicAttempt> {
        enable_atomic(device, card)?;
        let connector = connector_handle(connector_id, card).map_err(AtomicAttempt::Failed)?;
        let crtc = crtc_handle(crtc_id, card).map_err(AtomicAttempt::Failed)?;
        let plane = plane_handle(plane_id, card).map_err(AtomicAttempt::Failed)?;
        let connector_props =
            AtomicProperties::read(device, connector, card, "connector", connector_id)
                .map_err(AtomicAttempt::Failed)?;
        let crtc_props = AtomicProperties::read(device, crtc, card, "crtc", crtc_id)
            .map_err(AtomicAttempt::Failed)?;
        let plane_props = AtomicProperties::read(device, plane, card, "plane", plane_id)
            .map_err(AtomicAttempt::Failed)?;

        let mut request = AtomicModeReq::new();
        let connector_raw = RawResourceHandle::from(connector);
        let crtc_raw = RawResourceHandle::from(crtc);
        let plane_raw = RawResourceHandle::from(plane);
        let build = (|| -> Result<(), DrmScanoutError> {
            request.add_raw_property(
                connector_raw,
                connector_props.require(card, "connector", connector_id, "CRTC_ID")?,
                0,
            );
            request.add_raw_property(
                crtc_raw,
                crtc_props.require(card, "crtc", crtc_id, "ACTIVE")?,
                0,
            );
            request.add_raw_property(
                crtc_raw,
                crtc_props.require(card, "crtc", crtc_id, "MODE_ID")?,
                0,
            );
            request.add_raw_property(
                plane_raw,
                plane_props.require(card, "plane", plane_id, "FB_ID")?,
                0,
            );
            request.add_raw_property(
                plane_raw,
                plane_props.require(card, "plane", plane_id, "CRTC_ID")?,
                0,
            );
            Ok(())
        })();
        if let Err(error) = build {
            return Err(AtomicAttempt::Failed(error));
        }
        match device.atomic_commit(AtomicCommitFlags::ALLOW_MODESET, request) {
            Ok(()) => Ok(()),
            Err(error)
                if matches!(
                    error.raw_os_error(),
                    Some(libc::EOPNOTSUPP) | Some(libc::ENOSYS) | Some(libc::ENOTTY)
                ) =>
            {
                Err(AtomicAttempt::Unsupported(error.to_string()))
            }
            Err(error) => Err(AtomicAttempt::Failed(DrmScanoutError::ModeFailed {
                card: card.display().to_string(),
                connector: connector_id.to_string(),
                mode: "disable".to_owned(),
                reason: format!("atomic plane/CRTC disable: {error}"),
            })),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn legacy_commit(
        device: &LeaseDevice<'_>,
        card: &Path,
        connector_name: &str,
        crtc_handle: crtc::Handle,
        connector_handle: connector::Handle,
        framebuffer: framebuffer::Handle,
        mode: Mode,
        mode_name: &str,
    ) -> Result<bool, DrmScanoutError> {
        device
            .set_crtc(
                crtc_handle,
                Some(framebuffer),
                (0, 0),
                &[connector_handle],
                Some(mode),
            )
            .map_err(|error| DrmScanoutError::ModeFailed {
                card: card.display().to_string(),
                connector: connector_name.to_owned(),
                mode: mode_name.to_owned(),
                reason: error.to_string(),
            })?;
        Ok(
            match device.page_flip(crtc_handle, framebuffer, PageFlipFlags::empty(), None) {
                Ok(()) => true,
                Err(error) => {
                    // The CRTC commit above already programmed the mode. A
                    // flip-less virtual driver is still a successful commit,
                    // but the limitation is recorded honestly.
                    eprintln!(
                        "notice: page flip on {}:{} failed ({error}); set_crtc commit remains",
                        card.display(),
                        connector_name
                    );
                    false
                }
            },
        )
    }

    fn legacy_clear(
        device: &LeaseDevice<'_>,
        card: &Path,
        connector_name: &str,
        crtc_handle: crtc::Handle,
        mode: &str,
    ) -> Result<(), DrmScanoutError> {
        device
            .set_crtc(crtc_handle, None, (0, 0), &[], None)
            .map_err(|error| DrmScanoutError::ModeFailed {
                card: card.display().to_string(),
                connector: connector_name.to_owned(),
                mode: mode.to_owned(),
                reason: format!("clearing CRTC: {error}"),
            })
    }

    fn choose_mode(modes: &[Mode], requested: Option<&str>) -> Option<Mode> {
        match requested {
            Some(requested) => modes
                .iter()
                .find(|mode| {
                    mode.name()
                        .to_string_lossy()
                        .eq_ignore_ascii_case(requested.trim())
                })
                .copied(),
            None => modes.first().copied(),
        }
    }

    fn mode_name(mode: &Mode) -> String {
        mode.name().to_string_lossy().into_owned()
    }

    fn map_io(card: &Path, error: std::io::Error) -> DrmScanoutError {
        match error.raw_os_error() {
            Some(libc::EACCES) | Some(libc::EPERM) => DrmScanoutError::NoPermission {
                path: card.display().to_string(),
                source: error,
            },
            Some(libc::EBUSY) => DrmScanoutError::CapabilityRequired {
                card: card.display().to_string(),
                reason: error.to_string(),
            },
            _ => DrmScanoutError::Io {
                path: card.display().to_string(),
                source: error,
            },
        }
    }
}

#[cfg(target_os = "linux")]
pub use real::RealDrmScanoutBackend;

fn is_false(value: &bool) -> bool {
    !*value
}

/// Fill `bytes` with the documented XRGB8888 pattern and return its FNV-1a
/// checksum. Rows are written with the buffer pitch, not `width * 4`.
pub fn fill_pattern(bytes: &mut [u8], width: u32, height: u32, pitch: u32) -> u32 {
    let width = width as usize;
    let height = height as usize;
    let pitch = pitch as usize;
    for y in 0..height {
        let row_start = y * pitch;
        let row_end = row_start + width * 4;
        if row_end > bytes.len() {
            break;
        }
        for x in 0..width {
            let pixel = pattern_pixel(x as u32, y as u32, width as u32, height as u32);
            let offset = row_start + x * 4;
            bytes[offset..offset + 4].copy_from_slice(&pixel.to_le_bytes());
        }
    }
    pattern_checksum(bytes)
}

/// FNV-1a 32-bit checksum used for the pattern evidence.
pub fn pattern_checksum(bytes: &[u8]) -> u32 {
    let mut hash = 0x811c_9dc5u32;
    for byte in bytes {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

fn pattern_pixel(x: u32, y: u32, width: u32, height: u32) -> u32 {
    if x < BORDER_PX || y < BORDER_PX || x + BORDER_PX >= width || y + BORDER_PX >= height {
        return 0x00ff_ffff;
    }
    // A thick main diagonal makes orientation and a mis-sized mode visible.
    let line = u64::from(x) * u64::from(height);
    let diagonal = u64::from(y) * u64::from(width);
    let thickness = u64::from(DIAGONAL_PX) * u64::from(width.max(height));
    if line.abs_diff(diagonal) <= thickness {
        return 0x0000_0000;
    }
    let left = x < width / 2;
    let top = y < height / 2;
    match (top, left) {
        (true, true) => 0x00ff_0000,
        (true, false) => 0x0000_ff00,
        (false, true) => 0x0000_00ff,
        (false, false) => 0x00ff_ffff,
    }
}
