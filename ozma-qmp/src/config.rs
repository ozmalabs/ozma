// SPDX-License-Identifier: AGPL-3.0-only
//! Client configuration.

use std::path::PathBuf;
use std::time::Duration;

use crate::backoff::{BACKOFF_FACTOR, BACKOFF_INITIAL, BACKOFF_MAX};
use crate::error::QmpError;

/// Default timeout for connecting and for the greeting/capabilities
/// handshake. Matches the Python client's 3 second waits.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// Default timeout for a control command response. Matches the Python
/// client's 5 second per-response wait.
pub const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

/// QMP endpoints and timeouts.
///
/// `control_path` is required. When `input_path` is set the client opens two
/// independent sockets (the recommended QEMU setup); otherwise it uses the
/// legacy single-socket mode and sends input through the control socket.
#[derive(Debug, Clone)]
pub struct QmpConfig {
    /// Control QMP socket (`-qmp unix:/run/vm-ctrl.qmp,server,nowait`).
    pub control_path: PathBuf,
    /// Optional dedicated input QMP socket
    /// (`-qmp unix:/run/vm-input.qmp,server,nowait`).
    pub input_path: Option<PathBuf>,
    /// Timeout for opening a socket and completing the QMP handshake.
    pub connect_timeout: Duration,
    /// Timeout for each control command write/response.
    pub command_timeout: Duration,
    /// First reconnect delay.
    pub backoff_initial: Duration,
    /// Upper bound on reconnect delay.
    pub backoff_max: Duration,
    /// Reconnect delay multiplier.
    pub backoff_factor: f32,
}

impl Default for QmpConfig {
    fn default() -> Self {
        Self {
            control_path: PathBuf::new(),
            input_path: None,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            command_timeout: DEFAULT_COMMAND_TIMEOUT,
            backoff_initial: BACKOFF_INITIAL,
            backoff_max: BACKOFF_MAX,
            backoff_factor: BACKOFF_FACTOR,
        }
    }
}

impl QmpConfig {
    /// Create a single-socket configuration.
    pub fn new(control_path: impl Into<PathBuf>) -> Self {
        Self {
            control_path: control_path.into(),
            ..Self::default()
        }
    }

    /// Create a dual-socket configuration with a dedicated input socket.
    pub fn dual(control_path: impl Into<PathBuf>, input_path: impl Into<PathBuf>) -> Self {
        Self {
            control_path: control_path.into(),
            input_path: Some(input_path.into()),
            ..Self::default()
        }
    }

    /// Add a dedicated input socket (switches the client to dual mode).
    pub fn with_input_path(mut self, input_path: impl Into<PathBuf>) -> Self {
        self.input_path = Some(input_path.into());
        self
    }

    /// Override the connect/handshake timeout.
    pub fn with_connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }

    /// Override the control command timeout.
    pub fn with_command_timeout(mut self, timeout: Duration) -> Self {
        self.command_timeout = timeout;
        self
    }

    /// Override the reconnect backoff schedule.
    pub fn with_backoff(mut self, initial: Duration, max: Duration, factor: f32) -> Self {
        self.backoff_initial = initial;
        self.backoff_max = max;
        self.backoff_factor = factor;
        self
    }

    /// True when a dedicated input socket is configured.
    pub fn is_dual(&self) -> bool {
        self.input_path.is_some()
    }

    /// Delay for a zero-based reconnect attempt under this configuration.
    pub fn backoff_delay(&self, attempt: u32) -> Duration {
        crate::backoff::backoff_delay(
            attempt,
            self.backoff_initial,
            self.backoff_max,
            self.backoff_factor,
        )
    }

    /// Reject configurations that cannot be connected.
    pub fn validate(&self) -> Result<(), QmpError> {
        if self.control_path.as_os_str().is_empty() {
            return Err(QmpError::EmptyPath("control"));
        }
        if let Some(path) = &self.input_path {
            if path.as_os_str().is_empty() {
                return Err(QmpError::EmptyPath("input"));
            }
        }
        Ok(())
    }
}
