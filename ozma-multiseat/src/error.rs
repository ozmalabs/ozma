// SPDX-License-Identifier: AGPL-3.0-only
//! Typed errors for multi-seat planning and seat supervision.

use std::path::Path;

/// Errors returned by the display, input, planning, and supervisor layers.
///
/// The variants deliberately distinguish a backend that could not be read
/// from a plan that would be dishonest (for example a seat with no display).
#[derive(Debug, thiserror::Error)]
pub enum MultiseatError {
    /// The display backend reported no displays. The planner refuses to invent
    /// a fake output for a seat.
    #[error("no displays were reported by the display backend; refusing to invent a seat")]
    NoDisplays,

    /// Two display outputs have the same name or index. One seat per display
    /// cannot be assigned without a stable identity.
    #[error("duplicate display {display:?}: {reason}")]
    DuplicateDisplay {
        /// Output name or index that repeated.
        display: String,
        /// Why the duplicate was rejected.
        reason: String,
    },

    /// Two seat plans resolved to the same seat id.
    #[error("duplicate seat id {0:?}")]
    DuplicateSeat(String),

    /// A display has zero width or height, so a capture region would be
    /// invalid.
    #[error("display {display:?} has invalid geometry {width}x{height}")]
    InvalidDisplay {
        /// Output name.
        display: String,
        /// Reported width.
        width: u32,
        /// Reported height.
        height: u32,
    },

    /// The requested seat count does not fit in the port space.
    #[error("cannot allocate {count} seat port pairs from UDP base {base_udp} and HTTP base {base_http}")]
    PortOverflow {
        /// Requested number of seats.
        count: usize,
        /// First UDP port.
        base_udp: u16,
        /// First HTTP port.
        base_http: u16,
    },

    /// A seat count of zero is not meaningful. `None` means auto-detect.
    #[error("seat count must be greater than zero")]
    ZeroSeats,

    /// The X11 capture provider needs an explicit display, and neither the
    /// output nor the planner option supplied one.
    #[error(
        "display {0:?} has no X11 screen and no explicit x11 display was configured; \
         set PlannerOptions::x11_display or use a PipeWire capture target"
    )]
    MissingX11Display(String),

    /// PipeWire capture was requested for a display with no configured target.
    /// The planner does not derive a made-up PipeWire node name.
    #[error("no PipeWire capture target is configured for display {0:?}")]
    MissingPipeWireTarget(String),

    /// The display backend returned a typed failure.
    #[error("display backend error: {0}")]
    DisplayBackend(String),

    /// The input backend returned a typed failure.
    #[error("input backend error: {0}")]
    InputBackend(String),

    /// A sysfs, /dev, or temporary file could not be read.
    #[error("failed to read {path}: {source}")]
    Io {
        /// Path that failed.
        path: String,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// A profile name did not match a built-in seat profile.
    #[error("unknown seat profile {0:?}; expected desktop, gaming, workstation, media, kiosk, signage, or custom")]
    UnknownProfile(String),

    /// A supervisor was asked to stop a seat it does not know.
    #[error("unknown seat {0:?}")]
    UnknownSeat(String),

    /// A supervisor was asked to start a seat that is already running.
    #[error("seat {0:?} is already running")]
    SeatAlreadyRunning(String),

    /// A supervisor-specific failure. The fake supervisor uses this for
    /// injected test failures; a future subprocess launcher uses it for spawn
    /// errors.
    #[error("seat supervisor error: {0}")]
    Supervisor(String),

    /// A required external tool (for example `xinput` or `xrandr`) could not
    /// be executed. This is a typed error so a missing tool never becomes a
    /// silent routing/ownership success.
    #[error("required tool {program:?} is unavailable: {reason}")]
    CommandUnavailable {
        /// Program that could not be executed.
        program: String,
        /// Why it could not be executed.
        reason: String,
    },

    /// An external routing/ownership command ran but exited non-zero.
    #[error("{program} {args:?} exited with status {status}: {stderr}")]
    CommandFailed {
        /// Program that was run.
        program: String,
        /// Arguments after the program name.
        args: Vec<String>,
        /// Exit status, or `-1` when the process was terminated by a signal.
        status: i32,
        /// Captured stderr.
        stderr: String,
    },

    /// An input group could not be assigned to a seat, or a release/rollback
    /// step could not be completed.
    #[error("input routing error: {0}")]
    InputRouting(String),

    /// A display could not be owned by a seat, or an ownership release step
    /// could not be completed.
    #[error("display ownership error: {0}")]
    DisplayOwnership(String),

    /// The requested operation needs a platform feature this crate models but
    /// deliberately does not execute (for example a real `EVIOCGRAB` ioctl or a
    /// compositor seat).
    #[error("unsupported operation: {0}")]
    Unsupported(String),

    /// The persisted seat store could not be read.
    #[error("failed to read seat store {path}: {message}")]
    SeatStoreRead {
        /// Seat store path.
        path: String,
        /// Underlying I/O or permission message.
        message: String,
    },

    /// The persisted seat store is not valid JSON or has the wrong shape.
    #[error("failed to parse seat store {path}: {message}")]
    SeatStoreParse {
        /// Seat store path.
        path: String,
        /// Parser detail.
        message: String,
    },

    /// The persisted seat store was written by a newer version.
    #[error("seat store {path} has version {found}, but this build supports at most version {supported}")]
    SeatStoreVersion {
        /// Seat store path.
        path: String,
        /// Version found on disk.
        found: u32,
        /// Highest supported version.
        supported: u32,
    },

    /// The seat store itself is internally inconsistent (empty id, zero port,
    /// duplicate display/port/id).
    #[error("invalid seat store: {0}")]
    SeatStoreInvalid(String),

    /// Two persisted seats or a persisted seat and discovered topology
    /// conflict in a way the planner cannot resolve deterministically.
    #[error("seat store conflict: {0}")]
    SeatStoreConflict(String),

    /// Per-seat audio sink provisioning or removal failed. The caller keeps the
    /// seat lifecycle non-fatal and reports this typed failure in status.
    #[error("seat audio routing error: {0}")]
    Audio(String),

    /// A hotplug poll or topology reconciliation failed.
    #[error("hotplug reconciliation error: {0}")]
    Hotplug(String),

    /// The privileged seat-input helper could not be started, refused a
    /// request, or lost a grab. Isolation is never claimed after this error.
    #[error("seat input isolation error: {0}")]
    InputIsolation(String),
}

impl MultiseatError {
    /// Build an [`MultiseatError::Io`] from a path and error.
    pub(crate) fn io(path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            path: path.display().to_string(),
            source,
        }
    }
}
