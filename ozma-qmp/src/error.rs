// SPDX-License-Identifier: AGPL-3.0-only
//! Errors returned by the QMP client.

use std::io;
use std::path::PathBuf;
use std::time::Duration;

/// A typed failure from the QMP client.
///
/// Callers generally fall back to another input/power/capture backend when a
/// QMP operation fails. [`QmpError::Command`] means QEMU answered but rejected
/// the command; every other variant means the operation could not be delivered
/// (or the answer was not understood).
#[derive(Debug, thiserror::Error)]
pub enum QmpError {
    /// A required socket path was empty.
    #[error("QMP socket path for {0} is empty")]
    EmptyPath(&'static str),

    /// `connect_input` was called on a client configured without an input
    /// socket. Single-socket clients send input through the control socket.
    #[error("no separate QMP input socket is configured")]
    NoInputSocket,

    /// The Unix socket could not be opened.
    #[error("failed to connect to QMP socket {path}: {source}")]
    Connect {
        /// Socket that failed.
        path: PathBuf,
        /// Underlying connection error.
        #[source]
        source: io::Error,
    },

    /// The greeting or `qmp_capabilities` exchange did not look like QMP.
    #[error("QMP handshake failed: {0}")]
    Handshake(String),

    /// A command (or a connect/handshake step) exceeded its timeout.
    #[error("QMP command {command:?} timed out after {timeout:?}")]
    Timeout {
        /// Command or step that timed out.
        command: String,
        /// Timeout that was applied.
        timeout: Duration,
    },

    /// The client has no control connection, or a configured input socket is
    /// not connected.
    #[error("QMP client is not connected")]
    NotConnected,

    /// The peer closed the socket while a response was expected.
    #[error("QMP connection closed")]
    ConnectionClosed,

    /// QEMU returned an `{"error": ...}` response.
    #[error("QMP command {command:?} returned an error: {message}")]
    Command {
        /// Command that failed.
        command: String,
        /// Serialized QMP error object.
        message: String,
    },

    /// The peer sent unparseable or unexpected JSON.
    #[error("QMP protocol error: {0}")]
    Protocol(String),

    /// Socket I/O failed.
    #[error("QMP I/O error: {0}")]
    Io(#[from] io::Error),

    /// A filesystem path could not be represented as UTF-8 for QMP.
    #[error("invalid path for QMP command: {0:?}")]
    InvalidPath(PathBuf),

    /// The `getfd` SCM_RIGHTS transfer failed.
    #[error("QMP file-descriptor transfer failed: {0}")]
    Fd(String),
}
