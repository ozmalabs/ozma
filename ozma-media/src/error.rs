// SPDX-License-Identifier: AGPL-3.0-only
//! Typed media errors and their mapping onto the protocol's [`ApiError`].
//!
//! The pipeline uses [`MediaError`] internally so callers can match on
//! end-of-stream, format mismatches, and backend failures without parsing
//! strings. API-facing code converts it with [`MediaError::to_api_error`] to
//! get the stable error envelope every Ozma adapter uses.

use ozma_protocol::{codes, ApiError};

/// Additional namespaced error codes used by the local media path.
pub mod media_codes {
    /// The frame does not match the encoder/source format contract.
    pub const FORMAT_MISMATCH: &str = "media.format_mismatch";
    /// A frame buffer was empty or structurally invalid.
    pub const INVALID_FRAME: &str = "media.invalid_frame";
    /// A pipeline operation was cancelled by its owner.
    pub const CANCELLED: &str = "media.cancelled";
    /// An internal channel closed unexpectedly.
    pub const CHANNEL_CLOSED: &str = "media.channel_closed";
}

/// Errors produced by capture sources, encoders, and the media pipeline.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MediaError {
    /// The source produced its last frame. This is a normal shutdown signal.
    #[error("end of stream")]
    EndOfStream,
    /// The source has already been stopped.
    #[error("capture source `{id}` is closed")]
    Closed {
        /// Identifier of the closed source.
        id: String,
    },
    /// The operation was cancelled by its owner.
    #[error("media operation cancelled")]
    Cancelled,
    /// A frame buffer must contain at least one byte.
    #[error("frame buffer is empty")]
    EmptyFrame,
    /// The format is structurally invalid, for example zero dimensions.
    #[error("invalid frame format: {0}")]
    InvalidFormat(String),
    /// The source and encoder disagree about the frame format.
    #[error("frame format mismatch: expected {expected}, got {actual}")]
    FormatMismatch {
        /// Format the encoder required.
        expected: String,
        /// Format the source actually produced.
        actual: String,
    },
    /// A capture backend failed.
    #[error("capture source `{id}` failed: {message}")]
    Capture {
        /// Identifier of the source.
        id: String,
        /// Human-readable backend failure.
        message: String,
    },
    /// An encoder backend failed.
    #[error("encoder `{encoder}` failed: {message}")]
    Encode {
        /// Identifier of the encoder.
        encoder: String,
        /// Human-readable backend failure.
        message: String,
    },
    /// A pipeline channel closed while a stage still expected it.
    #[error("media channel closed")]
    ChannelClosed,
    /// A structured protocol error, for example from a provider adapter.
    #[error(transparent)]
    Api(#[from] ApiError),
}

impl MediaError {
    /// Wrap a capture backend failure.
    pub fn capture(id: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Capture {
            id: id.into(),
            message: message.into(),
        }
    }

    /// Wrap an encoder backend failure.
    pub fn encode(encoder: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Encode {
            encoder: encoder.into(),
            message: message.into(),
        }
    }

    /// Convert to the serializable error shape returned by the node API.
    ///
    /// The mapping is intentionally mechanical: callers that need richer
    /// details should return [`ApiError`] directly.
    pub fn to_api_error(&self) -> ApiError {
        match self {
            Self::Api(error) => error.clone(),
            Self::EndOfStream => ApiError::new(
                codes::CAPTURE_NO_FRAMES,
                "The capture source reached the end of its stream.",
            ),
            Self::Closed { id } => ApiError::new(
                codes::CAPTURE_UNAVAILABLE,
                format!("The capture source `{id}` is closed."),
            )
            .with_details(id.clone())
            .with_remediation("Open the source again or select another capture device."),
            Self::Cancelled => ApiError::new(media_codes::CANCELLED, "The media operation was cancelled."),
            Self::EmptyFrame => ApiError::new(
                media_codes::INVALID_FRAME,
                "The media pipeline received an empty frame.",
            )
            .with_remediation("Check the capture backend; an empty frame is never valid."),
            Self::InvalidFormat(message) => {
                ApiError::new(media_codes::INVALID_FRAME, message.clone())
            }
            Self::FormatMismatch { expected, actual } => ApiError::new(
                media_codes::FORMAT_MISMATCH,
                format!("Frame format mismatch: encoder expected {expected}, source produced {actual}."),
            )
            .with_details(format!("{expected} -> {actual}"))
            .with_remediation("Negotiate the capture format before starting the pipeline."),
            Self::Capture { id, message } => {
                ApiError::new(codes::CAPTURE_UNAVAILABLE, message.clone())
                    .with_details(id.clone())
                    .retryable()
            }
            Self::Encode { encoder, message } => {
                ApiError::new(codes::ENCODER_UNAVAILABLE, message.clone())
                    .with_details(encoder.clone())
                    .retryable()
            }
            Self::ChannelClosed => ApiError::new(
                media_codes::CHANNEL_CLOSED,
                "The media pipeline channel closed unexpectedly.",
            ),
        }
    }
}

impl From<MediaError> for ApiError {
    fn from(error: MediaError) -> Self {
        error.to_api_error()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_mismatch_maps_to_a_stable_code() {
        let error = MediaError::FormatMismatch {
            expected: "rgb24 320x240".to_owned(),
            actual: "nv12 320x240".to_owned(),
        };
        let api = ApiError::from(error);
        assert_eq!(api.code, media_codes::FORMAT_MISMATCH);
        assert!(api.message.contains("rgb24 320x240"));
    }

    #[test]
    fn capture_errors_are_retryable() {
        let api = MediaError::capture("fake0", "device unplugged").to_api_error();
        assert_eq!(api.code, codes::CAPTURE_UNAVAILABLE);
        assert!(api.retryable);
    }
}
