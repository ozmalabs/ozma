// SPDX-License-Identifier: AGPL-3.0-only
//! Channel-oriented transport adapters for Ozma sessions.
//!
//! An Ozma session is not a socket. It is a set of typed, prioritised
//! [`ChannelId`]s carried by one [`SessionTransport`]. This crate provides the
//! transport contract, the channel map that both peers enforce, a deterministic
//! in-process [`loopback`] pair, the path-selection [`director`], and (behind
//! the default-on `quic` feature) a native QUIC adapter.
//!
//! The protocol vocabulary — [`ozma_protocol::ChannelDescriptor`],
//! [`ozma_protocol::TransportDescriptor`], [`ozma_protocol::SessionOffer`] and
//! [`ozma_protocol::ApiError`] — is owned by `ozma-protocol`; this crate only
//! adapts it to bytes and connections.

#![forbid(unsafe_code)]

pub mod channel;
pub mod director;
pub mod error;
pub mod event;
pub mod loopback;
pub mod metrics;
pub mod offer;
#[cfg(feature = "quic")]
pub mod quic;
pub mod transport;

pub use channel::{ChannelId, ChannelMap};
pub use director::{
    mode_rank, LinkQuality, PathRejection, PathSelection, RejectionReason, SelectionStrategy,
    TransportCandidate, TransportDirector, TransportPolicy,
};
pub use error::{
    channel_closed, channel_unknown, payload_too_large, protocol_error, transport_unavailable,
};
pub use event::{CloseReason, TransportEvent};
pub use loopback::{LoopbackConfig, LoopbackTransport};
pub use metrics::TransportMetrics;
pub use offer::{session_offer, SessionOfferBuilder, TransportDescriptorBuilder};
#[cfg(feature = "quic")]
pub use quic::{QuicClient, QuicConfig, QuicServer, QuicTransport, SelfSignedCertificate, ALPN};
pub use transport::SessionTransport;

/// The protocol ALPN used by native Ozma sessions.
pub const DEFAULT_ALPN: &str = "ozma/1";
