// SPDX-License-Identifier: AGPL-3.0-only
//! Structured API errors. Every failure carries a stable code, a human
//! message, optional details, an optional remediation, and a retry hint.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

/// Stable error codes. Adapters may add their own namespaced codes, but these
/// are the ones the core API guarantees.
pub mod codes {
    pub const INVALID_REQUEST: &str = "invalid_request";
    pub const UNAUTHORIZED: &str = "unauthorized";
    pub const FORBIDDEN: &str = "forbidden";
    pub const NOT_FOUND: &str = "not_found";
    pub const CONFLICT: &str = "conflict";
    pub const UNSUPPORTED: &str = "unsupported";
    pub const PROTOCOL_VERSION_MISMATCH: &str = "protocol_version_mismatch";
    pub const CONSENT_REQUIRED: &str = "consent_required";
    pub const CONSENT_DENIED: &str = "consent_denied";
    pub const CAPTURE_NO_FRAMES: &str = "capture.no_frames";
    pub const CAPTURE_UNAVAILABLE: &str = "capture.unavailable";
    pub const ENCODER_PROBE_FAILED: &str = "encoder.probe_failed";
    pub const ENCODER_UNAVAILABLE: &str = "encoder.unavailable";
    pub const TRANSPORT_UNAVAILABLE: &str = "transport.unavailable";
    pub const DISPLAY_UNSUPPORTED: &str = "display.unsupported";
    pub const DISPLAY_CREATE_FAILED: &str = "display.create_failed";
    pub const DISPLAY_MODE_REJECTED: &str = "display.mode_rejected";
    pub const SESSION_NOT_READY: &str = "session.not_ready";
    pub const SESSION_STATE: &str = "session.invalid_state";
    pub const TOKEN_EXPIRED: &str = "token.expired";
    pub const TOKEN_INVALID: &str = "token.invalid";
    pub const INTERNAL: &str = "internal";
}

/// A typed, serializable error. `details` is deliberately free-form so a
/// provider can include device paths, modes, or driver diagnostics without
/// changing the outer shape.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiError {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub details: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remediation: Option<String>,
    #[serde(default)]
    pub retryable: bool,
}

impl ApiError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            details: Value::Null,
            remediation: None,
            retryable: false,
        }
    }

    pub fn with_details(mut self, details: impl Into<Value>) -> Self {
        self.details = details.into();
        self
    }

    pub fn with_remediation(mut self, remediation: impl Into<String>) -> Self {
        self.remediation = Some(remediation.into());
        self
    }

    pub fn retryable(mut self) -> Self {
        self.retryable = true;
        self
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(codes::INVALID_REQUEST, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(codes::INTERNAL, message)
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ApiError {}

impl From<String> for ApiError {
    fn from(value: String) -> Self {
        Self::internal(value)
    }
}

impl From<&str> for ApiError {
    fn from(value: &str) -> Self {
        Self::internal(value)
    }
}

/// Canonical error envelope returned by the HTTP/JSON API.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub ok: bool,
    pub error: ApiError,
}

impl From<ApiError> for ErrorResponse {
    fn from(error: ApiError) -> Self {
        Self { ok: false, error }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_shape_is_stable() {
        let error = ApiError::new(codes::CAPTURE_NO_FRAMES, "No frames")
            .with_remediation("Check the HDMI cable")
            .retryable();
        let value = serde_json::to_value(ErrorResponse::from(error)).unwrap();
        assert_eq!(value["ok"], false);
        assert_eq!(value["error"]["code"], "capture.no_frames");
        assert_eq!(value["error"]["retryable"], true);
    }
}
