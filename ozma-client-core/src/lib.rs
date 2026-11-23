// SPDX-License-Identifier: AGPL-3.0-only
//! Platform-neutral Ozma client logic.
//!
//! This crate is the seam between the wire protocol and an execution
//! environment. It contains no sockets, no threads, and no platform APIs, so
//! the same code compiles for native targets and for `wasm32-unknown-unknown`.
//!
//! - [`framing`] encodes and decodes the channel-multiplexed byte stream used
//!   by every reliable transport adapter.
//! - [`video`] reassembles fragmented video access units from datagrams.
//! - [`transport`] defines the small async transport contract a client driver
//!   needs; adapters live in the platform-specific crates.
//! - [`session`] is the platform-neutral session driver: handshake, control
//!   messages, media reassembly, input, and close.

#![forbid(unsafe_code)]

pub mod framing;
pub mod session;
pub mod transport;
pub mod video;

pub use framing::{encode_datagram, encode_reliable, FrameDecoder, FRAME_HEADER_LEN};
pub use session::{
    channel_for_kind, channel_map_for, channels_for_kind, webtransport_descriptor, ClientDriver,
    ClientEvent, InputLatencySample, MediaPacket,
};
pub use transport::{ChannelTransport, TransportError};
pub use video::{VideoAssembler, VideoAssemblyError, VideoFrame};
