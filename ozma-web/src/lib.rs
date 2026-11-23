// SPDX-License-Identifier: AGPL-3.0-only
//! Rust/WebAssembly Ozma client.
//!
//! The browser client is the native Ozma client compiled to wasm32: it uses
//! the same protocol crate, the same channel framing, and the same video
//! reassembler as the desktop client. Only the edges differ — the browser
//! provides WebTransport for the session, WebCodecs for H.264/Opus decode,
//! canvas for presentation, and Web Audio for output.
//!
//! Implemented by the web-client workstream.

#![cfg(target_arch = "wasm32")]
