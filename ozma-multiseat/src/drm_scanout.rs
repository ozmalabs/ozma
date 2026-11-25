// SPDX-License-Identifier: AGPL-3.0-only
//! Opt-in KMS scanout proof over a DRM lease.
//!
//! A lease alone only hands object IDs to a lessee. This module goes one step
//! further and proves the leased objects are actually usable for scanout:
//!
//! 1. acquire a lease for one connector, one CRTC, and optionally one validated
//!    plane;
//! 2. choose one of the connector's advertised modes;
//! 3. allocate one dumb buffer (or two/three rotating buffers for a frame
//!    loop), fill a known pattern, and add a framebuffer for each;
//! 4. program the connector/CRTC/plane with an atomic property commit
//!    (`set_plane` + `set_crtc` state) and a page-flip event, falling back to
//!    legacy `set_crtc`/`page_flip` only when atomic KMS is unsupported;
//! 5. optionally run a bounded frame loop (`2..=300` frames) that draws a
//!    deterministic moving damage rectangle into the next rotating buffer,
//!    commits it, and waits for its page-flip completion event with a bounded
//!    timeout;
//! 6. re-read CRTC/plane/connector/framebuffer state and compare it with what
//!    was requested;
//! 7. clear the plane/CRTC, destroy every framebuffer and dumb buffer, and
//!    revoke the lease.
//!
//! This is a **scanout proof**, not a compositor. It renders generated pixels
//! directly to one leased output. Even with a frame loop it does not accept
//! client surfaces, composite windows, perform real damage tracking, provide a
//! Wayland/X server, or isolate the host desktop. Status always carries a
//! [`DrmScanoutNoteKind::ScanoutProofOnly`] note saying exactly that.
//!
//! Card selection is conservative: `auto`/`vkms` only picks a sysfs `vkms`
//! card, and a real GPU output is only attempted when the operator names its
//! `/dev/dri/cardN` explicitly. The kernel ABI is provided by the `drm` crate;
//! this module adds no `unsafe` code.

use std::io;
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::drm_lease::{DrmCard, DrmCardSelector, DrmLeaseError, DrmLeaseRequest};

/// Stable name of the generated scanout pattern.
pub const SCANOUT_PATTERN: &str = "quadrants-border-diagonal-xrgb8888";
/// Stable name of the deterministic moving damage rectangle used by the frame
/// loop over the base pattern.
pub const SCANOUT_DAMAGE_PATTERN: &str = "moving-damage-rect-xrgb8888";
/// Default number of frames: the original single static proof frame.
pub const SCANOUT_DEFAULT_FRAMES: u32 = 1;
/// Upper bound on the opt-in frame loop. A larger request is a typed failure,
/// never an unbounded loop.
pub const SCANOUT_MAX_FRAMES: u32 = 300;
/// Default bounded wait for one page-flip completion event.
pub const SCANOUT_FLIP_TIMEOUT_MS: u64 = 1000;
/// Default bounded wait for one writeback capture job.
pub const SCANOUT_WRITEBACK_TIMEOUT_MS: u64 = 1000;
/// Upper bound on a writeback capture wait. A larger request is a typed
/// validation failure rather than an unbounded wait.
pub const SCANOUT_MAX_WRITEBACK_TIMEOUT_MS: u64 = 60_000;
/// Default number of retries for a transient writeback commit failure.
pub const SCANOUT_WRITEBACK_RETRIES: u32 = 1;
/// Upper bound on writeback commit retries.
pub const SCANOUT_MAX_WRITEBACK_RETRIES: u32 = 3;
/// Upper bound on a verification capture buffer. The scanout buffer is bounded
/// by the connector mode; this keeps verification from allocating an
/// unexpectedly large extra buffer.
pub const SCANOUT_MAX_VERIFY_BYTES: u64 = 64 * 1024 * 1024;
/// Upper bound on checksum samples retained in status. The frame loop itself is
/// separately bounded by [`SCANOUT_MAX_FRAMES`].
pub const SCANOUT_MAX_CAPTURE_SAMPLES: usize = SCANOUT_MAX_FRAMES as usize;
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

/// Opt-in pixel verification mechanism for the KMS proof.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DrmScanoutVerifyMode {
    /// No pixel capture. The proof still re-reads KMS state in `verify`.
    #[default]
    None,
    /// Capture the composed CRTC output through a leased writeback connector
    /// and compare a deterministic checksum for each captured frame.
    Writeback,
    /// Capture CRTC CRC values. This kernel/vkms supports CRC, but the safe
    /// `drm` crate surface used by this crate has no CRC ioctl wrapper and the
    /// crate forbids unsafe code, so the real backend reports the typed
    /// `kms-verification-unavailable` blocker instead of pretending to capture.
    Crc,
}

impl DrmScanoutVerifyMode {
    /// Stable wire/status name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Writeback => "writeback",
            Self::Crc => "crc",
        }
    }

    /// Whether this mode asks for pixel capture.
    pub fn is_capture(self) -> bool {
        matches!(self, Self::Writeback | Self::Crc)
    }
}

impl FromStr for DrmScanoutVerifyMode {
    type Err = DrmScanoutError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "none" => Ok(Self::None),
            "writeback" => Ok(Self::Writeback),
            "crc" => Ok(Self::Crc),
            other => Err(DrmScanoutError::InvalidRequest(
                other.to_owned(),
                "verification mode must be writeback, crc, or none".to_owned(),
            )),
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
    /// Total frames to present. `1` keeps the original single static proof;
    /// `2..=300` runs a bounded page-flip loop over rotating dumb buffers.
    #[serde(default = "default_frames", skip_serializing_if = "is_single_frame")]
    pub frames: u32,
    /// Optional pacing interval between completed frames.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_ms: Option<u64>,
    /// Bounded wait for one page-flip completion event.
    #[serde(
        default = "default_flip_timeout_ms",
        skip_serializing_if = "is_default_flip_timeout"
    )]
    pub flip_timeout_ms: u64,
    /// Opt-in pixel verification mechanism. Defaults to `none`; writeback
    /// capture is the implemented mechanism on this kernel.
    #[serde(default, skip_serializing_if = "is_verify_none")]
    pub verify: DrmScanoutVerifyMode,
    /// Bounded wait for one writeback capture job.
    #[serde(
        default = "default_writeback_timeout_ms",
        skip_serializing_if = "is_default_writeback_timeout"
    )]
    pub writeback_timeout_ms: u64,
    /// Bounded retries for a transient writeback commit failure (EBUSY/EAGAIN).
    #[serde(
        default = "default_writeback_retries",
        skip_serializing_if = "is_default_writeback_retries"
    )]
    pub writeback_retries: u32,
}

impl DrmScanoutRequest {
    /// Build a request with automatic mode selection and one frame.
    pub fn new(lease: DrmLeaseRequest) -> Self {
        Self {
            lease,
            mode: None,
            force_legacy: false,
            frames: SCANOUT_DEFAULT_FRAMES,
            interval_ms: None,
            flip_timeout_ms: SCANOUT_FLIP_TIMEOUT_MS,
            verify: DrmScanoutVerifyMode::None,
            writeback_timeout_ms: SCANOUT_WRITEBACK_TIMEOUT_MS,
            writeback_retries: SCANOUT_WRITEBACK_RETRIES,
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

    /// Set the total number of frames (validated by [`Self::validate`]).
    pub fn with_frames(mut self, frames: u32) -> Self {
        self.frames = frames;
        self
    }

    /// Pace completed frames at approximately `interval_ms` milliseconds.
    pub fn with_interval_ms(mut self, interval_ms: u64) -> Self {
        self.interval_ms = Some(interval_ms);
        self
    }

    /// Bounded per-flip event wait.
    pub fn with_flip_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.flip_timeout_ms = timeout_ms;
        self
    }

    /// Request an opt-in pixel verification mechanism.
    pub fn with_verify(mut self, verify: DrmScanoutVerifyMode) -> Self {
        self.verify = verify;
        self
    }

    /// Bounded wait for one writeback capture job.
    pub fn with_writeback_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.writeback_timeout_ms = timeout_ms;
        self
    }

    /// Bounded retries for a transient writeback commit failure.
    pub fn with_writeback_retries(mut self, retries: u32) -> Self {
        self.writeback_retries = retries;
        self
    }

    /// Validate frame count and timeout bounds before touching a device.
    pub fn validate(&self) -> Result<(), DrmScanoutError> {
        if self.frames == 0 || self.frames > SCANOUT_MAX_FRAMES {
            return Err(DrmScanoutError::InvalidFrameCount {
                frames: self.frames,
                min: 1,
                max: SCANOUT_MAX_FRAMES,
            });
        }
        if self.flip_timeout_ms == 0 {
            return Err(DrmScanoutError::InvalidRequest(
                self.lease.connector.clone(),
                "flip timeout must be greater than zero".to_owned(),
            ));
        }
        if self.writeback_timeout_ms == 0
            || self.writeback_timeout_ms > SCANOUT_MAX_WRITEBACK_TIMEOUT_MS
        {
            return Err(DrmScanoutError::InvalidRequest(
                self.lease.connector.clone(),
                format!("writeback timeout must be 1..={SCANOUT_MAX_WRITEBACK_TIMEOUT_MS}ms"),
            ));
        }
        if self.writeback_retries > SCANOUT_MAX_WRITEBACK_RETRIES {
            return Err(DrmScanoutError::InvalidRequest(
                self.lease.connector.clone(),
                format!("writeback retries must be 0..={SCANOUT_MAX_WRITEBACK_RETRIES}"),
            ));
        }
        Ok(())
    }

