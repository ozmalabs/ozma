// SPDX-License-Identifier: AGPL-3.0-only
//! Deterministic in-process transport pair for tests and local bring-up.
//!
//! The pair uses bounded `tokio::sync::mpsc` queues, so it applies real
//! backpressure and preserves FIFO ordering in both directions. That is
//! stronger than the per-channel ordering the trait requires, which makes
//! ordering tests unambiguous.

use async_trait::async_trait;
use bytes::Bytes;
use ozma_protocol::{ApiError, Result, TransportMode};
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::{mpsc, watch};

use crate::channel::{ChannelId, ChannelMap};
use crate::error::channel_closed;
use crate::event::{CloseReason, TransportEvent};
use crate::metrics::{Metrics, TransportMetrics};
use crate::transport::SessionTransport;

/// Configuration for a loopback pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoopbackConfig {
    /// Maximum queued messages in each direction. Values below 1 are treated
    /// as 1.
    pub capacity: usize,
}

impl Default for LoopbackConfig {
    fn default() -> Self {
        Self { capacity: 64 }
    }
}

impl LoopbackConfig {
    /// A configuration with the given bounded queue capacity.
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
        }
    }
}

#[derive(Debug)]
struct Envelope {
    channel: ChannelId,
    payload: Bytes,
    datagram: bool,
}

/// One endpoint of an in-process, bidirectional transport pair.
#[derive(Debug)]
pub struct LoopbackTransport {
    map: ChannelMap,
    tx: mpsc::Sender<Envelope>,
    rx: mpsc::Receiver<Envelope>,
    /// Notifies the peer that this endpoint closed.
    close_tx: watch::Sender<Option<CloseReason>>,
    /// Observes the peer closing.
    close_rx: watch::Receiver<Option<CloseReason>>,
    local_closed: AtomicBool,
    peer_gone: bool,
    recv_closed: Option<CloseReason>,
    metrics: Metrics,
}

impl LoopbackTransport {
    /// Create a connected pair with the default channel map and the given
    /// queue capacity.
    pub fn pair(capacity: usize) -> (Self, Self) {
        Self::pair_with_config(LoopbackConfig::new(capacity), ChannelMap::default_session())
    }

    /// Create a connected pair with an explicit configuration and channel map.
    pub fn pair_with_config(config: LoopbackConfig, map: ChannelMap) -> (Self, Self) {
        let capacity = config.capacity.max(1);
        let (a_tx, b_rx) = mpsc::channel(capacity);
        let (b_tx, a_rx) = mpsc::channel(capacity);

        // `a_close` is A's announcement observed by B, and vice versa.
        let (a_close_tx, a_close_rx) = watch::channel(None);
        let (b_close_tx, b_close_rx) = watch::channel(None);

        let a = Self {
            map: map.clone(),
            tx: a_tx,
            rx: a_rx,
            close_tx: a_close_tx,
            close_rx: b_close_rx,
            local_closed: AtomicBool::new(false),
            peer_gone: false,
            recv_closed: None,
            metrics: Metrics::default(),
        };
        let b = Self {
            map,
            tx: b_tx,
            rx: b_rx,
            close_tx: b_close_tx,
            close_rx: a_close_rx,
            local_closed: AtomicBool::new(false),
            peer_gone: false,
            recv_closed: None,
            metrics: Metrics::default(),
        };
        (a, b)
    }

    fn peer_close_reason(&self) -> Option<CloseReason> {
        *self.close_rx.borrow()
    }

    fn closed_error(&self, channel: Option<ChannelId>) -> ApiError {
        let reason = if self.local_closed.load(Ordering::Relaxed) {
            CloseReason::LocalShutdown
        } else {
            self.peer_close_reason()
                .unwrap_or(CloseReason::TransportError)
        };
        channel_closed(channel, reason)
    }

    fn deliver(&self, envelope: Envelope) -> TransportEvent {
        self.metrics
            .record_received(envelope.payload.len(), envelope.datagram);
        if envelope.datagram {
            TransportEvent::Datagram {
                channel: envelope.channel,
                payload: envelope.payload,
            }
        } else {
            TransportEvent::Payload {
                channel: envelope.channel,
                payload: envelope.payload,
            }
        }
    }

    fn finish_recv(&mut self, reason: CloseReason) -> TransportEvent {
        self.recv_closed = Some(reason);
        self.metrics.mark_closed();
        TransportEvent::Closed { reason }
    }
}

#[async_trait]
impl SessionTransport for LoopbackTransport {
    fn mode(&self) -> TransportMode {
        TransportMode::Loopback
    }

    fn channel_map(&self) -> &ChannelMap {
        &self.map
    }

