// SPDX-License-Identifier: AGPL-3.0-only
//! JavaScript error construction.
//!
//! Every fallible entry point returns a real `Error` with a stable `code`,
//! the protocol `details` (when present), and an optional `remediation`, so the
//! demo page and embedding applications can show something better than
//! `RuntimeError: unreachable`.

use ozma_protocol::ApiError;
use wasm_bindgen::JsValue;

/// Client-side error code for a malformed offer or transport field.
pub const INVALID_OFFER: &str = "client.invalid_offer";
/// Client-side error code for an input value the protocol cannot represent.
pub const INVALID_INPUT: &str = "client.invalid_input";
/// Client-side error code for a transport failure.
pub const TRANSPORT: &str = "client.transport";
/// Client-side error code for certificate decoding or fingerprint mismatch.
pub const CERTIFICATE: &str = "client.certificate";
/// Client-side error code for an unsupported browser feature.
pub const UNSUPPORTED: &str = "client.unsupported";
/// Client-side error code for a closed session.
pub const DISCONNECTED: &str = "client.disconnected";

fn error_with(kind: &str, code: &str, message: &str) -> js_sys::Error {
    let error = js_sys::Error::new(message);
    error.set_name(kind);
    let _ = js_sys::Reflect::set(
        error.as_ref(),
        &JsValue::from_str("code"),
        &JsValue::from_str(code),
    );
    error
}

/// Build an `Error` whose `name` is `OzmaError` and whose `code` is set.
pub fn client_error(code: &str, message: impl AsRef<str>) -> JsValue {
    error_with("OzmaError", code, message.as_ref()).into()
}

/// Build an `Error` from a protocol [`ApiError`], preserving its fields.
pub fn from_api_error(error: &ApiError) -> JsValue {
    let js = error_with("OzmaError", &error.code, &error.message);
    let object: &JsValue = js.as_ref();
    let _ = js_sys::Reflect::set(
        object,
        &JsValue::from_str("retryable"),
        &JsValue::from_bool(error.retryable),
    );
    if !error.details.is_null() {
        if let Ok(text) = serde_json::to_string(&error.details) {
            let details = js_sys::JSON::parse(&text).unwrap_or_else(|_| JsValue::from_str(&text));
            let _ = js_sys::Reflect::set(object, &JsValue::from_str("details"), &details);
        }
    }
    if let Some(remediation) = &error.remediation {
        let _ = js_sys::Reflect::set(
            object,
            &JsValue::from_str("remediation"),
            &JsValue::from_str(remediation),
        );
    }
    js.into()
}

/// Convert an arbitrary `JsValue` (usually a DOM exception) to a readable
/// `OzmaError`.
pub fn from_js(value: JsValue, code: &str, context: &str) -> JsValue {
    let detail = value
        .as_string()
        .or_else(|| {
            js_sys::Reflect::get(&value, &JsValue::from_str("message"))
                .ok()
                .and_then(|message| message.as_string())
        })
        .unwrap_or_else(|| format!("{value:?}"));
    client_error(code, format!("{context}: {detail}"))
}
