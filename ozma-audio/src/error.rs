// SPDX-License-Identifier: AGPL-3.0-only
//! Typed audio errors and their mapping onto the protocol's [`ApiError`].
//!
//! The audio path uses [`AudioError`] internally so callers can distinguish
//! "no device" from "device failed mid-stream" from "the encoder was handed a
//! frame it cannot encode" without parsing strings. [`AudioError::to_api_error`]
//! produces the stable envelope every Ozma adapter returns.

use ozma_protocol::ApiError;

/// Namespaced error codes for the server-side audio path.
///
/// `ozma-protocol` owns the cross-cutting codes; these are the audio-specific
/// ones, following the same `area.problem` convention as `capture.*` and
/// `encoder.*`.
pub mod audio_codes {
    /// No usable ALSA capture device, or the device could not be configured.
    pub const AUDIO_UNAVAILABLE: &str = "audio.unavailable";
    /// A capture device failed after the stream had started.
    pub const AUDIO_CAPTURE_FAILED: &str = "audio.capture_failed";
    /// A frame does not match the encoder's declared audio format.
    pub const AUDIO_FORMAT_MISMATCH: &str = "audio.format_mismatch";
    /// A frame is structurally invalid (empty, ragged channel data, ...).
    pub const AUDIO_INVALID_FRAME: &str = "audio.invalid_frame";
    /// The Opus encoder backend failed.
    pub const AUDIO_ENCODE_FAILED: &str = "audio.encode_failed";
    /// The Opus decoder backend failed.
    pub const AUDIO_DECODE_FAILED: &str = "audio.decode_failed";
    /// The source has already been stopped.
    pub const AUDIO_CLOSED: &str = "audio.closed";
    /// The source produced its last frame.
    pub const AUDIO_END_OF_STREAM: &str = "audio.end_of_stream";
}

/// Errors produced by audio sources and encoders.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AudioError {
    /// No usable capture device, or it could not be configured.
    #[error("audio device `{device}` is unavailable: {message}")]
    Unavailable {
        /// ALSA device string, for example `default` or `hw:1,0`.
        device: String,
        /// Backend failure text.
        message: String,
        /// Operator-facing next step.
        remediation: String,
    },
    /// A capture device failed after the stream started.
    #[error("audio capture on `{device}` failed: {message}")]
    Capture {
        /// ALSA device string.
        device: String,
        /// Backend failure text.
        message: String,
    },
    /// The source has already been stopped.
    #[error("audio source `{id}` is closed")]
    Closed {
        /// Identifier of the closed source.
        id: String,
    },
    /// The source produced its last frame. This is a normal shutdown signal.
    #[error("end of audio stream")]
    EndOfStream,
    /// A frame is structurally invalid.
    #[error("invalid audio frame: {0}")]
    InvalidFrame(String),
    /// The frame does not match the encoder contract.
    #[error("audio format mismatch: expected {expected}, got {actual}")]
    FormatMismatch {
        /// Format the encoder required.
        expected: String,
        /// What the frame actually carried.
        actual: String,
    },
    /// The encoder backend failed.
    #[error("audio encoder `{encoder}` failed: {message}")]
    Encode {
        /// Identifier of the encoder.
        encoder: String,
        /// Backend failure text.
        message: String,
    },
    /// The decoder backend failed.
    #[error("audio decoder failed: {message}")]
    Decode {
        /// Backend failure text.
        message: String,
    },
    /// A channel closed while a stage still expected it.
    #[error("audio channel closed")]
    ChannelClosed,
}

impl AudioError {
    /// Build an [`AudioError::Unavailable`] with standard device remediation.
    pub fn unavailable(
        device: impl Into<String>,
        message: impl Into<String>,
        remediation: impl Into<String>,
    ) -> Self {
        Self::Unavailable {
            device: device.into(),
            message: message.into(),
            remediation: remediation.into(),
        }
    }

    /// Build an [`AudioError::Capture`].
    pub fn capture(device: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Capture {
            device: device.into(),
            message: message.into(),
        }
    }

    /// Build an [`AudioError::Encode`].
    pub fn encode(encoder: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Encode {
            encoder: encoder.into(),
            message: message.into(),
        }
    }

    /// Convert to the serializable error shape returned by the node API.
    pub fn to_api_error(&self) -> ApiError {
        match self {
            Self::Unavailable {
                device,
                message,
                remediation,
            } => ApiError::new(
                audio_codes::AUDIO_UNAVAILABLE,
                format!("Audio device `{device}` is unavailable: {message}"),
            )
            .with_details(device.clone())
            .with_remediation(remediation.clone()),
            Self::Capture { device, message } => ApiError::new(
                audio_codes::AUDIO_CAPTURE_FAILED,
                format!("Audio capture on `{device}` failed: {message}"),
            )
            .with_details(device.clone())
            .with_remediation("Check the device is still connected and retry the audio track.")
            .retryable(),
            Self::Closed { id } => ApiError::new(
                audio_codes::AUDIO_CLOSED,
                format!("The audio source `{id}` is closed."),
            )
            .with_details(id.clone())
            .with_remediation("Open the source again or select another audio device."),
            Self::EndOfStream => ApiError::new(
                audio_codes::AUDIO_END_OF_STREAM,
                "The audio source reached the end of its stream.",
            ),
            Self::InvalidFrame(message) => {
                ApiError::new(audio_codes::AUDIO_INVALID_FRAME, message.clone())
            }
            Self::FormatMismatch { expected, actual } => ApiError::new(
                audio_codes::AUDIO_FORMAT_MISMATCH,
                format!("Audio frame mismatch: encoder expected {expected}, got {actual}."),
            )
            .with_details(format!("{expected} -> {actual}"))
            .with_remediation(
                "Negotiate sample rate, channel count, and frame duration before encoding.",
            ),
            Self::Encode { encoder, message } => {
                ApiError::new(audio_codes::AUDIO_ENCODE_FAILED, message.clone())
                    .with_details(encoder.clone())
                    .retryable()
            }
            Self::Decode { message } => {
                ApiError::new(audio_codes::AUDIO_DECODE_FAILED, message.clone())
            }
            Self::ChannelClosed => ApiError::new(
                audio_codes::AUDIO_CAPTURE_FAILED,
                "The audio capture channel closed unexpectedly.",
            ),
        }
    }
}

impl From<AudioError> for ApiError {
    fn from(error: AudioError) -> Self {
        error.to_api_error()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_maps_to_a_stable_code_with_device_details() {
        let error = AudioError::unavailable("hw:9,0", "No such device", "Check the cable.");
        let api = error.to_api_error();
        assert_eq!(api.code, audio_codes::AUDIO_UNAVAILABLE);
        assert_eq!(api.details.as_str(), Some("hw:9,0"));
        assert_eq!(api.remediation.as_deref(), Some("Check the cable."));
        assert!(!api.retryable);
    }

    #[test]
    fn capture_failures_are_retryable() {
        let api = AudioError::capture("default", "overrun").to_api_error();
        assert_eq!(api.code, audio_codes::AUDIO_CAPTURE_FAILED);
        assert!(api.retryable);
    }

    #[test]
    fn format_mismatch_keeps_both_descriptions() {
        let error = AudioError::FormatMismatch {
            expected: "opus 48000 Hz 2 ch 960 samples".to_owned(),
            actual: "opus 44100 Hz 1 ch 512 samples".to_owned(),
        };
        let api = error.to_api_error();
        assert_eq!(api.code, audio_codes::AUDIO_FORMAT_MISMATCH);
        assert!(api.details.as_str().unwrap().contains("44100"));
    }
}
