// SPDX-License-Identifier: AGPL-3.0-only
//! Stable error codes and constructors used by the input sinks.

use std::path::Path;

use ozma_protocol::ApiError;
use serde_json::json;

/// Stable error codes understood by the node API.
///
/// The protocol's generic codes are reused where they fit. Input-specific
/// failures get a namespaced `input.*` code so callers can distinguish a
/// missing gadget from an encoder or transport failure.
pub mod codes {
    pub use ozma_protocol::codes::*;

    /// A keyboard or mouse gadget could not be opened.
    pub const INPUT_UNAVAILABLE: &str = "input.unavailable";
    /// A report could not be written to a gadget.
    pub const INPUT_WRITE_FAILED: &str = "input.write_failed";
    /// A seventh key was pressed while the boot keyboard already held six.
    pub const INPUT_KEY_LIMIT: &str = "input.key_limit";
}

pub(crate) fn invalid_key(code: u32) -> ApiError {
    ApiError::new(
        codes::INVALID_REQUEST,
        format!("Key code {code} is not a HID usage id; expected 1..=255"),
    )
    .with_details(json!({ "keycode": code }))
}

pub(crate) fn key_limit(keycode: u8, held: usize) -> ApiError {
    ApiError::new(
        codes::INPUT_KEY_LIMIT,
        format!(
            "The boot keyboard report holds at most {} keys; rejecting key 0x{keycode:02X}",
            crate::state::MAX_KEYS,
        ),
    )
    .with_details(json!({
        "keycode": keycode,
        "held_keys": held,
        "max_keys": crate::state::MAX_KEYS,
    }))
    .with_remediation("Release a held key before pressing another, or use a non-boot keyboard report.")
}

pub(crate) fn unsupported_event(event: &'static str, message: impl Into<String>) -> ApiError {
    ApiError::new(codes::UNSUPPORTED, message)
        .with_details(json!({ "event": event }))
        .with_remediation("Use a sink that supports this event class, or keep the event on the control channel.")
}

pub(crate) fn wrong_mouse_mode(event: &'static str, required: &'static str) -> ApiError {
    ApiError::new(
        codes::UNSUPPORTED,
        format!("{event} requires the {required} mouse report mode"),
    )
    .with_details(json!({
        "event": event,
        "required_mouse_protocol": required,
    }))
    .with_remediation(format!(
        "Open the sink with MouseProtocol::{required} for this session's pointer mode."
    ))
}

pub(crate) fn open_error(path: &Path, error: std::io::Error) -> ApiError {
    ApiError::new(
        codes::INPUT_UNAVAILABLE,
        format!("Cannot open HID gadget {}: {error}", path.display()),
    )
    .with_details(json!({
        "path": path.display().to_string(),
        "operation": "open",
        "kind": format!("{:?}", error.kind()),
        "os_error": error.raw_os_error(),
    }))
    .with_remediation(
        "Check that the USB gadget is configured and the device node exists; \
         gadget device nodes are normally owned by root.",
    )
}

pub(crate) fn write_error(path: &Path, error: std::io::Error) -> ApiError {
    ApiError::new(
        codes::INPUT_WRITE_FAILED,
        format!("Cannot write HID report to {}: {error}", path.display()),
    )
    .with_details(json!({
        "path": path.display().to_string(),
        "operation": "write",
        "kind": format!("{:?}", error.kind()),
        "os_error": error.raw_os_error(),
    }))
}
