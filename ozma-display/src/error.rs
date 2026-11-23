// SPDX-License-Identifier: AGPL-3.0-only
//! Structured errors shared by the virtual-display providers and the mapping
//! helpers.
//!
//! The protocol guarantees the `display.*` codes used here. Everything is an
//! [`ApiError`] so an API adapter can serialize it without translation.

use ozma_protocol::display::{DisplayId, DisplayMode};
use ozma_protocol::error::codes;
use ozma_protocol::ApiError;
use serde_json::json;

/// A display-local or global coordinate fell outside the display that owns it.
///
/// This is a namespaced adapter code: `ozma-protocol` deliberately only
/// guarantees the core `display.*` codes, and adapters may add their own.
pub const DISPLAY_OUT_OF_BOUNDS: &str = "display.out_of_bounds";

/// The requested platform virtual display is not available.
pub fn unsupported(message: impl Into<String>) -> ApiError {
    ApiError::new(codes::DISPLAY_UNSUPPORTED, message)
}

/// The provider understood the request but could not create the connector.
pub fn create_failed(message: impl Into<String>) -> ApiError {
    ApiError::new(codes::DISPLAY_CREATE_FAILED, message)
}

/// The requested mode is not advertised by the display.
pub fn mode_rejected(id: &DisplayId, mode: &DisplayMode, reason: impl Into<String>) -> ApiError {
    ApiError::new(
        codes::DISPLAY_MODE_REJECTED,
        format!("Display {id} rejected mode {mode}: {}", reason.into()),
    )
    .with_details(json!({
        "display_id": id.as_str(),
        "mode": mode.label(),
    }))
}

/// An operation named a display that the provider does not know.
pub fn unknown_display(id: &DisplayId) -> ApiError {
    ApiError::new(codes::NOT_FOUND, format!("Unknown display {id}"))
        .with_details(json!({ "display_id": id.as_str() }))
        .with_remediation("Refresh the display list and retry with an attached display id")
}

/// A coordinate was outside the display's local surface.
pub fn out_of_bounds(
    id: &DisplayId,
    space: &str,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
) -> ApiError {
    ApiError::new(
        DISPLAY_OUT_OF_BOUNDS,
        format!("{space} coordinate ({x}, {y}) is outside display {id} ({width}x{height})"),
    )
    .with_details(json!({
        "display_id": id.as_str(),
        "space": space,
        "x": x,
        "y": y,
        "width": width,
        "height": height,
    }))
}

/// A proposed topology is not usable.
pub fn invalid_topology(message: impl Into<String>) -> ApiError {
    ApiError::new(codes::INVALID_REQUEST, message)
        .with_details(json!({ "topology": true }))
        .with_remediation("Supply one placement per attached display with a non-zero size")
}
