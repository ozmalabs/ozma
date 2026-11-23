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
    .with_remediation(
        "Release a held key before pressing another, or use a non-boot keyboard report.",
    )
}

pub(crate) fn unsupported_event(event: &'static str, message: impl Into<String>) -> ApiError {
    ApiError::new(codes::UNSUPPORTED, message)
        .with_details(json!({ "event": event }))
        .with_remediation(
            "Use a sink that supports this event class, or keep the event on the control channel.",
        )
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

pub(crate) fn unmapped_key(usage: u8) -> ApiError {
    ApiError::new(
        codes::UNSUPPORTED,
        format!("HID usage 0x{usage:02X} has no Linux evdev key mapping"),
    )
    .with_details(json!({ "hid_usage": usage }))
    .with_remediation(
        "Use a HID usage from the page 0x07 subset the uinput sink maps, \
         or keep this key on a sink that supports the full HID report.",
    )
}

pub(crate) fn unmapped_qmp_key(usage: u8) -> ApiError {
    ApiError::new(
        codes::UNSUPPORTED,
        format!("HID usage 0x{usage:02X} has no QEMU qcode mapping"),
    )
    .with_details(json!({ "hid_usage": usage, "sink": "qmp" }))
    .with_remediation(
        "Use a HID usage from the page 0x07 subset ozma-qmp maps, or fall \
         back to the HID gadget sink, which passes the raw usage id.",
    )
}

pub(crate) fn qmp_connect_error(
    config: &ozma_qmp::QmpConfig,
    error: ozma_qmp::QmpError,
) -> ApiError {
    let mut details = json!({
        "sink": "qmp",
        "operation": "connect",
        "control_path": config.control_path.display().to_string(),
        "error": error.to_string(),
    });
    if let Some(input_path) = &config.input_path {
        details["input_path"] = json!(input_path.display().to_string());
    }

    ApiError::new(
        codes::INPUT_UNAVAILABLE,
        format!(
            "Cannot connect to the QMP control socket {}: {error}",
            config.control_path.display()
        ),
    )
    .with_details(details)
    .with_remediation(
        "Check that QEMU is running with -qmp unix:...,server,nowait, that \
         the socket path matches the VM configuration, and that the node user \
         can open it; otherwise fall back to another input sink.",
    )
}

pub(crate) fn qmp_channel_closed() -> ApiError {
    ApiError::new(
        codes::INPUT_UNAVAILABLE,
        "The QMP input task has stopped; input events cannot be queued",
    )
    .with_details(json!({ "sink": "qmp", "operation": "send" }))
    .with_remediation(
        "Restart the QMP input task with QmpSink::spawn, or select another \
         input sink for the session.",
    )
}

#[cfg(target_os = "linux")]
pub(crate) fn uinput_open_error(error: std::io::Error) -> ApiError {
    ApiError::new(
        codes::INPUT_UNAVAILABLE,
        format!("Cannot open Linux uinput device /dev/uinput: {error}"),
    )
    .with_details(json!({
        "path": "/dev/uinput",
        "operation": "open",
        "kind": format!("{:?}", error.kind()),
        "os_error": error.raw_os_error(),
    }))
    .with_remediation(
        "Load the uinput module (modprobe uinput), make sure /dev/uinput exists, \
         and grant the node user write access with a udev rule or the input group.",
    )
}

#[cfg(target_os = "linux")]
pub(crate) fn uinput_write_error(error: std::io::Error) -> ApiError {
    ApiError::new(
        codes::INPUT_WRITE_FAILED,
        format!("Cannot write an input event to the uinput device: {error}"),
    )
    .with_details(json!({
        "path": "/dev/uinput",
        "operation": "write",
        "kind": format!("{:?}", error.kind()),
        "os_error": error.raw_os_error(),
    }))
    .with_remediation(
        "The virtual device may have been removed. Reopen the UinputSink and \
         call release_all before resuming input.",
    )
}

#[cfg(target_os = "linux")]
pub(crate) fn uinput_name_too_long(kind: &'static str, name: &str) -> ApiError {
    ApiError::new(
        codes::INVALID_REQUEST,
        format!(
            "The uinput {kind} name is {} bytes; the kernel limit is {} bytes",
            name.len(),
            crate::uinput::MAX_UINPUT_NAME_LEN,
        ),
    )
    .with_details(json!({
        "kind": kind,
        "name": name,
        "length": name.len(),
        "max_length": crate::uinput::MAX_UINPUT_NAME_LEN,
    }))
    .with_remediation("Use a shorter keyboard_name or pointer_name in UinputOptions.")
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn uinput_unsupported() -> ApiError {
    ApiError::new(
        codes::UNSUPPORTED,
        "The uinput input sink is only available on Linux",
    )
    .with_details(json!({ "platform": std::env::consts::OS }))
    .with_remediation("Use the HID gadget, QMP, or recording sink on this platform.")
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