    async fn send(&self, channel: ChannelId, payload: Bytes) -> Result<()> {
        if self.local_closed.load(Ordering::Relaxed) || self.peer_gone {
            self.metrics.record_send_error();
            return Err(self.closed_error(Some(channel)));
        }
        if self.peer_close_reason().is_some() {
            self.metrics.record_send_error();
            return Err(self.closed_error(Some(channel)));
        }

        let datagram = !self.map.require(channel)?.reliable;
        self.map.check_payload(channel, payload.len())?;

        let len = payload.len();
        self.tx
            .send(Envelope {
                channel,
                payload,
                datagram,
            })
            .await
            .map_err(|_| {
                self.metrics.record_send_error();
                self.closed_error(Some(channel))
            })?;
        self.metrics.record_sent(len, datagram);
        Ok(())
    }

    async fn recv(&mut self) -> TransportEvent {
        if let Some(reason) = self.recv_closed {
            return TransportEvent::Closed { reason };
        }

        loop {
            // Drain queued payloads before reporting a close, so messages sent
            // before `close` are never lost.
            match self.rx.try_recv() {
                Ok(envelope) => return self.deliver(envelope),
                Err(TryRecvError::Disconnected) => {
                    let reason = self.peer_close_reason().unwrap_or(CloseReason::PeerClosed);
                    return self.finish_recv(reason);
                }
                Err(TryRecvError::Empty) => {}
            }

            if self.peer_gone {
                return self.finish_recv(CloseReason::PeerClosed);
            }
            if let Some(reason) = self.peer_close_reason() {
                return self.finish_recv(reason);
            }

            tokio::select! {
                biased;
                received = self.rx.recv() => {
                    match received {
                        Some(envelope) => return self.deliver(envelope),
                        None => {
                            self.peer_gone = true;
                            continue;
                        }
                    }
                }
                changed = self.close_rx.changed() => {
                    if changed.is_err() {
                        // The peer endpoint was dropped without an explicit
                        // close; treat it as a peer close after draining.
                        self.peer_gone = true;
                    }
                    continue;
                }
            }
        }
    }

    async fn close_with_reason(&self, reason: CloseReason) -> Result<()> {
        if self.local_closed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let _ = self.close_tx.send(Some(reason));
        self.metrics.mark_closed();
        Ok(())
    }

    fn metrics(&self) -> TransportMetrics {
        let mut snapshot = self.metrics.snapshot(TransportMode::Loopback);
        snapshot.closed |= self.recv_closed.is_some();
        snapshot
    }
}

/// Convenience alias for a boxed pair, useful in tests that swap adapters.
pub type BoxedLoopback = (Box<dyn SessionTransport>, Box<dyn SessionTransport>);

