// SPDX-License-Identifier: AGPL-3.0-only
//! HTTP error conversion. `ApiError` lives in `ozma-protocol`; this wrapper
//! adds the axum response mapping without violating the orphan rule.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use ozma_protocol::error::codes;
use ozma_protocol::{ApiError, ErrorResponse};

/// An API error ready to be returned from an axum handler.
#[derive(Clone, Debug)]
pub struct ApiFailure(pub ApiError);

impl ApiFailure {
    pub fn new(error: ApiError) -> Self {
        Self(error)
    }

    pub fn status_for(error: &ApiError) -> StatusCode {
        match error.code.as_str() {
            codes::INVALID_REQUEST
            | codes::PROTOCOL_VERSION_MISMATCH
            | codes::DISPLAY_MODE_REJECTED => StatusCode::BAD_REQUEST,
            codes::UNAUTHORIZED | codes::TOKEN_EXPIRED | codes::TOKEN_INVALID => {
                StatusCode::UNAUTHORIZED
            }
            codes::FORBIDDEN | codes::CONSENT_DENIED => StatusCode::FORBIDDEN,
            codes::NOT_FOUND => StatusCode::NOT_FOUND,
            codes::CONFLICT | codes::SESSION_STATE | codes::SESSION_NOT_READY => {
                StatusCode::CONFLICT
            }
            codes::UNSUPPORTED
            | codes::DISPLAY_UNSUPPORTED
            | codes::ENCODER_UNAVAILABLE
            | codes::TRANSPORT_UNAVAILABLE => StatusCode::NOT_IMPLEMENTED,
            codes::CONSENT_REQUIRED => StatusCode::PRECONDITION_REQUIRED,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl From<ApiError> for ApiFailure {
    fn from(error: ApiError) -> Self {
        Self(error)
    }
}

impl IntoResponse for ApiFailure {
    fn into_response(self) -> Response {
        let status = Self::status_for(&self.0);
        (status, Json(ErrorResponse::from(self.0))).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_known_codes() {
        assert_eq!(
            ApiFailure::status_for(&ApiError::new(codes::NOT_FOUND, "no")),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            ApiFailure::status_for(&ApiError::new(codes::CONSENT_DENIED, "no")),
            StatusCode::FORBIDDEN
        );
    }
}
