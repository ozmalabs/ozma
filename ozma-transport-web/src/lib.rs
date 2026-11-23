// SPDX-License-Identifier: AGPL-3.0-only
//! Browser WebTransport adapter for Ozma sessions.
//!
//! The node exposes a WebTransport endpoint that carries the same channel
//! framing as the native QUIC adapter: one reliable bidirectional stream for
//! ordered channels, datagrams for unreliable channels, and unidirectional
//! streams for resources. This crate is only built for
//! `wasm32-unknown-unknown`; it wraps the browser's `WebTransport` object and
//! implements [`ozma_client_core::ChannelTransport`].
//!
//! Implemented by the web-transport workstream.

#![cfg(target_arch = "wasm32")]
