// SPDX-License-Identifier: AGPL-3.0-only
//! Structured errors for the client decode path.
//!
//! Video decoding is fed by an unreliable transport, so "this access unit is
//! empty", "this access unit is not valid Annex-B", and "the codec backend
//! failed" have to be distinguishable without parsing strings. Every error
//! carries a stable `decode.*` code and maps onto the same [`ApiError`]
//! envelope the node and controller use.

use ozma_protocol::ApiError;

/// Stable error codes for client-side decoding.
///
/// These follow the protocol's `area.problem` convention. They are deliberately
/// separate from `encoder.*` and `audio.*`: a decode failure is a client-side
/// property of one access unit, not a node capability.
pub mod decode_codes {
    /// The access unit had no bytes.
    pub const EMPTY_ACCESS_UNIT: &str = "decode.empty_access_unit";
    /// The access unit was not a structurally valid Annex-B unit.
    pub const MALFORMED_ACCESS_UNIT: &str = "decode.malformed_access_unit";
    /// A decoded frame could not be represented (zero dimensions, bad planes).
    pub const INVALID_FRAME: &str = "decode.invalid_frame";
    /// The codec backend rejected the access unit.
    pub const BACKEND_FAILED: &str = "decode.failed";
    /// The decoder could not be initialised on this target.
    pub const UNAVAILABLE: &str = "decode.unavailable";
    /// The decoder has already been stopped.
    pub const CLOSED: &str = "decode.closed";
    /// A caller supplied metadata the decoder cannot honour.
    pub const FORMAT_MISMATCH: &str = "decode.format_mismatch";
}

/// Errors produced while decoding video or audio for a client.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    /// The access unit was empty. This is never a valid H.264 unit.
    #[error("the access unit is empty")]
    EmptyAccessUnit,
    /// The access unit was not a structurally valid Annex-B unit.
    #[error("malformed access unit: {message}")]
    Malformed {
        /// What was wrong with the byte stream.
        message: String,
    },
    /// The backend produced a frame that cannot be represented safely.
    #[error("invalid decoded frame: {message}")]
    InvalidFrame {
        /// What was wrong with the decoded output.
        message: String,
    },
    /// The codec backend failed.
    #[error("decoder backend failed: {message}")]
    Backend {
        /// Backend failure text.
        message: String,
    },
    /// The decoder could not be initialised.
    #[error("decoder is unavailable: {message}")]
    Unavailable {
        /// Initialisation failure text.
        message: String,
    },
    /// The decoder has been stopped and cannot be reused.
    #[error("decoder `{id}` is closed")]
    Closed {
        /// Decoder identifier.
        id: String,
    },
    /// The metadata supplied by the caller does not fit the decoder.
    #[error("decoder format mismatch: expected {expected}, got {actual}")]
    FormatMismatch {
        /// What the decoder requires.
        expected: String,
        /// What the caller supplied.
        actual: String,
    },
}

impl DecodeError {
    /// Build a [`DecodeError::Malformed`].
    pub fn malformed(message: impl Into<String>) -> Self {
        Self::Malformed {
            message: message.into(),
        }
    }

    /// Build a [`DecodeError::InvalidFrame`].
    pub fn invalid_frame(message: impl Into<String>) -> Self {
        Self::InvalidFrame {
            message: message.into(),
        }
    }

    /// Build a [`DecodeError::Backend`].
    pub fn backend(message: impl Into<String>) -> Self {
        Self::Backend {
            message: message.into(),
        }
    }

