// SPDX-License-Identifier: AGPL-3.0-only
//! Typed failures returned by power backends.

use std::io;

use ozma_qmp::QmpError;

/// A typed failure from a power backend.
///
/// Every variant carries enough context to pick a remediation without logging
/// credentials or environment details:
///
/// * [`PowerError::NotConnected`] — call `connect()` first.
/// * [`PowerError::MissingCommand`] — install the missing package (for libvirt
///   that is `libvirt-client`, which provides `virsh`) or use the QMP backend.
/// * [`PowerError::CommandFailed`] — inspect the command and its stderr.
/// * [`PowerError::Qmp`] — QEMU answered but rejected or broke the request.
#[derive(Debug, thiserror::Error)]
pub enum PowerError {
    /// The operation is not implemented for this backend.
    #[error("unsupported power operation: {0}")]
    Unsupported(String),

    /// The backend must be connected before commands can be issued.
    #[error("power backend is not connected; connect it before issuing power commands")]
    NotConnected,

    /// Configuration supplied to a backend is invalid.
    #[error("invalid power backend configuration for {field}: {reason}")]
    InvalidConfig {
        /// Configuration field that failed validation (`domain`, `uri`, ...).
        field: &'static str,
        /// Human-readable reason.
        reason: String,
    },

    /// The executable could not be found on `PATH`.
    #[error("cannot run {command:?}: executable not found; {remediation}")]
    MissingCommand {
        /// Program name that was not found.
        command: String,
        /// Action the operator can take.
        remediation: String,
    },

    /// The subprocess could not be spawned (permissions, resource limits, ...).
    #[error("failed to run {command:?}: {source}")]
    Spawn {
        /// Full command that could not be started.
        command: String,
        /// Underlying spawn error.
        #[source]
        source: io::Error,
    },

    /// A subprocess exited with a non-zero status.
    #[error("command {command:?} failed with status {status}: {stderr}")]
    CommandFailed {
        /// Full command, rendered for humans.
        command: String,
        /// Exit status, or `-1` when the process was terminated by a signal.
        status: i32,
        /// Captured standard error (trimmed).
        stderr: String,
    },

    /// A QMP operation failed.
    #[error("QMP power operation failed: {0}")]
    Qmp(#[source] QmpError),

    /// A state or action name was not recognized, or is not valid in this
    /// context.
    #[error("invalid power state or action: {0}")]
    InvalidState(String),
}

impl PowerError {
    /// Stable machine-readable kind for HTTP/API responses and logs.
    ///
    /// The display string is for humans; this value lets a controller
    /// distinguish, for example, a refused/closed QMP connection from an
    /// unsupported operation without parsing prose.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Unsupported(_) => "unsupported",
            Self::NotConnected => "not-connected",
            Self::InvalidConfig { .. } => "invalid-config",
            Self::MissingCommand { .. } => "missing-command",
            Self::Spawn { .. } => "spawn",
            Self::CommandFailed { .. } => "command-failed",
            Self::Qmp(error) => match error {
                QmpError::Command { .. } => "qmp-command",
                QmpError::ConnectionClosed => "qmp-closed",
                QmpError::Connect { .. } => "qmp-connect",
                QmpError::Timeout { .. } => "qmp-timeout",
                _ => "qmp",
            },
            Self::InvalidState(_) => "invalid-state",
        }
    }
}

impl From<QmpError> for PowerError {
    fn from(error: QmpError) -> Self {
        match error {
            // Keep the stable backend-level meaning so callers can use one
            // `NotConnected` branch regardless of the backend.
            QmpError::NotConnected => PowerError::NotConnected,
            other => PowerError::Qmp(other),
        }
    }
}

/// Remediation shown when the executable is not installed.
pub(crate) const VIRSH_REMEDIATION: &str =
    "install the package that provides it (for virsh: libvirt-client) or use the QMP power backend";
