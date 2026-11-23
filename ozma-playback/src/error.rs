// SPDX-License-Identifier: AGPL-3.0-only
//! Typed playback errors.
//!
//! Playback happens on the client, so these errors do not go through the node's
//! protocol error envelope by default. They follow the same `area.problem` code
//! convention so a client can surface a stable code and a recovery hint without
//! parsing strings.

/// Stable error codes for client-side playback.
pub mod playback_codes {
    /// A decoded block does not match the sink's fixed format.
    pub const PLAYBACK_FORMAT_MISMATCH: &str = "playback.format_mismatch";
    /// A decoded audio block is structurally invalid.
    pub const PLAYBACK_INVALID_AUDIO: &str = "playback.invalid_audio";
    /// The audio device does not exist or cannot be configured.
    pub const PLAYBACK_DEVICE_UNAVAILABLE: &str = "playback.device_unavailable";
    /// The audio device failed after the stream started.
    pub const PLAYBACK_DEVICE_FAILED: &str = "playback.device_failed";
    /// A sink is being used before it was opened, or after it was stopped.
    pub const PLAYBACK_NOT_OPEN: &str = "playback.not_open";
    /// The sink is still busy with earlier work.
    pub const PLAYBACK_BUSY: &str = "playback.busy";
    /// A bounded playback queue rejected a frame.
    pub const PLAYBACK_QUEUE_FULL: &str = "playback.queue_full";
    /// The engine or sink has been stopped.
    pub const PLAYBACK_STOPPED: &str = "playback.stopped";
    /// A file or directory could not be written.
    pub const PLAYBACK_IO: &str = "playback.io";
    /// A window could not be opened or was closed.
    pub const PLAYBACK_WINDOW_UNAVAILABLE: &str = "playback.window_unavailable";
    /// A playback worker thread could not be started or panicked.
    pub const PLAYBACK_WORKER_FAILED: &str = "playback.worker_failed";
}

/// Errors produced by playback sinks and the playback engine.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PlaybackError {
    /// A decoded block does not match the sink's fixed format.
    #[error("playback format mismatch: expected {expected}, got {actual}")]
    FormatMismatch {
        /// Format the sink requires.
        expected: String,
        /// Format the caller supplied.
        actual: String,
    },
    /// A decoded audio block is structurally invalid.
    #[error("invalid audio block: {message}")]
    InvalidAudio {
        /// Why the block was rejected.
        message: String,
    },
    /// The audio device does not exist or cannot be configured.
    #[error("audio device `{device}` is unavailable: {message}")]
    DeviceUnavailable {
        /// ALSA PCM name, for example `default` or `hw:1,0`.
        device: String,
        /// Backend failure text.
        message: String,
        /// Operator-facing next step.
        remediation: String,
    },
    /// The audio device failed after the stream started.
    #[error("audio device `{device}` failed: {message}")]
    DeviceFailed {
        /// ALSA PCM name.
        device: String,
        /// Backend failure text.
        message: String,
    },
    /// A sink is being used before it was opened, or after it was stopped.
    #[error("{sink} is not open")]
    NotOpen {
        /// Sink identifier.
        sink: String,
    },
    /// The sink is still busy with earlier work.
    #[error("{sink} is busy: {message}")]
    Busy {
        /// Sink identifier.
        sink: String,
        /// Why the operation could not complete in time.
        message: String,
    },
    /// A bounded playback queue rejected a frame.
    #[error("playback queue is full (capacity {capacity})")]
    QueueFull {
        /// Capacity of the rejecting queue.
        capacity: usize,
    },
    /// The engine or sink has been stopped.
    #[error("playback is stopped")]
    Stopped,
    /// A file or directory could not be written.
    #[error("cannot write `{path}`: {message}")]
    Io {
        /// Path involved in the failure.
        path: String,
        /// Underlying I/O text.
        message: String,
    },
    /// A window could not be opened or was closed.
    #[error("window sink is unavailable: {message}")]
    WindowUnavailable {
        /// Why the window is unavailable.
        message: String,
    },
    /// A playback worker thread could not be started or panicked.
    #[error("playback worker failed: {message}")]
    Worker {
        /// Failure text.
        message: String,
    },
}

impl PlaybackError {
    /// Build a [`PlaybackError::FormatMismatch`].
    pub fn format_mismatch(expected: impl Into<String>, actual: impl Into<String>) -> Self {
        Self::FormatMismatch {
            expected: expected.into(),
            actual: actual.into(),
        }
    }

