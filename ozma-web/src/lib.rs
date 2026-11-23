// SPDX-License-Identifier: AGPL-3.0-only
//! Rust/WebAssembly Ozma client.
//!
//! The browser client is the native Ozma client compiled to wasm32: it uses
//! the same protocol crate, the same channel framing, and the same video
//! reassembler as the desktop client. Only the edges differ — the browser
//! provides WebTransport for the session, WebCodecs for H.264/Opus decode,
//! canvas for presentation, and Web Audio for output.
//!
//! - [`input`] maps DOM key/pointer events to protocol input and is pure Rust,
//!   so its table is unit-tested on the host.
//! - `client` is the `#[wasm_bindgen]` facade (`OzmaClient`).
//! - `video`, `canvas`, and `audio` are the WebCodecs/Web Audio presentation
//!   layer.
//!
//! The crate compiles on both wasm32 (the real client) and the host, so the
//! pure input mapping and certificate helpers can be tested with
//! `cargo test -p ozma-web`.

pub mod input;
pub mod pin;

#[cfg(target_arch = "wasm32")]
mod audio;
#[cfg(target_arch = "wasm32")]
mod canvas;
#[cfg(target_arch = "wasm32")]
mod client;
#[cfg(target_arch = "wasm32")]
mod error;
#[cfg(target_arch = "wasm32")]
mod stats;
#[cfg(target_arch = "wasm32")]
mod video;

#[cfg(target_arch = "wasm32")]
pub use client::OzmaClient;

/// Translate a `KeyboardEvent.code` value to a USB HID usage id.
///
/// Exported for the demo page and for embedders that prefer to reuse the Rust
/// table instead of duplicating it in JavaScript. Returns `undefined` for keys
/// without a stable HID usage.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen::prelude::wasm_bindgen(js_name = jsCodeToHid)]
pub fn js_code_to_hid(code: &str) -> Option<u8> {
    input::js_code_to_hid(code)
}

/// Install the panic hook once. wasm-bindgen runs this at module init.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen::prelude::wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
}
