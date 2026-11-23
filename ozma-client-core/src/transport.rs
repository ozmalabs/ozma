// SPDX-License-Identifier: AGPL-3.0-only
//! The minimal async transport contract a client driver needs.
//!
//! Unlike [`ozma_transport::SessionTransport`], this trait does not require
//! `Send + Sync`: browser transports hold JavaScript handles that are confined
//! to one thread. Native adapters can implement it by delegating to a
//! `SessionTransport`.

use async_trait::async_trait;
use bytes::Bytes;
use ozma_protocol::{Result, TransportMode};
use ozma_transport::{ChannelId, ChannelMap, CloseReason, TransportEvent};

/// A transport failure surfaced to a client driver.
pub type TransportError = ozma_protocol::ApiError;

/// A channel-oriented connection as seen by a client session driver.
///
/// Implementations must preserve ordering within one reliable channel and must
/// keep returning a closed event after the first close.
#[async_trait(?Send)]
pub trait ChannelTransport {
    /// Adapter kind, for diagnostics.
    fn mode(&self) -> TransportMode;

    /// The negotiated channel map for this session.
    fn channel_map(&self) -> &ChannelMap;

    /// Send one payload on a channel.
    async fn send(&self, channel: ChannelId, payload: Bytes) -> Result<()>;

    /// Wait for the next payload, datagram, error, or close.
    async fn recv(&mut self) -> TransportEvent;

    /// Close with a reason. Idempotent.
    async fn close_with_reason(&self, reason: CloseReason) -> Result<()>;
}