    /// Frame-loop plan implied by this request.
    pub fn frame_plan(&self) -> DrmScanoutFramePlan {
        DrmScanoutFramePlan {
            frames: self.frames,
            interval: self.interval_ms.map(Duration::from_millis),
            flip_timeout: Duration::from_millis(self.flip_timeout_ms),
        }
    }
}

impl FromStr for DrmScanoutRequest {
    type Err = DrmLeaseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(Self::new(value.parse::<DrmLeaseRequest>()?))
    }
}

/// Bounded frame-loop parameters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DrmScanoutFramePlan {
    /// Total frames including the initial modeset frame.
    pub frames: u32,
    /// Optional pacing interval between completed frames.
    pub interval: Option<Duration>,
    /// Bounded wait for one page-flip completion event.
    pub flip_timeout: Duration,
}

/// Frame-loop accounting.
///
/// `submitted` counts commits sent to KMS; `completed` counts flip events that
/// actually arrived. A frame is never counted as completed just because its
/// commit ioctl returned. `dropped` is submitted minus completed for the frames
/// attempted so far; `missed` is the subset that timed out waiting for an
/// event.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrmScanoutFrameStats {
    /// Total frames requested (including the initial modeset frame).
    #[serde(default)]
    pub requested: u32,
    /// Commits issued to KMS.
    #[serde(default)]
    pub submitted: u32,
    /// Page-flip completions observed.
    #[serde(default)]
    pub completed: u32,
    /// Submitted frames that did not complete.
    #[serde(default)]
    pub dropped: u32,
    /// Dropped frames whose flip event timed out.
    #[serde(default)]
    pub missed: u32,
    /// Number of rotating dumb buffers/framebuffers.
    #[serde(default)]
    pub buffer_count: u32,
    /// Page-flip events consumed from the lessee fd.
    #[serde(default)]
    pub page_flip_events: u32,
    /// Moving damage rectangles drawn after the initial frame.
    #[serde(default)]
    pub damage_updates: u32,
    /// Average interval between observed flip completions, in microseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub average_interval_us: Option<u64>,
    /// Last observed vblank/flip sequence, when the driver reports one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_sequence: Option<u32>,
    /// FNV-1a checksum of the buffer backing the last completed frame.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_checksum: Option<u32>,
}

impl DrmScanoutFrameStats {
    /// Initial single-frame stats for a request that has committed one frame.
    pub fn initial(frames: u32, buffer_count: usize, checksum: u32) -> Self {
        Self {
            requested: frames,
            submitted: 1,
            completed: 1,
            dropped: 0,
            missed: 0,
            buffer_count: buffer_count as u32,
            page_flip_events: 0,
            damage_updates: 0,
            average_interval_us: None,
            last_sequence: None,
            final_checksum: Some(checksum),
        }
    }
}

/// Number of rotating dumb buffers for a frame count.
///
/// The original single-frame proof keeps one buffer. A real flip loop uses
/// double buffering for two frames and triple buffering for three or more, so
/// the previous frame can still be scanned out while the next is prepared.
pub fn frame_buffer_count(frames: u32) -> usize {
    match frames {
        0 | 1 => 1,
        2 => 2,
        _ => 3,
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
    /// A bounded page-flip loop presented more than the static proof frame.
    FrameLoop,
    /// Writeback capture is configured for the leased CRTC.
    WritebackCapture,
    /// The requested pixel verification mechanism is unavailable.
    VerificationUnavailable,
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
            Self::FrameLoop => "frame_loop",
            Self::WritebackCapture => "writeback_capture",
            Self::VerificationUnavailable => "verification_unavailable",
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

/// Bounded pixel-capture accounting for one scanout proof.
///
/// Checksums are FNV-1a over the visible pixels only (`width * 4` bytes per
/// row, pitch padding excluded) with the alpha byte masked to zero. This makes
/// the value independent of the driver's alpha normalisation and of dumb-buffer
/// row padding while still proving every visible RGB pixel.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrmScanoutCapture {
    /// Writeback connector object id used for capture.
    pub writeback_connector_id: u32,
    /// Capture buffer width.
    pub width: u32,
    /// Capture buffer height.
    pub height: u32,
    /// Capture buffer pitch.
    pub pitch: u32,
    /// Capture framebuffer format name.
    pub format: String,
    /// Frames captured so far.
    #[serde(default)]
    pub captured_frames: u32,
    /// Captured visible-RGB checksums, bounded by
    /// [`SCANOUT_MAX_CAPTURE_SAMPLES`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checksums: Vec<u32>,
    /// Expected visible-RGB checksums for the corresponding captures.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expected_checksums: Vec<u32>,
    /// Captures whose checksum differed from the scanout buffer.
    #[serde(default)]
    pub mismatches: u32,
    /// Capture jobs that timed out.
    #[serde(default)]
    pub timeouts: u32,
    /// Transient commit retries actually performed.
    #[serde(default)]
    pub job_retries: u32,
    /// First mismatch detail, when any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_mismatch: Option<String>,
}

impl DrmScanoutCapture {
    /// Initial empty capture accounting for a configured writeback connector.
    pub fn new(writeback_connector_id: u32, width: u32, height: u32, pitch: u32) -> Self {
        Self {
            writeback_connector_id,
            width,
            height,
            pitch,
            format: SCANOUT_FOURCC.to_owned(),
            captured_frames: 0,
            checksums: Vec::new(),
            expected_checksums: Vec::new(),
            mismatches: 0,
            timeouts: 0,
            job_retries: 0,
            last_mismatch: None,
        }
    }

    /// Record one completed capture and return whether it matched.
    pub fn record(&mut self, captured: u32, expected: u32, frame: u32) -> bool {
        self.captured_frames = self.captured_frames.saturating_add(1);
        if self.checksums.len() < SCANOUT_MAX_CAPTURE_SAMPLES {
            self.checksums.push(captured);
            self.expected_checksums.push(expected);
        }
        if captured == expected {
            true
        } else {
            self.mismatches = self.mismatches.saturating_add(1);
            self.last_mismatch = Some(format!(
                "writeback frame {frame}: captured visible-RGB checksum {captured:#010x} does not \
                 match scanout checksum {expected:#010x}"
            ));
            false
        }
    }
}

/// A precise reason why the requested pixel verification could not run.
///
/// This is evidence, not a simulated capture: the scanout proof can still have
/// programmed the leased objects, but `verify` will return the typed
/// `kms-verification-unavailable` error and no captured checksum is invented.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrmScanoutVerificationBlocker {
    /// Stable blocker kind: always `kms-verification-unavailable`.
    pub kind: String,
    /// Requested mechanism (`writeback` or `crc`).
    pub mechanism: String,
    /// Why it could not run.
    pub reason: String,
    /// Kernel/card evidence collected while deciding.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kernel_evidence: Vec<String>,
}

