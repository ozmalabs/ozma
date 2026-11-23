// SPDX-License-Identifier: AGPL-3.0-only
//! Stable error codes and constructors used by the native client.
//!
//! Every public fallible API returns [`ozma_protocol::ApiError`], so a CLI,
//! a desktop UI, and tests can all match on `error.code` without depending on
//! string messages.

use ozma_protocol::{codes as protocol_codes, ApiError};
use serde_json::json;

/// Client-specific codes. Errors relayed from the controller or the node keep
/// their original protocol code (for example `forbidden` or `not_found`).
pub mod codes {
    /// Invalid CLI/library configuration, including a bad base URL.
    pub const CLIENT_CONFIG: &str = "client.config";
    /// Certificate loading, decoding, or fingerprint mismatch.
    pub const CLIENT_CERTIFICATE: &str = "client.certificate";
    /// The peer sent control data the client could not parse.
    pub const CLIENT_PROTOCOL: &str = "client.protocol";
    /// The controller HTTP API failed.
    pub const CLIENT_CONTROLLER: &str = "client.controller";
    /// An operation timed out.
    pub const CLIENT_TIMEOUT: &str = "client.timeout";
    /// The session transport closed when an operation needed it.
    pub const CLIENT_DISCONNECTED: &str = "client.disconnected";
    /// A requested transport or feature is not available.
    pub const CLIENT_UNSUPPORTED: &str = "client.unsupported";
}

pub(crate) fn config(message: impl Into<String>) -> ApiError {
    ApiError::new(codes::CLIENT_CONFIG, message)
}

pub(crate) fn certificate(message: impl Into<String>) -> ApiError {
    ApiError::new(codes::CLIENT_CERTIFICATE, message)
}

pub(crate) fn protocol(message: impl Into<String>) -> ApiError {
    ApiError::new(codes::CLIENT_PROTOCOL, message)
}

pub(crate) fn controller(message: impl Into<String>) -> ApiError {
    ApiError::new(codes::CLIENT_CONTROLLER, message)
        .with_remediation("Check the controller URL, token, and that the controller is running.")
}

pub(crate) fn timeout(what: &str, duration: std::time::Duration) -> ApiError {
    ApiError::new(
        codes::CLIENT_TIMEOUT,
        format!("Timed out {what} after {} ms", duration.as_millis()),
    )
    .retryable()
}

pub(crate) fn disconnected(reason: impl std::fmt::Display) -> ApiError {
    ApiError::new(
        codes::CLIENT_DISCONNECTED,
        format!("The session transport is closed: {reason}"),
    )
    .with_remediation("Reconnect the session and retry.")
}

/// Map an HTTP status plus optional controller body to the protocol error
/// vocabulary. A structured `{"error": {code, message, ...}}` body is
/// preserved verbatim; a plain `{"detail": "..."}` body becomes a typed error.
pub(crate) fn http_response_error(status: reqwest::StatusCode, body: &[u8]) -> ApiError {
    let code = match status.as_u16() {
        400 => protocol_codes::INVALID_REQUEST,
        401 => protocol_codes::UNAUTHORIZED,
        403 => protocol_codes::FORBIDDEN,
        404 => protocol_codes::NOT_FOUND,
        409 => protocol_codes::CONFLICT,
        422 => protocol_codes::INVALID_REQUEST,
        429 => protocol_codes::CONFLICT,
        503 => protocol_codes::TRANSPORT_UNAVAILABLE,
        _ => codes::CLIENT_CONTROLLER,
    };

    let text = String::from_utf8_lossy(body);
    let trimmed = text.trim();
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
        if let Some(error) = value.get("error") {
            if error.is_object() {
                if let Ok(api) = serde_json::from_value::<ApiError>(error.clone()) {
                    return api;
                }
            }
            if let Some(message) = error.as_str() {
                return ApiError::new(code, message)
                    .with_details(json!({ "status": status.as_u16() }));
            }
        }
        if let Some(detail) = value.get("detail") {
            if let Some(message) = detail.as_str() {
                return ApiError::new(code, message)
                    .with_details(json!({ "status": status.as_u16() }));
            }
            if !detail.is_null() {
                return ApiError::new(code, detail.to_string())
                    .with_details(json!({ "status": status.as_u16() }));
            }
        }
        if let Some(message) = value.get("message").and_then(|value| value.as_str()) {
            return ApiError::new(code, message).with_details(json!({ "status": status.as_u16() }));
        }
    }

    let message = if trimmed.is_empty() {
        format!("The controller returned HTTP {status}")
    } else {
        format!("The controller returned HTTP {status}: {trimmed}")
    };
    ApiError::new(code, message).with_details(json!({ "status": status.as_u16() }))
}

pub(crate) fn transport_error(error: impl std::fmt::Display) -> ApiError {
    ApiError::new(
        protocol_codes::TRANSPORT_UNAVAILABLE,
        format!("Transport request failed: {error}"),
    )
    .retryable()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structured_error_bodies_are_preserved() {
        let body = br#"{"ok":false,"error":{"code":"capture.no_frames","message":"No frames","retryable":true}}"#;
        let error = http_response_error(reqwest::StatusCode::SERVICE_UNAVAILABLE, body);
        assert_eq!(error.code, "capture.no_frames");
        assert!(error.retryable);
    }

    #[test]
    fn detail_bodies_become_typed_errors() {
        let error = http_response_error(
            reqwest::StatusCode::NOT_FOUND,
            br#"{"detail":"Node not found"}"#,
        );
        assert_eq!(error.code, protocol_codes::NOT_FOUND);
        assert_eq!(error.message, "Node not found");
        assert_eq!(error.details["status"], 404);
    }
}