    /// Build a [`PlaybackError::InvalidAudio`].
    pub fn invalid_audio(message: impl Into<String>) -> Self {
        Self::InvalidAudio {
            message: message.into(),
        }
    }

    /// Build a [`PlaybackError::DeviceUnavailable`].
    pub fn device_unavailable(
        device: impl Into<String>,
        message: impl Into<String>,
        remediation: impl Into<String>,
    ) -> Self {
        Self::DeviceUnavailable {
            device: device.into(),
            message: message.into(),
            remediation: remediation.into(),
        }
    }

    /// Build a [`PlaybackError::DeviceFailed`].
    pub fn device_failed(device: impl Into<String>, message: impl Into<String>) -> Self {
        Self::DeviceFailed {
            device: device.into(),
            message: message.into(),
        }
    }

    /// Build a [`PlaybackError::NotOpen`].
    pub fn not_open(sink: impl Into<String>) -> Self {
        Self::NotOpen { sink: sink.into() }
    }

    /// Build a [`PlaybackError::Busy`].
    pub fn busy(sink: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Busy {
            sink: sink.into(),
            message: message.into(),
        }
    }

    /// Build a [`PlaybackError::Io`].
    pub fn io(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Io {
            path: path.into(),
            message: message.into(),
        }
    }

    /// Build a [`PlaybackError::WindowUnavailable`].
    pub fn window_unavailable(message: impl Into<String>) -> Self {
        Self::WindowUnavailable {
            message: message.into(),
        }
    }

    /// Build a [`PlaybackError::Worker`].
    pub fn worker(message: impl Into<String>) -> Self {
        Self::Worker {
            message: message.into(),
        }
    }

    /// The stable `area.problem` code for this error.
    pub fn code(&self) -> &'static str {
        use playback_codes::*;
        match self {
            Self::FormatMismatch { .. } => PLAYBACK_FORMAT_MISMATCH,
            Self::InvalidAudio { .. } => PLAYBACK_INVALID_AUDIO,
            Self::DeviceUnavailable { .. } => PLAYBACK_DEVICE_UNAVAILABLE,
            Self::DeviceFailed { .. } => PLAYBACK_DEVICE_FAILED,
            Self::NotOpen { .. } => PLAYBACK_NOT_OPEN,
            Self::Busy { .. } => PLAYBACK_BUSY,
            Self::QueueFull { .. } => PLAYBACK_QUEUE_FULL,
            Self::Stopped => PLAYBACK_STOPPED,
            Self::Io { .. } => PLAYBACK_IO,
            Self::WindowUnavailable { .. } => PLAYBACK_WINDOW_UNAVAILABLE,
            Self::Worker { .. } => PLAYBACK_WORKER_FAILED,
        }
    }

    /// Whether retrying the same operation could succeed.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Busy { .. } | Self::QueueFull { .. } | Self::DeviceFailed { .. }
        )
    }

    /// An operator-facing next step, when one exists.
    pub fn remediation(&self) -> Option<&str> {
        match self {
            Self::DeviceUnavailable { remediation, .. } => Some(remediation),
            Self::DeviceFailed { .. } => {
                Some("Check the audio device is still connected, then reopen the sink.")
            }
            Self::FormatMismatch { .. } => {
                Some("Decode to the sink format: 48 kHz stereo S16 for audio.")
            }
            Self::WindowUnavailable { .. } => {
                Some("Run with a display server, or use a headless video sink instead.")
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_namespaced_and_stable() {
        assert_eq!(
            PlaybackError::format_mismatch("48 kHz", "44.1 kHz").code(),
            playback_codes::PLAYBACK_FORMAT_MISMATCH
        );
        assert_eq!(
            PlaybackError::device_unavailable("hw:9,0", "No such device", "Check it").code(),
            playback_codes::PLAYBACK_DEVICE_UNAVAILABLE
        );
        assert_eq!(
            PlaybackError::Stopped.code(),
            playback_codes::PLAYBACK_STOPPED
        );
    }

    #[test]
    fn device_errors_carry_remediation() {
        let error = PlaybackError::device_unavailable("hw:9,0", "No such device", "Check it");
        assert_eq!(error.remediation(), Some("Check it"));
        assert!(!error.is_retryable());
        assert!(PlaybackError::device_failed("default", "underrun").is_retryable());
    }
}