/// Box both ends of a default pair.
pub fn boxed_pair(capacity: usize) -> BoxedLoopback {
    let (a, b) = LoopbackTransport::pair(capacity);
    (Box::new(a), Box::new(b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ozma_protocol::codes;
    use std::time::Duration;
    use tokio::time::timeout;

    async fn recv_within(transport: &mut LoopbackTransport, limit: Duration) -> TransportEvent {
        timeout(limit, transport.recv())
            .await
            .expect("recv timed out")
    }

    #[tokio::test]
    async fn preserves_order_and_channel_identity() {
        let (a, mut b) = LoopbackTransport::pair(32);
        for i in 0..20u8 {
            let channel = if i % 2 == 0 {
                ChannelId::CONTROL
            } else {
                ChannelId::INPUT
            };
            a.send(channel, Bytes::from(vec![i])).await.unwrap();
        }

        let mut control = Vec::new();
        let mut input = Vec::new();
        for _ in 0..20 {
            match recv_within(&mut b, Duration::from_secs(1)).await {
                TransportEvent::Payload { channel, payload } => {
                    let value = payload[0];
                    // Even sends used control, odd used input.
                    assert_eq!(value % 2 == 0, channel == ChannelId::CONTROL);
                    if channel == ChannelId::CONTROL {
                        control.push(value);
                    } else {
                        input.push(value);
                    }
                }
                other => panic!("unexpected event: {other:?}"),
            }
        }
        assert_eq!(
            control,
            (0..20u8).filter(|v| v % 2 == 0).collect::<Vec<_>>()
        );
        assert_eq!(input, (0..20u8).filter(|v| v % 2 == 1).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn unreliable_channels_produce_datagrams() {
        let (a, mut b) = LoopbackTransport::pair(8);
        a.send(ChannelId::VIDEO, Bytes::from_static(b"frame"))
            .await
            .unwrap();
        // Control is reliable.
        a.send(ChannelId::CONTROL, Bytes::from_static(b"hello"))
            .await
            .unwrap();

        let first = recv_within(&mut b, Duration::from_secs(1)).await;
        assert!(matches!(
            first,
            TransportEvent::Datagram {
                channel: ChannelId::VIDEO,
                ..
            }
        ));
        let second = recv_within(&mut b, Duration::from_secs(1)).await;
        assert!(matches!(
            second,
            TransportEvent::Payload {
                channel: ChannelId::CONTROL,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn close_is_delivered_after_queued_payloads_and_is_idempotent() {
        let (a, mut b) = LoopbackTransport::pair(8);
        a.send(ChannelId::CONTROL, Bytes::from_static(b"last"))
            .await
            .unwrap();
        a.close_with_reason(CloseReason::Normal).await.unwrap();
        // Closing again is harmless.
        a.close().await.unwrap();

        assert!(matches!(
            recv_within(&mut b, Duration::from_secs(1)).await,
            TransportEvent::Payload { .. }
        ));
        assert_eq!(
            recv_within(&mut b, Duration::from_secs(1)).await,
            TransportEvent::Closed {
                reason: CloseReason::Normal
            }
        );
        // Terminal state is sticky.
        assert_eq!(
            recv_within(&mut b, Duration::from_secs(1)).await,
            TransportEvent::Closed {
                reason: CloseReason::Normal
            }
        );
        assert!(b.is_closed());
    }

    #[tokio::test]
    async fn send_after_close_or_unknown_channel_is_a_typed_error() {
        let (a, b) = LoopbackTransport::pair(4);
        a.close().await.unwrap();
        let error = a
            .send(ChannelId::CONTROL, Bytes::from_static(b"x"))
            .await
            .unwrap_err();
        assert_eq!(error.code, codes::TRANSPORT_UNAVAILABLE);

        // The open peer also refuses to send once it observes the close.
        let error = b
            .send(ChannelId::CONTROL, Bytes::from_static(b"x"))
            .await
            .unwrap_err();
        assert_eq!(error.code, codes::TRANSPORT_UNAVAILABLE);

        let (c, _d) = LoopbackTransport::pair(4);
        let error = c
            .send(ChannelId::new(42), Bytes::from_static(b"x"))
            .await
            .unwrap_err();
        assert_eq!(error.code, codes::INVALID_REQUEST);
    }

    #[tokio::test]
    async fn metrics_count_both_directions() {
        let (a, mut b) = LoopbackTransport::pair(8);
        a.send(ChannelId::CONTROL, Bytes::from_static(b"1234"))
            .await
            .unwrap();
        a.send(ChannelId::VIDEO, Bytes::from_static(b"ab"))
            .await
            .unwrap();
        let _ = recv_within(&mut b, Duration::from_secs(1)).await;
        let _ = recv_within(&mut b, Duration::from_secs(1)).await;

        let am = a.metrics();
        assert_eq!(am.mode, TransportMode::Loopback);
        assert_eq!(am.payloads_sent, 1);
        assert_eq!(am.datagrams_sent, 1);
        assert_eq!(am.bytes_sent, 6);

        let bm = b.metrics();
        assert_eq!(bm.payloads_received, 1);
        assert_eq!(bm.datagrams_received, 1);
        assert_eq!(bm.bytes_received, 6);
    }

    #[tokio::test]
    async fn bounded_queue_applies_backpressure() {
        let (a, mut b) = LoopbackTransport::pair(1);
        a.send(ChannelId::CONTROL, Bytes::from_static(b"1"))
            .await
            .unwrap();
        // The queue is full, so the second send must not complete yet.
        let blocked = timeout(
            Duration::from_millis(50),
            a.send(ChannelId::CONTROL, Bytes::from_static(b"2")),
        )
        .await;
        assert!(blocked.is_err(), "send should be backpressured");

        // Draining one message unblocks it.
        let _ = recv_within(&mut b, Duration::from_secs(1)).await;
        timeout(
            Duration::from_secs(1),
            a.send(ChannelId::CONTROL, Bytes::from_static(b"2")),
        )
        .await
        .expect("send did not unblock")
        .unwrap();
    }

    #[tokio::test]
    async fn boxed_pair_works_as_trait_objects() {
        let (a, mut b) = boxed_pair(4);
        a.send(ChannelId::CONTROL, Bytes::from_static(b"boxed"))
            .await
            .unwrap();
        let event = timeout(Duration::from_secs(1), b.recv())
            .await
            .expect("recv timed out");
        assert_eq!(event.payload().map(|p| p.as_ref()), Some(b"boxed".as_ref()));
    }
}