    /// Build a [`DecodeError::Unavailable`].
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::Unavailable {
            message: message.into(),
        }
    }

    /// Build a [`DecodeError::Closed`].
    pub fn closed(id: impl Into<String>) -> Self {
        Self::Closed { id: id.into() }
    }

    /// The stable protocol error code for this failure.
    pub fn code(&self) -> &'static str {
        match self {
            Self::EmptyAccessUnit => decode_codes::EMPTY_ACCESS_UNIT,
            Self::Malformed { .. } => decode_codes::MALFORMED_ACCESS_UNIT,
            Self::InvalidFrame { .. } => decode_codes::INVALID_FRAME,
            Self::Backend { .. } => decode_codes::BACKEND_FAILED,
            Self::Unavailable { .. } => decode_codes::UNAVAILABLE,
            Self::Closed { .. } => decode_codes::CLOSED,
            Self::FormatMismatch { .. } => decode_codes::FORMAT_MISMATCH,
        }
    }

    /// Whether retrying the same input could succeed.
    ///
    /// A malformed or empty access unit will fail again; waiting for the next
    /// keyframe will. A closed decoder cannot be retried at all.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Backend { .. })
    }

    /// Convert to the serializable error shape returned by Ozma APIs.
    pub fn to_api_error(&self) -> ApiError {
        match self {
            Self::EmptyAccessUnit => ApiError::new(
                decode_codes::EMPTY_ACCESS_UNIT,
                "The video decoder received an empty access unit.",
            )
            .with_remediation(
                "Drop empty datagrams and wait for a complete access unit before decoding.",
            ),
            Self::Malformed { message } => ApiError::new(
                decode_codes::MALFORMED_ACCESS_UNIT,
                format!("The video decoder received a malformed access unit: {message}"),
            )
            .with_details(message.clone())
            .with_remediation(
                "Start at an H.264 keyframe with SPS/PPS and check transport reassembly.",
            ),
            Self::InvalidFrame { message } => ApiError::new(
                decode_codes::INVALID_FRAME,
                format!("The video decoder produced an invalid frame: {message}"),
            )
            .with_details(message.clone())
            .with_remediation(
                "Treat this as a decode failure and request a fresh keyframe from the node.",
            )
            .retryable(),
            Self::Backend { message } => ApiError::new(
                decode_codes::BACKEND_FAILED,
                format!("The video decoder backend failed: {message}"),
            )
            .with_details(message.clone())
            .with_remediation(
                "Request a new keyframe; if the failure persists, reset the decoder.",
            )
            .retryable(),
            Self::Unavailable { message } => ApiError::new(
                decode_codes::UNAVAILABLE,
                format!("No H.264 decoder is available on this client: {message}"),
            )
            .with_details(message.clone())
            .with_remediation(
                "Install a build with the bundled OpenH264 decoder, or use a client with hardware decode.",
            ),
            Self::Closed { id } => ApiError::new(
                decode_codes::CLOSED,
                format!("The video decoder `{id}` is closed."),
            )
            .with_details(id.clone())
            .with_remediation("Create a new decoder for a new stream."),
            Self::FormatMismatch { expected, actual } => ApiError::new(
                decode_codes::FORMAT_MISMATCH,
                format!("Video decoder expected {expected}, got {actual}."),
            )
            .with_details(format!("{expected} -> {actual}"))
            .with_remediation("Negotiate one codec and pixel layout before decoding."),
        }
    }
}

impl From<DecodeError> for ApiError {
    fn from(error: DecodeError) -> Self {
        error.to_api_error()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_variant_has_a_decode_code() {
        let errors = [
            DecodeError::EmptyAccessUnit,
            DecodeError::malformed("no start code"),
            DecodeError::invalid_frame("bad plane"),
            DecodeError::backend("native error"),
            DecodeError::unavailable("no library"),
            DecodeError::closed("h264"),
            DecodeError::FormatMismatch {
                expected: "h264".to_owned(),
                actual: "hevc".to_owned(),
            },
        ];
        for error in errors {
            assert!(
                error.code().starts_with("decode."),
                "{} did not use a decode.* code",
                error.code()
            );
        }
    }

    #[test]
    fn malformed_units_are_not_retryable_and_backend_errors_are() {
        assert!(!DecodeError::malformed("no start code").is_retryable());
        assert!(DecodeError::backend("native error").is_retryable());
    }

    #[test]
    fn api_error_keeps_the_code_and_details() {
        let api = DecodeError::malformed("missing SPS").to_api_error();
        assert_eq!(api.code, decode_codes::MALFORMED_ACCESS_UNIT);
        assert_eq!(api.details.as_str(), Some("missing SPS"));
        assert!(api.remediation.is_some());
    }
}