impl DrmScanoutVerificationBlocker {
    /// Build a `kms-verification-unavailable` blocker.
    pub fn new(
        mechanism: impl Into<String>,
        reason: impl Into<String>,
        kernel_evidence: Vec<String>,
    ) -> Self {
        Self {
            kind: "kms-verification-unavailable".to_owned(),
            mechanism: mechanism.into(),
            reason: reason.into(),
            kernel_evidence,
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
    /// Pixel verification mechanism used by this verification.
    #[serde(default)]
    pub verification_mode: DrmScanoutVerifyMode,
    /// Writeback capture accounting, when capture ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture: Option<DrmScanoutCapture>,
    /// Typed blocker when the requested mechanism could not run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification_blocker: Option<DrmScanoutVerificationBlocker>,
    /// Whether every checked field matched the request and every capture
    /// matched its expected checksum.
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
    /// Frame-loop accounting, when `frames > 1`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frames: Option<DrmScanoutFrameStats>,
    /// Whether a page flip was accepted after the modeset.
    #[serde(default)]
    pub page_flipped: bool,
    /// Requested pixel verification mechanism.
    #[serde(default)]
    pub verification_mode: DrmScanoutVerifyMode,
    /// Leased writeback connector, when capture is configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub writeback_connector_id: Option<u32>,
    /// Writeback connector name, when capture is configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub writeback_connector: Option<String>,
    /// Capture accounting (frames, checksums, mismatches, timeouts).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture: Option<DrmScanoutCapture>,
    /// Typed blocker when the requested capture mechanism could not run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification_blocker: Option<DrmScanoutVerificationBlocker>,
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
            && self.frames.is_none()
            && !self.page_flipped
            && self.verification_mode == DrmScanoutVerifyMode::None
            && self.writeback_connector_id.is_none()
            && self.writeback_connector.is_none()
            && self.capture.is_none()
            && self.verification_blocker.is_none()
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
            verification_mode: request.verify,
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
    /// Present the bounded frame loop described by `plan`, waiting for each
    /// page-flip completion event with a bounded timeout. The initial modeset
    /// frame already committed by [`DrmScanoutBackend::start`] counts as frame
    /// one; `plan.frames == 1` is a no-op that returns the initial stats.
    ///
    /// Frame stats are recorded in [`DrmScanoutStatus::frames`] even when this
    /// returns a typed error, so partial accounting is never lost. A frame is
    /// only counted as completed after its flip event arrives.
    fn run_frames(
        &mut self,
        plan: DrmScanoutFramePlan,
    ) -> Result<DrmScanoutFrameStats, DrmScanoutError> {
        let _ = plan;
        Err(DrmScanoutError::FlipUnsupported {
            card: self.status().card.clone(),
            reason: "this scanout backend does not implement the frame loop".to_owned(),
        })
    }
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

    /// The requested frame count is outside the bounded proof range.
    #[error("invalid scanout frame count {frames}; expected {min}..={max}")]
    InvalidFrameCount {
        /// Requested frame count.
        frames: u32,
        /// Inclusive lower bound.
        min: u32,
        /// Inclusive upper bound.
        max: u32,
    },

    /// A page-flip completion event did not arrive inside the bounded wait.
    #[error("timed out waiting for frame {frame} flip completion on {card} after {timeout_ms}ms")]
    FlipTimeout {
        /// Card path or name.
        card: String,
        /// Zero-based frame index that timed out.
        frame: u32,
        /// Bounded wait.
        timeout_ms: u64,
    },

    /// The driver or commit path cannot deliver page-flip events.
    #[error("page flips are not supported on {card}: {reason}")]
    FlipUnsupported {
        /// Card path or name.
        card: String,
        /// Kernel detail.
        reason: String,
    },

    /// A page-flip commit failed.
    #[error("page flip for frame {frame} failed on {card}: {reason}")]
    FlipFailed {
        /// Card path or name.
        card: String,
        /// Zero-based frame index that failed.
        frame: u32,
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

    /// The requested pixel verification mechanism could not run. No capture is
    /// invented; the kernel evidence is in status.
    #[error("KMS {mechanism} verification is unavailable on {card}: {reason}")]
    VerificationUnavailable {
        /// Card path or name.
        card: String,
        /// Requested mechanism (`writeback` or `crc`).
        mechanism: String,
        /// Why it is unavailable.
        reason: String,
    },

    /// A writeback capture job did not complete inside the bounded wait.
    #[error("timed out waiting for writeback frame {frame} on {card} after {timeout_ms}ms")]
    WritebackTimeout {
        /// Card path or name.
        card: String,
        /// Zero-based capture frame index.
        frame: u32,
        /// Bounded wait.
        timeout_ms: u64,
    },

    /// A writeback job could not be committed or prepared.
    #[error("writeback capture for frame {frame} failed on {card}: {reason}")]
    WritebackFailed {
        /// Card path or name.
        card: String,
        /// Zero-based capture frame index.
        frame: u32,
        /// Kernel/driver detail.
        reason: String,
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
            Self::InvalidFrameCount { .. } => "invalid_frame_count",
            Self::FlipTimeout { .. } => "flip_timeout",
            Self::FlipUnsupported { .. } => "flip_unsupported",
            Self::FlipFailed { .. } => "flip_failed",
            Self::VerificationFailed { .. } => "verification_failed",
            Self::VerificationUnavailable { .. } => "kms-verification-unavailable",
            Self::WritebackTimeout { .. } => "writeback_timeout",
            Self::WritebackFailed { .. } => "writeback_failed",
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
    use std::fs;
    use std::num::NonZeroU32;
    use std::os::fd::{AsFd, BorrowedFd};
    use std::path::Path;
    use std::time::{Duration, Instant};

    use drm::buffer::{Buffer, DrmFourcc};
    use drm::control::{
        atomic::AtomicModeReq, connector, crtc, framebuffer, plane, property, AtomicCommitFlags,
        Device as ControlDevice, Event, Mode, PageFlipFlags, RawResourceHandle, ResourceHandle,
    };
    use drm::ClientCapability;
    use drm::Device;
    use nix::errno::Errno;
    use nix::poll::{poll, PollFd, PollFlags, PollTimeout};

    use super::{
        apply_frame_damage, drm_status_name, ensure_scanout_card, fill_pattern, frame_buffer_count,
        pattern_checksum, visible_rgb_checksum, DrmScanoutBackend, DrmScanoutCapture,
        DrmScanoutCommit, DrmScanoutError, DrmScanoutFramePlan, DrmScanoutFrameStats,
        DrmScanoutHandle, DrmScanoutNote, DrmScanoutNoteKind, DrmScanoutRequest, DrmScanoutStatus,
        DrmScanoutVerification, DrmScanoutVerificationBlocker, DrmScanoutVerifyMode, BORDER_PX,
        DIAGONAL_PX, MAX_ATOMIC_PROPERTIES, SCANOUT_BPP, SCANOUT_DAMAGE_PATTERN, SCANOUT_DEPTH,
        SCANOUT_FOURCC, SCANOUT_MAX_FRAMES, SCANOUT_MAX_VERIFY_BYTES, SCANOUT_PATTERN,
    };
    use crate::drm_lease::{
        DrmCard, DrmCardSelector, DrmCrtcSelector, DrmLeaseBackend, DrmLeaseHandle,
        DrmLeaseRequest, DrmWritebackConnector, RealDrmLeaseBackend,
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

        /// Resolve the CRTC before leasing when writeback verification needs to
        /// add a writeback connector to the same lease. Pinning the CRTC makes
        /// the writeback connector selection deterministic.
        fn resolve_crtc(
            &self,
            card: &DrmCard,
            request: &DrmLeaseRequest,
        ) -> Result<u32, DrmScanoutError> {
            if let DrmCrtcSelector::Id(id) = request.crtc {
                return Ok(id);
            }
            let inspected = self.lease.inspect(card)?;
            if let Some(connector) = inspected
                .connectors
                .iter()
                .find(|connector| connector.name == request.connector)
            {
                if let Some(crtc_id) = connector.crtc_id {
                    return Ok(crtc_id);
                }
            }
            inspected
                .crtcs
                .first()
                .copied()
                .ok_or_else(|| DrmScanoutError::UnsupportedConnector {
                    card: card.path.display().to_string(),
                    connector: request.connector.clone(),
                    reason: "the card reports no CRTC".to_owned(),
                })
        }
    }

    fn kernel_evidence(card: &DrmCard) -> Vec<String> {
        let mut evidence = vec![format!(
            "card {} driver {}",
            card.path.display(),
            card.driver
        )];
        if let Ok(release) = fs::read_to_string("/proc/sys/kernel/osrelease") {
            evidence.push(format!("kernel {}", release.trim()));
        }
        for (label, path) in [
            (
                "vkms enable_writeback",
                "/sys/module/vkms/parameters/enable_writeback",
            ),
            (
                "vkms create_default_dev",
                "/sys/module/vkms/parameters/create_default_dev",
            ),
        ] {
            if let Ok(value) = fs::read_to_string(path) {
                evidence.push(format!("{label}={}", value.trim()));
            }
        }
        evidence
    }

    fn crc_blocker(card: &DrmCard) -> DrmScanoutVerificationBlocker {
        let mut evidence = kernel_evidence(card);
        evidence.push(
            "vkms implements CRTC CRC callbacks (vkms_get_crc_sources/vkms_set_crc_source)"
                .to_owned(),
        );
        evidence.push(
            "userspace CRC needs DRM_IOCTL_CRTC_GET_SEQUENCE, DRM_IOCTL_CRTC_SET_CRC_SOURCE, and \
             DRM_IOCTL_CRTC_GET_CRC"
                .to_owned(),
        );
        evidence.push(
            "drm 0.15.0 exposes no safe CRC wrapper and this crate forbids unsafe code; writeback \
             capture is the implemented strongest mechanism"
                .to_owned(),
        );
        DrmScanoutVerificationBlocker::new(
            "crc",
            "CRTC CRC capture is not exposed by the safe DRM API used by ozma-multiseat",
            evidence,
        )
    }

    impl DrmScanoutBackend for RealDrmScanoutBackend {
        fn start(
            &self,
            request: &DrmScanoutRequest,
        ) -> Result<Box<dyn DrmScanoutHandle>, DrmScanoutError> {
            request.validate()?;
            let card = self.resolve_card(&request.lease.card)?;
            ensure_scanout_card(&card, &request.lease.card)?;

            // Resolve the CRTC before leasing only when capture has to add a
            // writeback connector to the same lease. The hint never changes the
            // normal scanout lease shape when verification is off.
            let crtc_hint =
                if request.verify == DrmScanoutVerifyMode::Writeback && !request.force_legacy {
                    Some(self.resolve_crtc(&card, &request.lease)?)
                } else {
                    None
                };
            let mut lease_request = request.lease.clone();
            let mut writeback: Option<DrmWritebackConnector> = None;
            let mut verification_blocker = None;
            match request.verify {
                DrmScanoutVerifyMode::None => {}
                DrmScanoutVerifyMode::Crc => {
                    verification_blocker = Some(crc_blocker(&card));
                }
                DrmScanoutVerifyMode::Writeback if request.force_legacy => {
                    verification_blocker = Some(DrmScanoutVerificationBlocker::new(
                        "writeback",
                        "writeback capture requires atomic KMS, but the request forced the legacy \
                         commit path",
                        kernel_evidence(&card),
                    ));
                }
                DrmScanoutVerifyMode::Writeback => {
                    let crtc_hint = crtc_hint.unwrap_or(0);
                    match self.lease.writeback_connectors(&request.lease.card) {
                        Ok(connectors) => {
                            let compatible = connectors
                                .into_iter()
                                .find(|connector| connector.possible_crtcs.contains(&crtc_hint));
                            match compatible {
                                Some(connector) => {
                                    lease_request = lease_request
                                        .with_crtc(crtc_hint)
                                        .with_writeback_connector(connector.object_id);
                                    writeback = Some(connector);
                                }
                                None => {
                                    let mut evidence = kernel_evidence(&card);
                                    evidence.push(format!(
                                        "no writeback connector compatible with CRTC {crtc_hint} \
                                         was reported by the lessor"
                                    ));
                                    verification_blocker =
                                        Some(DrmScanoutVerificationBlocker::new(
                                            "writeback",
                                            format!(
                                                "no writeback connector can drive CRTC {crtc_hint}"
                                            ),
                                            evidence,
                                        ));
                                }
                            }
                        }
                        Err(error) => {
                            let mut evidence = kernel_evidence(&card);
                            evidence.push(format!("writeback connector discovery failed: {error}"));
                            verification_blocker = Some(DrmScanoutVerificationBlocker::new(
                                "writeback",
                                format!("cannot discover writeback connectors: {error}"),
                                evidence,
                            ));
                        }
                    }
                }
            }

            let lease = self.lease.acquire(&lease_request)?;
            let connector_id = lease.connector_id();
            let crtc_id = lease.crtc_id();
            let Some(fd) = lease.as_fd() else {
                return Err(DrmScanoutError::UnsupportedVkms {
                    card: card.path.display().to_string(),
                    driver: card.driver.clone(),
                });
            };
            let device = LeaseDevice { fd };

            let display_connector = connector_handle(connector_id, &card.path)?;
            let crtc_handle = crtc_handle(crtc_id, &card.path)?;

            let connector_info = device
                .get_connector(display_connector, false)
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

            // The original proof keeps one buffer. A real loop uses two or
            // three so the previous frame stays scannable while the next one is
            // prepared and committed.
            let buffer_count = frame_buffer_count(request.frames);
            let mut slots: Vec<ScanoutSlot> = Vec::with_capacity(buffer_count);
            for _ in 0..buffer_count {
                let mut buffer = device
                    .create_dumb_buffer((width, height), DrmFourcc::Xrgb8888, SCANOUT_BPP)
                    .map_err(|error| DrmScanoutError::BufferFailed {
                        card: card.path.display().to_string(),
                        reason: error.to_string(),
                    })?;
                let pitch = buffer.pitch();
                let (checksum, visible_checksum) = {
                    let mut mapping = device.map_dumb_buffer(&mut buffer).map_err(|error| {
                        DrmScanoutError::BufferFailed {
                            card: card.path.display().to_string(),
                            reason: error.to_string(),
                        }
                    })?;
                    let checksum = fill_pattern(&mut mapping, width, height, pitch);
                    let visible = visible_rgb_checksum(&mapping, width, height, pitch);
                    (checksum, visible)
                };
                let framebuffer = device
                    .add_framebuffer(&buffer, SCANOUT_DEPTH, SCANOUT_BPP)
                    .map_err(|error| DrmScanoutError::FramebufferFailed {
                        card: card.path.display().to_string(),
                        reason: error.to_string(),
                    })?;
                slots.push(ScanoutSlot {
                    buffer,
                    framebuffer,
                    checksum,
                    visible_checksum,
                });
            }
            let pitch = slots[0].buffer.pitch();
            let initial_checksum = slots[0].checksum;

            let plane_ids = lease.plane_ids().to_vec();
            let requested_plane = request.lease.plane_id();
            let request_flip_event = request.frames > 1;
            let mut fallback_reason = None;
            let mut forced_legacy = false;
            let initial_framebuffer = slots[0].framebuffer;
            let (commit, plane_id, page_flipped) = if request.force_legacy {
                forced_legacy = true;
                match legacy_commit(
                    &device,
                    &card.path,
                    &request.lease.connector,
                    crtc_handle,
                    display_connector,
                    initial_framebuffer,
                    mode,
                    &mode_name,
                    request_flip_event,
                ) {
                    Ok(page_flipped) => {
                        if request_flip_event && !page_flipped {
                            let _ = destroy_slots(&device, &card.path, &mut slots);
                            return Err(DrmScanoutError::FlipUnsupported {
                                card: card.path.display().to_string(),
                                reason: "legacy page flip was rejected; the frame loop needs \
                                         completion events"
                                    .to_owned(),
                            });
                        }
                        (DrmScanoutCommit::Legacy, requested_plane, page_flipped)
                    }
                    Err(error) => {
                        let _ = destroy_slots(&device, &card.path, &mut slots);
                        return Err(error);
                    }
                }
            } else {
                match try_atomic_commit(
                    &device,
                    &card.path,
                    &request.lease.connector,
                    display_connector,
                    connector_id,
                    crtc_handle,
                    crtc_id,
                    initial_framebuffer,
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
                            display_connector,
                            initial_framebuffer,
                            mode,
                            &mode_name,
                            request_flip_event,
                        ) {
                            Ok(page_flipped) => {
                                if request_flip_event && !page_flipped {
                                    let _ = destroy_slots(&device, &card.path, &mut slots);
                                    return Err(DrmScanoutError::FlipUnsupported {
                                        card: card.path.display().to_string(),
                                        reason: "legacy page flip was rejected; the frame loop \
                                                 needs completion events"
                                            .to_owned(),
                                    });
                                }
                                (DrmScanoutCommit::Legacy, requested_plane, page_flipped)
                            }
                            Err(error) => {
                                let _ = destroy_slots(&device, &card.path, &mut slots);
                                return Err(error);
                            }
                        }
                    }
                    Err(AtomicAttempt::Failed(error)) => {
                        let _ = destroy_slots(&device, &card.path, &mut slots);
                        return Err(error);
                    }
                }
            };

            // Configure writeback capture only after the scanout commit chose
            // its final commit path. A writeback connector is useless if the
            // proof had to fall back to legacy KMS.
            let mut capture: Option<CaptureSlot> = None;
            if let Some(writeback) = writeback.as_ref() {
                if commit == DrmScanoutCommit::Legacy {
                    verification_blocker = Some(DrmScanoutVerificationBlocker::new(
                        "writeback",
                        "writeback capture requires atomic KMS, but scanout fell back to the \
                         legacy commit path",
                        kernel_evidence(&card),
                    ));
                } else if let Err(error) =
                    device.set_client_capability(ClientCapability::WritebackConnectors, true)
                {
                    let mut evidence = kernel_evidence(&card);
                    evidence.push(format!(
                        "DRM_CLIENT_CAP_WRITEBACK_CONNECTORS on the lessee failed: {error}"
                    ));
                    verification_blocker = Some(DrmScanoutVerificationBlocker::new(
                        "writeback",
                        format!("the lessee cannot use writeback connectors: {error}"),
                        evidence,
                    ));
                } else {
                    let writeback_handle = connector_handle(writeback.object_id, &card.path)?;
                    match AtomicProperties::read(
                        &device,
                        writeback_handle,
                        &card.path,
                        "connector",
                        writeback.object_id,
                    ) {
                        Ok(properties) => {
                            let has_crtc = properties
                                .require(&card.path, "connector", writeback.object_id, "CRTC_ID")
                                .is_ok();
                            let has_fb = properties
                                .require(
                                    &card.path,
                                    "connector",
                                    writeback.object_id,
                                    "WRITEBACK_FB_ID",
                                )
                                .is_ok();
                            let formats = properties
                                .blob(&device, &card.path, "WRITEBACK_PIXEL_FORMATS")
                                .unwrap_or(None);
                            let xrgb = DrmFourcc::Xrgb8888 as u32;
                            let supports_xrgb = formats.as_ref().is_some_and(|blob| {
                                blob.chunks_exact(4).any(|chunk| {
                                    u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]])
                                        == xrgb
                                })
                            });
                            let capture_bytes = u64::from(pitch) * u64::from(height);
                            if !has_crtc || !has_fb {
                                let mut evidence = kernel_evidence(&card);
                                evidence.push(format!(
                                    "writeback connector {} lacks CRTC_ID or WRITEBACK_FB_ID",
                                    writeback.object_id
                                ));
                                verification_blocker = Some(DrmScanoutVerificationBlocker::new(
                                    "writeback",
                                    "the writeback connector lacks the atomic properties \
                                         required to queue a job",
                                    evidence,
                                ));
                            } else if !supports_xrgb {
                                let mut evidence = kernel_evidence(&card);
                                evidence.push(format!(
                                    "WRITEBACK_PIXEL_FORMATS={:?}",
                                    formats.as_ref().map(|blob| blob
                                        .chunks_exact(4)
                                        .map(|chunk| u32::from_le_bytes([
                                            chunk[0], chunk[1], chunk[2], chunk[3]
                                        ]))
                                        .collect::<Vec<_>>())
                                ));
                                verification_blocker =
                                    Some(DrmScanoutVerificationBlocker::new(
                                        "writeback",
                                        format!(
                                            "writeback connector {} does not advertise {SCANOUT_FOURCC}",
                                            writeback.object_id
                                        ),
                                        evidence,
                                    ));
                            } else if capture_bytes > SCANOUT_MAX_VERIFY_BYTES {
                                let mut evidence = kernel_evidence(&card);
                                evidence.push(format!(
                                    "capture buffer would be {capture_bytes} bytes; bound is \
                                     {SCANOUT_MAX_VERIFY_BYTES}"
                                ));
                                verification_blocker = Some(DrmScanoutVerificationBlocker::new(
                                    "writeback",
                                    "the capture buffer exceeds the verification size bound",
                                    evidence,
                                ));
                            } else {
                                let buffer = match device.create_dumb_buffer(
                                    (width, height),
                                    DrmFourcc::Xrgb8888,
                                    SCANOUT_BPP,
                                ) {
                                    Ok(buffer) => buffer,
                                    Err(error) => {
                                        let _ = destroy_slots(&device, &card.path, &mut slots);
                                        return Err(DrmScanoutError::BufferFailed {
                                            card: card.path.display().to_string(),
                                            reason: format!("writeback capture buffer: {error}"),
                                        });
                                    }
                                };
                                let framebuffer = match device.add_framebuffer(
                                    &buffer,
                                    SCANOUT_DEPTH,
                                    SCANOUT_BPP,
                                ) {
                                    Ok(framebuffer) => framebuffer,
                                    Err(error) => {
                                        let _ = device.destroy_dumb_buffer(buffer);
                                        let _ = destroy_slots(&device, &card.path, &mut slots);
                                        return Err(DrmScanoutError::FramebufferFailed {
                                            card: card.path.display().to_string(),
                                            reason: format!(
                                                "writeback capture framebuffer: {error}"
                                            ),
                                        });
                                    }
                                };
                                capture = Some(CaptureSlot {
                                    buffer,
                                    framebuffer,
                                });
                            }
                        }
                        Err(error) => {
                            let mut evidence = kernel_evidence(&card);
                            evidence.push(format!(
                                "reading writeback connector properties failed: {error}"
                            ));
                            verification_blocker = Some(DrmScanoutVerificationBlocker::new(
                                "writeback",
                                format!(
                                    "cannot read writeback connector {} properties: {error}",
                                    writeback.object_id
                                ),
                                evidence,
                            ));
                        }
                    }
                }
            }

            let stats =
                DrmScanoutFrameStats::initial(request.frames, buffer_count, initial_checksum);
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
                framebuffer_id: Some(slots[0].framebuffer_id()),
                buffer_handle: Some(slots[0].buffer_handle()),
                pattern: SCANOUT_PATTERN.to_owned(),
                pattern_checksum: Some(initial_checksum),
                frames: Some(stats),
                page_flipped,
                verification_mode: request.verify,
                writeback_connector_id: writeback.as_ref().map(|writeback| writeback.object_id),
                writeback_connector: writeback.as_ref().map(|writeback| writeback.name.clone()),
                capture: capture.as_ref().map(|_| {
                    DrmScanoutCapture::new(
                        writeback
                            .as_ref()
                            .map(|writeback| writeback.object_id)
                            .unwrap_or(0),
                        width,
                        height,
                        pitch,
                    )
                }),
                verification_blocker: verification_blocker.clone(),
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
            if request.frames > 1 {
                status.push_note(DrmScanoutNote::new(
                    DrmScanoutNoteKind::FrameLoop,
                    format!(
                        "bounded frame loop: {} frames across {} rotating dumb buffers, \
                         damage pattern {SCANOUT_DAMAGE_PATTERN}, flip timeout {}ms",
                        request.frames, buffer_count, request.flip_timeout_ms
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
            if capture.is_some() {
                status.push_note(DrmScanoutNote::new(
                    DrmScanoutNoteKind::WritebackCapture,
                    format!(
                        "writeback connector {} captures the CRTC into an XRGB8888 dumb buffer; \
                         visible-RGB checksum, {}ms timeout, {} retries, max {} bytes",
                        writeback
                            .as_ref()
                            .map(|writeback| writeback.object_id)
                            .unwrap_or(0),
                        request.writeback_timeout_ms,
                        request.writeback_retries,
                        SCANOUT_MAX_VERIFY_BYTES
                    ),
                ));
            }
            if let Some(blocker) = status.verification_blocker.as_ref() {
                status.push_note(DrmScanoutNote::new(
                    DrmScanoutNoteKind::VerificationUnavailable,
                    format!(
                        "{} capture unavailable: {} (no capture claimed)",
                        blocker.mechanism, blocker.reason
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
                writeback_connector_id: writeback.as_ref().map(|writeback| writeback.object_id),
                capture,
                capture_timeout: std::time::Duration::from_millis(request.writeback_timeout_ms),
                capture_retries: request.writeback_retries,
                status,
                buffers: slots,
                current: 0,
                active: true,
            }))
        }
    }

    struct ScanoutSlot {
        buffer: drm::control::dumbbuffer::DumbBuffer,
        framebuffer: framebuffer::Handle,
        checksum: u32,
        visible_checksum: u32,
    }

    impl ScanoutSlot {
        fn buffer_handle(&self) -> u32 {
            u32::from(self.buffer.handle())
        }

        fn framebuffer_id(&self) -> u32 {
            u32::from(self.framebuffer)
        }
    }

    struct CaptureSlot {
        buffer: drm::control::dumbbuffer::DumbBuffer,
        framebuffer: framebuffer::Handle,
    }

    impl CaptureSlot {
        fn framebuffer_id(&self) -> u32 {
            u32::from(self.framebuffer)
        }
    }

    fn destroy_capture(
        device: &LeaseDevice<'_>,
        card: &Path,
        capture: CaptureSlot,
    ) -> Option<DrmScanoutError> {
        let mut first_error = None;
        if let Err(error) = device.destroy_framebuffer(capture.framebuffer) {
            first_error.get_or_insert(DrmScanoutError::FramebufferFailed {
                card: card.display().to_string(),
                reason: format!(
                    "destroying writeback framebuffer {:?}: {error}",
                    capture.framebuffer
                ),
            });
        }
        if let Err(error) = device.destroy_dumb_buffer(capture.buffer) {
            first_error.get_or_insert(DrmScanoutError::BufferFailed {
                card: card.display().to_string(),
                reason: format!("destroying writeback dumb buffer: {error}"),
            });
        }
        first_error
    }

    fn destroy_slots(
        device: &LeaseDevice<'_>,
        card: &Path,
        slots: &mut Vec<ScanoutSlot>,
    ) -> Option<DrmScanoutError> {
        let mut first_error = None;
        while let Some(slot) = slots.pop() {
            if let Err(error) = device.destroy_framebuffer(slot.framebuffer) {
                first_error.get_or_insert(DrmScanoutError::FramebufferFailed {
                    card: card.display().to_string(),
                    reason: format!("destroying framebuffer {:?}: {error}", slot.framebuffer),
                });
            }
            if let Err(error) = device.destroy_dumb_buffer(slot.buffer) {
                first_error.get_or_insert(DrmScanoutError::BufferFailed {
                    card: card.display().to_string(),
                    reason: format!("destroying dumb buffer: {error}"),
                });
            }
        }
        first_error
    }

    struct RealDrmScanout {
        lease: Option<Box<dyn DrmLeaseHandle>>,
        card: std::path::PathBuf,
        connector_name: String,
        connector_id: u32,
        crtc_id: u32,
        plane_id: Option<u32>,
        writeback_connector_id: Option<u32>,
        capture: Option<CaptureSlot>,
        capture_timeout: Duration,
        capture_retries: u32,
        status: DrmScanoutStatus,
        buffers: Vec<ScanoutSlot>,
        current: usize,
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

        fn run_frames(
            &mut self,
            plan: DrmScanoutFramePlan,
        ) -> Result<DrmScanoutFrameStats, DrmScanoutError> {
            if !self.active {
                return Err(DrmScanoutError::Released(self.card.display().to_string()));
            }
            if plan.frames == 0 || plan.frames > SCANOUT_MAX_FRAMES {
                return Err(DrmScanoutError::InvalidFrameCount {
                    frames: plan.frames,
                    min: 1,
                    max: SCANOUT_MAX_FRAMES,
                });
            }
            let Some(lease) = self.lease.as_ref() else {
                return Err(DrmScanoutError::Released(self.card.display().to_string()));
            };
            let Some(fd) = lease.as_fd() else {
                return Err(DrmScanoutError::Released(self.card.display().to_string()));
            };
            let device = LeaseDevice { fd };
            let card = self.card.clone();
            let crtc = crtc_handle(self.crtc_id, &card)?;
            let commit = self.status.commit;
            let plane_id = self.plane_id;
            let width = self.status.width;
            let height = self.status.height;
            let pitch = self.status.pitch;
            let buffer_count = self.buffers.len().max(1);
            let mut stats = self.status.frames.clone().unwrap_or_else(|| {
                DrmScanoutFrameStats::initial(
                    plan.frames,
                    buffer_count,
                    self.status.pattern_checksum.unwrap_or(0),
                )
            });
            stats.requested = plan.frames;
            stats.buffer_count = buffer_count as u32;
            stats.damage_updates = 0;
            stats.page_flip_events = 0;
            stats.average_interval_us = None;
            stats.last_sequence = None;

            // A one-frame plan is the original static proof: the initial
            // modeset commit already counts as the completed frame.
            if plan.frames <= 1 {
                stats.submitted = 1;
                stats.completed = 1;
                stats.dropped = 0;
                stats.missed = 0;
                stats.final_checksum = Some(self.status.pattern_checksum.unwrap_or(0));
                self.status.frames = Some(stats.clone());
                return Ok(stats);
            }

            let mut completions: Vec<Instant> = Vec::new();
            // Frame zero (the modeset) requested a completion event in
            // `start`; it is only counted as completed once that event lands.
            match wait_for_flip(&device, crtc, &card, 0, plan.flip_timeout) {
                Ok(sequence) => {
                    stats.page_flip_events += 1;
                    stats.last_sequence = Some(sequence);
                    completions.push(Instant::now());
                }
                Err(error) => {
                    stats.completed = 0;
                    stats.dropped = 1;
                    if matches!(error, DrmScanoutError::FlipTimeout { .. }) {
                        stats.missed = 1;
                    }
                    self.status.frames = Some(stats.clone());
                    self.status.mark_error(&error);
                    return Err(error);
                }
            }
            if self.capture.is_some() {
                if let Some(writeback_id) = self.writeback_connector_id {
                    let expected = self
                        .buffers
                        .get(self.current)
                        .map(|slot| slot.visible_checksum)
                        .unwrap_or(0);
                    if let Err(error) = capture_scanout_frame(
                        &device,
                        &card,
                        crtc,
                        self.crtc_id,
                        writeback_id,
                        &mut self.capture,
                        &mut self.status.capture,
                        expected,
                        width,
                        height,
                        self.capture_timeout,
                        self.capture_retries,
                        0,
                    ) {
                        self.status.frames = Some(stats.clone());
                        self.status.mark_error(&error);
                        return Err(error);
                    }
                }
            }

            for index in 1..plan.frames {
                if let (Some(interval), Some(last)) = (plan.interval, completions.last().copied()) {
                    let target = last + interval;
                    let now = Instant::now();
                    if target > now {
                        std::thread::sleep(target - now);
                    }
                }

                let slot_index = index as usize % buffer_count;
                let (checksum, visible_checksum) = {
                    let slot = &mut self.buffers[slot_index];
                    let handle = slot.buffer_handle();
                    let mut mapping =
                        device.map_dumb_buffer(&mut slot.buffer).map_err(|error| {
                            DrmScanoutError::BufferFailed {
                                card: card.display().to_string(),
                                reason: format!(
                                    "mapping dumb buffer {handle} for frame {index}: {error}"
                                ),
                            }
                        })?;
                    let checksum = apply_frame_damage(&mut mapping, width, height, pitch, index);
                    let visible = visible_rgb_checksum(&mapping, width, height, pitch);
                    (checksum, visible)
                };
                self.buffers[slot_index].checksum = checksum;
                self.buffers[slot_index].visible_checksum = visible_checksum;
                stats.damage_updates += 1;
                let framebuffer = self.buffers[slot_index].framebuffer;

                stats.submitted += 1;
                let flip = match commit {
                    DrmScanoutCommit::Atomic => match plane_id {
                        Some(plane_id) => {
                            atomic_page_flip(&device, &card, plane_id, framebuffer, index)
                        }
                        None => Err(DrmScanoutError::UnsupportedPlane {
                            card: card.display().to_string(),
                            crtc: self.crtc_id,
                            reason: "the atomic frame loop needs a leased plane".to_owned(),
                        }),
                    },
                    DrmScanoutCommit::Legacy => {
                        legacy_page_flip(&device, &card, crtc, framebuffer, index)
                    }
                };
                if let Err(error) = flip {
                    stats.dropped += 1;
                    self.status.frames = Some(stats.clone());
                    self.status.mark_error(&error);
                    return Err(error);
                }

                match wait_for_flip(&device, crtc, &card, index, plan.flip_timeout) {
                    Ok(sequence) => {
                        stats.completed += 1;
                        stats.page_flip_events += 1;
                        stats.last_sequence = Some(sequence);
                        completions.push(Instant::now());
                        self.current = slot_index;
                        self.status.framebuffer_id =
                            Some(self.buffers[slot_index].framebuffer_id());
                        self.status.buffer_handle = Some(self.buffers[slot_index].buffer_handle());
                        self.status.pattern_checksum = Some(checksum);
                        if self.capture.is_some() {
                            if let Some(writeback_id) = self.writeback_connector_id {
                                let expected = self.buffers[slot_index].visible_checksum;
                                if let Err(error) = capture_scanout_frame(
                                    &device,
                                    &card,
                                    crtc,
                                    self.crtc_id,
                                    writeback_id,
                                    &mut self.capture,
                                    &mut self.status.capture,
                                    expected,
                                    width,
                                    height,
                                    self.capture_timeout,
                                    self.capture_retries,
                                    index,
                                ) {
                                    self.status.frames = Some(stats.clone());
                                    self.status.mark_error(&error);
                                    return Err(error);
                                }
                            }
                        }
                    }
                    Err(error) => {
                        stats.dropped += 1;
                        if matches!(error, DrmScanoutError::FlipTimeout { .. }) {
                            stats.missed += 1;
                        }
                        self.status.frames = Some(stats.clone());
                        self.status.mark_error(&error);
                        return Err(error);
                    }
                }
            }

            if completions.len() >= 2 {
                let total = completions[completions.len() - 1] - completions[0];
                let micros = u64::try_from(total.as_micros()).unwrap_or(u64::MAX);
                stats.average_interval_us = Some(micros / (completions.len() as u64 - 1));
            }
            let final_checksum = {
                let slot = &mut self.buffers[self.current];
                let mapping = device.map_dumb_buffer(&mut slot.buffer).map_err(|error| {
                    DrmScanoutError::BufferFailed {
                        card: card.display().to_string(),
                        reason: format!("mapping the final dumb buffer for checksum: {error}"),
                    }
                })?;
                pattern_checksum(&mapping)
            };
            stats.final_checksum = Some(final_checksum);
            self.status.pattern_checksum = Some(final_checksum);
            self.status.frames = Some(stats.clone());
            Ok(stats)
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
            let Some(slot) = self.buffers.get(self.current) else {
                return Err(DrmScanoutError::Released(self.card.display().to_string()));
            };
            let framebuffer = slot.framebuffer;
            let (buffer_handle, size, pitch) = (
                u32::from(slot.buffer.handle()),
                slot.buffer.size(),
                slot.buffer.pitch(),
            );
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
            // Re-map the current dumb buffer and checksum the bytes that are
            // still being scanned out. This is stronger than trusting the
            // fill-time checksum stored in status.
            let actual_checksum = {
                let Some(slot) = self.buffers.get_mut(self.current) else {
                    return Err(DrmScanoutError::Released(self.card.display().to_string()));
                };
                let mapping = device.map_dumb_buffer(&mut slot.buffer).map_err(|error| {
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

            // GETFB returns a *new* GEM handle in this file for the framebuffer
            // object (drm_gem_fb_create_handle). Close it before any capture
            // return path so repeated verify calls do not accumulate handles.
            if let Some(handle) = framebuffer_info.buffer() {
                let _ = device.close_buffer(handle);
            }

            if mismatch.is_none()
                && self.status.verification_blocker.is_none()
                && self.status.verification_mode == DrmScanoutVerifyMode::Writeback
            {
                if let Some(writeback_id) = self.writeback_connector_id {
                    if self.capture.is_some() {
                        let expected = self
                            .buffers
                            .get(self.current)
                            .map(|slot| slot.visible_checksum)
                            .unwrap_or(0);
                        let capture_frame = self
                            .status
                            .capture
                            .as_ref()
                            .map(|capture| capture.captured_frames)
                            .unwrap_or(0);
                        if let Err(error) = capture_scanout_frame(
                            &device,
                            &self.card,
                            crtc_handle,
                            self.crtc_id,
                            writeback_id,
                            &mut self.capture,
                            &mut self.status.capture,
                            expected,
                            self.status.width,
                            self.status.height,
                            self.capture_timeout,
                            self.capture_retries,
                            capture_frame,
                        ) {
                            self.status_error(&error);
                            return Err(error);
                        }
                    }
                }
            }
            if let Some(capture) = self.status.capture.as_ref() {
                if capture.mismatches > 0 && mismatch.is_none() {
                    mismatch = capture
                        .last_mismatch
                        .clone()
                        .or_else(|| Some("writeback capture checksum mismatch".to_owned()));
                }
            }

            let blocker = self.status.verification_blocker.clone();
            let capture = self.status.capture.clone();
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
                verification_mode: self.status.verification_mode,
                capture,
                verification_blocker: blocker.clone(),
                ok: mismatch.is_none() && blocker.is_none(),
                mismatch: mismatch.clone(),
            };
            self.status.verification = Some(verification.clone());
            if let Some(blocker) = blocker {
                let error = DrmScanoutError::VerificationUnavailable {
                    card: self.card.display().to_string(),
                    mechanism: blocker.mechanism.clone(),
                    reason: blocker.reason.clone(),
                };
                self.status_error(&error);
                return Err(error);
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
                    // Clear the writeback job/detach the connector before the
                    // CRTC/plane state changes. Waiting for the capture event
                    // and retrying here covers the kernel's deferred job
                    // cleanup window.
                    if let Some(writeback_id) = self.writeback_connector_id {
                        match connector_handle(writeback_id, &card) {
                            Ok(writeback_handle) => {
                                if let Err(error) =
                                    clear_writeback(&device, &card, writeback_handle, writeback_id)
                                {
                                    first_error.get_or_insert(error);
                                }
                            }
                            Err(error) => {
                                first_error.get_or_insert(error);
                            }
                        }
                    }
                    if let Some(capture) = self.capture.take() {
                        if let Some(error) = destroy_capture(&device, &card, capture) {
                            first_error.get_or_insert(error);
                        }
                    }
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
                    if let Some(error) = destroy_slots(&device, &card, &mut self.buffers) {
                        first_error.get_or_insert(error);
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
                && !self.buffers.is_empty()
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

        fn blob(
            &self,
            device: &LeaseDevice<'_>,
            card: &Path,
            property: &str,
        ) -> Result<Option<Vec<u8>>, DrmScanoutError> {
            let Some((handle, raw)) = self.values.get(property) else {
                return Ok(None);
            };
            let info = device
                .get_property(*handle)
                .map_err(|error| map_io(card, error))?;
            match info.value_type().convert_value(*raw) {
                property::Value::Blob(id) if id != 0 => device
                    .get_property_blob(id)
                    .map(Some)
                    .map_err(|error| map_io(card, error)),
                _ => Ok(None),
            }
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
        request_event: bool,
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
        let flags = if request_event {
            PageFlipFlags::EVENT
        } else {
            PageFlipFlags::empty()
        };
        Ok(
            match device.page_flip(crtc_handle, framebuffer, flags, None) {
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

    /// Queue one legacy page flip that will deliver a completion event.
    fn legacy_page_flip(
        device: &LeaseDevice<'_>,
        card: &Path,
        crtc_handle: crtc::Handle,
        framebuffer: framebuffer::Handle,
        frame: u32,
    ) -> Result<(), DrmScanoutError> {
        device
            .page_flip(crtc_handle, framebuffer, PageFlipFlags::EVENT, None)
            .map_err(|error| match error.raw_os_error() {
                Some(libc::EOPNOTSUPP) | Some(libc::ENOSYS) | Some(libc::ENOTTY) => {
                    DrmScanoutError::FlipUnsupported {
                        card: card.display().to_string(),
                        reason: error.to_string(),
                    }
                }
                _ => DrmScanoutError::FlipFailed {
                    card: card.display().to_string(),
                    frame,
                    reason: error.to_string(),
                },
            })
    }

    /// Queue one atomic page flip (`FB_ID` only) that delivers an event.
    fn atomic_page_flip(
        device: &LeaseDevice<'_>,
        card: &Path,
        plane_id: u32,
        framebuffer: framebuffer::Handle,
        frame: u32,
    ) -> Result<(), DrmScanoutError> {
        let plane = plane_handle(plane_id, card)?;
        let plane_props = AtomicProperties::read(device, plane, card, "plane", plane_id)?;
        let mut request = AtomicModeReq::new();
        request.add_raw_property(
            RawResourceHandle::from(plane),
            plane_props.require(card, "plane", plane_id, "FB_ID")?,
            u64::from(u32::from(framebuffer)),
        );
        match device.atomic_commit(AtomicCommitFlags::PAGE_FLIP_EVENT, request) {
            Ok(()) => Ok(()),
            Err(error)
                if matches!(
                    error.raw_os_error(),
                    Some(libc::EOPNOTSUPP) | Some(libc::ENOSYS) | Some(libc::ENOTTY)
                ) =>
            {
                Err(DrmScanoutError::FlipUnsupported {
                    card: card.display().to_string(),
                    reason: error.to_string(),
                })
            }
            Err(error) => Err(DrmScanoutError::FlipFailed {
                card: card.display().to_string(),
                frame,
                reason: format!("atomic page flip: {error}"),
            }),
        }
    }

    struct CaptureOutcome {
        checksum: u32,
        retries: u32,
    }

    fn has_visible_data(bytes: &[u8], width: u32, height: u32, pitch: u32) -> bool {
        let row_bytes = (width as usize).saturating_mul(4);
        let pitch = pitch as usize;
        for y in 0..height as usize {
            let row_start = y.saturating_mul(pitch);
            let row_end = row_start.saturating_add(row_bytes);
            if row_end > bytes.len() {
                return false;
            }
            for pixel in bytes[row_start..row_end].chunks_exact(4) {
                let value =
                    u32::from_le_bytes([pixel[0], pixel[1], pixel[2], pixel[3]]) & 0x00ff_ffff;
                if value != 0x00a5_a5a5 {
                    return true;
                }
            }
        }
        false
    }

    /// Queue one writeback job for the scanout CRTC, wait for its completion
    /// event, then wait (bounded) for the capture buffer to leave the sentinel
    /// pattern. No out-fence pointer is passed: the safe `drm` crate surface
    /// cannot own the returned fd, and the proof already waits for the page-flip
    /// event before reading the buffer.
    #[allow(clippy::too_many_arguments)]
    fn capture_writeback_frame(
        device: &LeaseDevice<'_>,
        card: &Path,
        crtc: crtc::Handle,
        crtc_id: u32,
        writeback_handle: connector::Handle,
        writeback_id: u32,
        capture: &mut CaptureSlot,
        width: u32,
        height: u32,
        timeout: Duration,
        retries: u32,
        frame: u32,
    ) -> Result<CaptureOutcome, DrmScanoutError> {
        let pitch = capture.buffer.pitch();
        {
            let mut mapping = device
                .map_dumb_buffer(&mut capture.buffer)
                .map_err(|error| DrmScanoutError::BufferFailed {
                    card: card.display().to_string(),
                    reason: format!("mapping the writeback capture buffer: {error}"),
                })?;
            mapping.fill(0xA5);
        }

        let properties =
            AtomicProperties::read(device, writeback_handle, card, "connector", writeback_id)?;
        let writeback_raw = RawResourceHandle::from(writeback_handle);
        let mut retries_used = 0;
        loop {
            let mut request = AtomicModeReq::new();
            request.add_raw_property(
                writeback_raw,
                properties.require(card, "connector", writeback_id, "CRTC_ID")?,
                u64::from(crtc_id),
            );
            request.add_raw_property(
                writeback_raw,
                properties.require(card, "connector", writeback_id, "WRITEBACK_FB_ID")?,
                u64::from(capture.framebuffer_id()),
            );
            match device.atomic_commit(
                AtomicCommitFlags::ALLOW_MODESET | AtomicCommitFlags::PAGE_FLIP_EVENT,
                request,
            ) {
                Ok(()) => break,
                Err(error)
                    if matches!(error.raw_os_error(), Some(libc::EBUSY) | Some(libc::EAGAIN))
                        && retries_used < retries =>
                {
                    retries_used += 1;
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(error) => {
                    return Err(DrmScanoutError::WritebackFailed {
                        card: card.display().to_string(),
                        frame,
                        reason: format!("atomic writeback commit: {error}"),
                    })
                }
            }
        }

        wait_for_flip(device, crtc, card, frame, timeout).map_err(|error| match error {
            DrmScanoutError::FlipTimeout { .. } => DrmScanoutError::WritebackTimeout {
                card: card.display().to_string(),
                frame,
                timeout_ms: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
            },
            other => other,
        })?;

        let deadline = Instant::now() + timeout;
        loop {
            let (ready, checksum) = {
                let mapping = device
                    .map_dumb_buffer(&mut capture.buffer)
                    .map_err(|error| DrmScanoutError::BufferFailed {
                        card: card.display().to_string(),
                        reason: format!("re-mapping the writeback capture buffer: {error}"),
                    })?;
                (
                    has_visible_data(&mapping, width, height, pitch),
                    visible_rgb_checksum(&mapping, width, height, pitch),
                )
            };
            if ready {
                return Ok(CaptureOutcome {
                    checksum,
                    retries: retries_used,
                });
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(DrmScanoutError::WritebackTimeout {
                    card: card.display().to_string(),
                    frame,
                    timeout_ms: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
                });
            }
            std::thread::sleep(remaining.min(Duration::from_millis(2)));
        }
    }

    /// Run one writeback capture and record it. A checksum mismatch is kept in
    /// status but does not abort the frame loop; `verify` turns any recorded
    /// mismatch into a typed `verification_failed`. Timeouts and commit
    /// failures are returned immediately because no pixel evidence exists.
    #[allow(clippy::too_many_arguments)]
    fn capture_scanout_frame(
        device: &LeaseDevice<'_>,
        card: &Path,
        crtc: crtc::Handle,
        crtc_id: u32,
        writeback_id: u32,
        slot: &mut Option<CaptureSlot>,
        status_capture: &mut Option<DrmScanoutCapture>,
        expected: u32,
        width: u32,
        height: u32,
        timeout: Duration,
        retries: u32,
        frame: u32,
    ) -> Result<(), DrmScanoutError> {
        let Some(capture) = slot.as_mut() else {
            return Ok(());
        };
        let writeback = connector_handle(writeback_id, card)?;
        match capture_writeback_frame(
            device,
            card,
            crtc,
            crtc_id,
            writeback,
            writeback_id,
            capture,
            width,
            height,
            timeout,
            retries,
            frame,
        ) {
            Ok(outcome) => {
                if let Some(capture) = status_capture.as_mut() {
                    capture.job_retries = capture.job_retries.saturating_add(outcome.retries);
                    capture.record(outcome.checksum, expected, frame);
                }
                Ok(())
            }
            Err(error) => {
                if let Some(capture) = status_capture.as_mut() {
                    if matches!(error, DrmScanoutError::WritebackTimeout { .. }) {
                        capture.timeouts = capture.timeouts.saturating_add(1);
                    }
                }
                Err(error)
            }
        }
    }

    /// Clear the writeback job and detach the writeback connector. The kernel
    /// defers job cleanup after signalling, so a bounded retry absorbs the
    /// short window in which a commit can still return EINVAL/EBUSY.
    fn clear_writeback(
        device: &LeaseDevice<'_>,
        card: &Path,
        writeback_handle: connector::Handle,
        writeback_id: u32,
    ) -> Result<(), DrmScanoutError> {
        let properties =
            AtomicProperties::read(device, writeback_handle, card, "connector", writeback_id)?;
        let writeback_raw = RawResourceHandle::from(writeback_handle);
        let mut first_error = None;
        for (property, value, flags, label) in [
            (
                "WRITEBACK_FB_ID",
                0,
                AtomicCommitFlags::empty(),
                "clearing WRITEBACK_FB_ID",
            ),
            (
                "CRTC_ID",
                0,
                AtomicCommitFlags::ALLOW_MODESET,
                "detaching the writeback connector",
            ),
        ] {
            let property_handle =
                match properties.require(card, "connector", writeback_id, property) {
                    Ok(handle) => handle,
                    Err(error) => {
                        first_error.get_or_insert(error);
                        continue;
                    }
                };
            let mut cleared = false;
            for _ in 0..250 {
                let mut request = AtomicModeReq::new();
                request.add_raw_property(writeback_raw, property_handle, value);
                match device.atomic_commit(flags, request) {
                    Ok(()) => {
                        cleared = true;
                        break;
                    }
                    Err(error)
                        if matches!(
                            error.raw_os_error(),
                            Some(libc::EINVAL) | Some(libc::EBUSY) | Some(libc::EAGAIN)
                        ) =>
                    {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => {
                        first_error.get_or_insert(DrmScanoutError::WritebackFailed {
                            card: card.display().to_string(),
                            frame: 0,
                            reason: format!("{label}: {error}"),
                        });
                        break;
                    }
                }
            }
            if !cleared && first_error.is_none() {
                first_error.get_or_insert(DrmScanoutError::WritebackFailed {
                    card: card.display().to_string(),
                    frame: 0,
                    reason: format!("{label} timed out"),
                });
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Wait for one page-flip completion event for `crtc` with a bounded poll.
    ///
    /// Returns the kernel's vblank sequence when it reports one. Other event
    /// kinds are drained and ignored; a timeout is a typed `flip_timeout`
    /// rather than a silent skip.
    fn wait_for_flip(
        device: &LeaseDevice<'_>,
        crtc: crtc::Handle,
        card: &Path,
        frame: u32,
        timeout: Duration,
    ) -> Result<u32, DrmScanoutError> {
        let timeout_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX);
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(DrmScanoutError::FlipTimeout {
                    card: card.display().to_string(),
                    frame,
                    timeout_ms,
                });
            }
            let mut fds = [PollFd::new(device.as_fd(), PollFlags::POLLIN)];
            let poll_timeout = PollTimeout::try_from(remaining).unwrap_or(PollTimeout::MAX);
            match poll(&mut fds, poll_timeout) {
                Ok(0) => {
                    return Err(DrmScanoutError::FlipTimeout {
                        card: card.display().to_string(),
                        frame,
                        timeout_ms,
                    })
                }
                Ok(_) => {}
                Err(Errno::EINTR) => continue,
                Err(error) => {
                    return Err(DrmScanoutError::Io {
                        path: card.display().to_string(),
                        source: std::io::Error::from(error),
                    })
                }
            }
            let events = device
                .receive_events()
                .map_err(|error| DrmScanoutError::Io {
                    path: card.display().to_string(),
                    source: error,
                })?;
            for event in events {
                if let Event::PageFlip(flip) = event {
                    if flip.crtc == crtc {
                        return Ok(flip.frame);
                    }
                }
            }
        }
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

fn default_frames() -> u32 {
    SCANOUT_DEFAULT_FRAMES
}

fn is_single_frame(frames: &u32) -> bool {
    *frames == SCANOUT_DEFAULT_FRAMES
}

fn default_flip_timeout_ms() -> u64 {
    SCANOUT_FLIP_TIMEOUT_MS
}

fn is_default_flip_timeout(timeout: &u64) -> bool {
    *timeout == SCANOUT_FLIP_TIMEOUT_MS
}

fn is_verify_none(mode: &DrmScanoutVerifyMode) -> bool {
    *mode == DrmScanoutVerifyMode::None
}

fn default_writeback_timeout_ms() -> u64 {
    SCANOUT_WRITEBACK_TIMEOUT_MS
}

fn is_default_writeback_timeout(timeout: &u64) -> bool {
    *timeout == SCANOUT_WRITEBACK_TIMEOUT_MS
}

fn default_writeback_retries() -> u32 {
    SCANOUT_WRITEBACK_RETRIES
}

fn is_default_writeback_retries(retries: &u32) -> bool {
    *retries == SCANOUT_WRITEBACK_RETRIES
}

/// Deterministic moving damage rectangle for `frame`: `(x, y, width, height)`.
///
/// The rectangle visits a 3x3 grid (or a single position on very small modes)
/// in a fixed order. It is a proof of damage-style updates, not a damage
/// tracker: no client surface, dirty-region merge, or compositor is involved.
pub fn frame_damage_rect(width: u32, height: u32, frame: u32) -> (u32, u32, u32, u32) {
    if width == 0 || height == 0 {
        return (0, 0, 0, 0);
    }
    let rect_w = (width / 8).max(16).min(width);
    let rect_h = (height / 8).max(16).min(height);
    let steps_x = if width > rect_w { 3 } else { 1 };
    let steps_y = if height > rect_h { 3 } else { 1 };
    let slot = frame % (steps_x * steps_y);
    let col = slot % steps_x;
    let row = slot / steps_x;
    let x = if steps_x > 1 {
        col * (width - rect_w) / (steps_x - 1)
    } else {
        0
    };
    let y = if steps_y > 1 {
        row * (height - rect_h) / (steps_y - 1)
    } else {
        0
    };
    (x, y, rect_w, rect_h)
}

fn frame_damage_color(frame: u32) -> u32 {
    let red = frame.wrapping_mul(53) & 0xff;
    let green = frame.wrapping_mul(97) & 0xff;
    let blue = frame.wrapping_mul(29) & 0xff;
    (red << 16) | (green << 8) | blue
}

/// Draw only the deterministic damage rectangle for `frame` into `bytes` and
/// return the whole-buffer FNV-1a checksum.
///
/// The buffer is expected to already contain [`fill_pattern`] (or previous
/// damage), so this models an incremental update rather than a full repaint.
pub fn apply_frame_damage(
    bytes: &mut [u8],
    width: u32,
    height: u32,
    pitch: u32,
    frame: u32,
) -> u32 {
    let (rect_x, rect_y, rect_w, rect_h) = frame_damage_rect(width, height, frame);
    let color = frame_damage_color(frame);
    let pitch = pitch as usize;
    for y in rect_y..rect_y.saturating_add(rect_h) {
        let row_start = y as usize * pitch;
        for x in rect_x..rect_x.saturating_add(rect_w) {
            let offset = row_start + x as usize * 4;
            if offset + 4 <= bytes.len() {
                bytes[offset..offset + 4].copy_from_slice(&color.to_le_bytes());
            }
        }
    }
    pattern_checksum(bytes)
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

/// FNV-1a checksum of only the visible pixels, independent of row padding and
/// alpha normalisation.
///
/// Each XRGB8888 pixel is masked to `0x00ff_ffff` before hashing. Drivers may
/// write `0xff` into the unused X byte on capture, and dumb buffers normally
/// have pitch padding that the hardware never writes; neither is part of the
/// proof. This is the checksum used to compare a writeback capture with the
/// scanout buffer that produced it.
pub fn visible_rgb_checksum(bytes: &[u8], width: u32, height: u32, pitch: u32) -> u32 {
    let row_bytes = (width as usize).saturating_mul(4);
    let pitch = pitch as usize;
    let height = height as usize;
    let mut hash = 0x811c_9dc5u32;
    for y in 0..height {
        let row_start = y.saturating_mul(pitch);
        let row_end = row_start.saturating_add(row_bytes);
        if row_end > bytes.len() {
            break;
        }
        for pixel in bytes[row_start..row_end].chunks_exact(4) {
            let value = u32::from_le_bytes([pixel[0], pixel[1], pixel[2], pixel[3]]) & 0x00ff_ffff;
            for byte in value.to_le_bytes() {
                hash ^= u32::from(byte);
                hash = hash.wrapping_mul(0x0100_0193);
            }
        }
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
