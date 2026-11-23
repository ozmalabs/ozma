// SPDX-License-Identifier: AGPL-3.0-only
//! The transport contract implemented by every adapter.

use async_trait::async_trait;
use bytes::Bytes;
use ozma_protocol::{Result, TransportMode};

use crate::channel::{ChannelId, ChannelMap};
use crate::event::{CloseReason, TransportEvent};
use crate::metrics::TransportMetrics;

/// A channel-oriented session connection.
///
/// Implementations must:
/// - preserve ordering **within** a reliable channel;
/// - never reorder reliable channels relative to each other beyond what the
///   underlying transport defines (loopback and QUIC control share one stream);
/// - treat an unknown channel or an oversized payload as a typed error rather
///   than silently dropping data;
/// - return [`TransportEvent::Closed`] once and on every subsequent `recv`;
/// - make `metrics` safe to call while `recv` is pending.
///
/// The trait is object-safe, so a session can hold `Box<dyn SessionTransport>`
/// and switch from loopback to QUIC without changing the media pipeline.
#[async_trait]
pub trait SessionTransport: Send + Sync {
    /// The adapter kind, for diagnostics and selection.
    fn mode(&self) -> TransportMode;

    /// The negotiated channels this transport carries.
    fn channel_map(&self) -> &ChannelMap;

    /// Send one payload on a channel.
    ///
    /// Reliable channels use the transport's ordered stream; unreliable
    /// channels use datagrams and may be lost or reordered.
    async fn send(&self, channel: ChannelId, payload: Bytes) -> Result<()>;

    /// Receive the next event. Waits until data, an error, or close arrives.
    async fn recv(&mut self) -> TransportEvent;

    /// Close with an explicit reason. Idempotent.
    async fn close_with_reason(&self, reason: CloseReason) -> Result<()>;

    /// Close normally. Idempotent.
    async fn close(&self) -> Result<()> {
        self.close_with_reason(CloseReason::Normal).await
    }

    /// A consistent snapshot of this endpoint's counters.
    fn metrics(&self) -> TransportMetrics;

    /// Whether this endpoint has been closed or observed close.
    fn is_closed(&self) -> bool {
        self.metrics().closed
    }
}
