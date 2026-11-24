// SPDX-License-Identifier: AGPL-3.0-only
//! Typed failures for USB gadget, virtual-media, and USB/IP operations.

use std::io;
use std::path::PathBuf;

/// A typed USB/media failure.
///
/// Every variant is deliberately specific enough for a node API or the
/// controller to return a stable machine-readable `kind()` without parsing
/// prose. Filesystem failures keep the failing path and operation so a
/// misconfigured ConfigFS root is distinguishable from a missing gadget.
#[derive(Debug, thiserror::Error)]
pub enum UsbError {
    /// The gadget configuration itself is invalid.
    #[error("invalid USB gadget configuration: {0}")]
    InvalidConfig(String),

    /// One configuration field failed validation.
    #[error("invalid {field}: {reason}")]
    InvalidField {
        /// Field name (`name`, `udc`, `functions`, ...).
        field: &'static str,
        /// Human-readable reason.
        reason: String,
    },

    /// The named gadget directory does not exist.
    #[error("USB gadget {0:?} does not exist")]
    MissingGadget(String),

    /// The named gadget directory already exists.
    #[error("USB gadget {0:?} already exists")]
    GadgetExists(String),

    /// A mass-storage function is not present in the gadget.
    #[error("USB gadget {gadget:?} has no mass-storage function {function:?}")]
    MissingFunction {
        /// Gadget name.
        gadget: String,
        /// Function instance or `mass_storage.<instance>`.
        function: String,
    },

    /// The gadget is already bound to another UDC.
    #[error("USB gadget {gadget:?} is already bound to UDC {udc:?}")]
    AlreadyBound {
        /// Gadget name.
        gadget: String,
        /// Currently bound UDC.
        udc: String,
    },

    /// ConfigFS is not mounted at the configured root.
    #[error("ConfigFS root {0} does not exist; load libcomposite and mount configfs")]
    MissingRoot(PathBuf),

    /// The configured ConfigFS root exists but is not a directory.
    #[error("ConfigFS root {0} is not a directory")]
    RootNotDirectory(PathBuf),

    /// A media image path does not exist.
    #[error("media image {0} does not exist")]
    MediaNotFound(PathBuf),

    /// A media image path exists but is not a regular file.
    #[error("media image {0} is not a regular file")]
    MediaNotFile(PathBuf),

    /// An operation would make a read-only image writable.
    #[error("media is read-only: {0}")]
    ReadOnlyMedia(String),

    /// A USB/IP bus id, host, or port is malformed.
    #[error("invalid USB/IP {field}: {reason}")]
    InvalidUsbIp {
        /// Field name (`bus_id`, `host`, `port`).
        field: &'static str,
        /// Human-readable reason.
        reason: String,
    },

    /// A ConfigFS directory or attribute operation failed.
    #[error("ConfigFS {operation} failed at {path}: {source}")]
    ConfigFs {
        /// Operation that failed.
        operation: &'static str,
        /// Path involved.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: io::Error,
    },

    /// A non-ConfigFS filesystem operation failed.
    #[error("I/O {operation} failed at {path}: {source}")]
    Io {
        /// Operation that failed.
        operation: &'static str,
        /// Path involved.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: io::Error,
    },

    /// The executable needed by a subprocess controller is absent.
    #[error("cannot run {command:?}: executable not found; {remediation}")]
    MissingCommand {
        /// Program name.
        command: String,
        /// Operator remediation.
        remediation: String,
    },

    /// A subprocess could not be spawned.
    #[error("failed to run {command:?}: {source}")]
    Spawn {
        /// Rendered command for diagnostics.
        command: String,
        /// Underlying spawn error.
        #[source]
        source: io::Error,
    },

    /// A subprocess exited non-zero.
    #[error("command {command:?} failed with status {status}: {stderr}")]
    CommandFailed {
        /// Rendered command for diagnostics.
        command: String,
        /// Exit status (`-1` when terminated by a signal).
        status: i32,
        /// Trimmed standard error.
        stderr: String,
    },

    /// The operation is not available on this platform.
    #[error("operation is not supported on this platform")]
    UnsupportedPlatform,
}

impl UsbError {
    /// Stable machine-readable kind for API responses and logs.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::InvalidConfig(_) | Self::InvalidField { .. } => "invalid-config",
            Self::MissingGadget(_) => "missing-gadget",
            Self::GadgetExists(_) => "gadget-exists",
            Self::MissingFunction { .. } => "missing-function",
            Self::AlreadyBound { .. } => "already-bound",
            Self::MissingRoot(_) => "missing-configfs-root",
            Self::RootNotDirectory(_) => "configfs-root-not-directory",
            Self::MediaNotFound(_) => "media-not-found",
            Self::MediaNotFile(_) => "media-not-file",
            Self::ReadOnlyMedia(_) => "read-only-media",
            Self::InvalidUsbIp { .. } => "invalid-usbip",
            Self::ConfigFs { .. } => "configfs-io",
            Self::Io { .. } => "io",
            Self::MissingCommand { .. } => "missing-command",
            Self::Spawn { .. } => "spawn",
            Self::CommandFailed { .. } => "command-failed",
            Self::UnsupportedPlatform => "unsupported-platform",
        }
    }
}

pub(crate) fn configfs_error(
    operation: &'static str,
    path: PathBuf,
    source: io::Error,
) -> UsbError {
    UsbError::ConfigFs {
        operation,
        path,
        source,
    }
}

pub(crate) fn io_error(operation: &'static str, path: PathBuf, source: io::Error) -> UsbError {
    UsbError::Io {
        operation,
        path,
        source,
    }
}
