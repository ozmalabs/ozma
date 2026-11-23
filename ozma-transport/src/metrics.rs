// SPDX-License-Identifier: AGPL-3.0-only
//! Transport counters shared by all adapters.
//!
//! Counters are snapshots, not live handles. They are cheap enough to read on
//! every telemetry tick and intentionally contain no floats so that they can be
//! compared deterministically in tests.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use ozma_protocol::TransportMode;

/// Snapshot of one transport endpoint's counters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransportMetrics {
    /// Transport adapter that produced these counters.
    pub mode: TransportMode,
    /// Reliable payloads accepted by `send`.
    pub payloads_sent: u64,
    /// Reliable payloads delivered by `recv`.
    pub payloads_received: u64,
    /// Unreliable datagrams accepted by `send`.
    pub datagrams_sent: u64,
    /// Unreliable datagrams delivered by `recv`.
    pub datagrams_received: u64,
    /// Payload bytes accepted by `send` (excluding framing).
    pub bytes_sent: u64,
    /// Payload bytes delivered by `recv` (excluding framing).
    pub bytes_received: u64,
    /// Failed `send` calls.
    pub send_errors: u64,
    /// Errors reported by the receive path.
    pub receive_errors: u64,
    /// True if this endpoint is (or has observed) closed.
    pub closed: bool,
}

impl Default for TransportMetrics {
    fn default() -> Self {
        Self {
            mode: TransportMode::Loopback,
            payloads_sent: 0,
            payloads_received: 0,
            datagrams_sent: 0,
            datagrams_received: 0,
            bytes_sent: 0,
            bytes_received: 0,
            send_errors: 0,
            receive_errors: 0,
            closed: false,
        }
    }
}

impl TransportMetrics {
    /// Payloads plus datagrams sent.
    pub fn messages_sent(&self) -> u64 {
        self.payloads_sent + self.datagrams_sent
    }

    /// Payloads plus datagrams received.
    pub fn messages_received(&self) -> u64 {
        self.payloads_received + self.datagrams_received
    }
}

/// Mutable counter set used inside adapters.
#[derive(Debug, Default)]
pub(crate) struct Metrics {
    payloads_sent: AtomicU64,
    payloads_received: AtomicU64,
    datagrams_sent: AtomicU64,
    datagrams_received: AtomicU64,
    bytes_sent: AtomicU64,
    bytes_received: AtomicU64,
    send_errors: AtomicU64,
    receive_errors: AtomicU64,
    closed: AtomicBool,
}

impl Metrics {
    pub(crate) fn record_sent(&self, bytes: usize, datagram: bool) {
        if datagram {
            self.datagrams_sent.fetch_add(1, Ordering::Relaxed);
        } else {
            self.payloads_sent.fetch_add(1, Ordering::Relaxed);
        }
        self.bytes_sent.fetch_add(bytes as u64, Ordering::Relaxed);
    }

    pub(crate) fn record_received(&self, bytes: usize, datagram: bool) {
        if datagram {
            self.datagrams_received.fetch_add(1, Ordering::Relaxed);
        } else {
            self.payloads_received.fetch_add(1, Ordering::Relaxed);
        }
        self.bytes_received
            .fetch_add(bytes as u64, Ordering::Relaxed);
    }

    pub(crate) fn record_send_error(&self) {
        self.send_errors.fetch_add(1, Ordering::Relaxed);
    }

    #[cfg_attr(not(feature = "quic"), allow(dead_code))]
    pub(crate) fn record_receive_error(&self) {
        self.receive_errors.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn mark_closed(&self) {
        self.closed.store(true, Ordering::Relaxed);
    }

    pub(crate) fn snapshot(&self, mode: TransportMode) -> TransportMetrics {
        TransportMetrics {
            mode,
            payloads_sent: self.payloads_sent.load(Ordering::Relaxed),
            payloads_received: self.payloads_received.load(Ordering::Relaxed),
            datagrams_sent: self.datagrams_sent.load(Ordering::Relaxed),
            datagrams_received: self.datagrams_received.load(Ordering::Relaxed),
            bytes_sent: self.bytes_sent.load(Ordering::Relaxed),
            bytes_received: self.bytes_received.load(Ordering::Relaxed),
            send_errors: self.send_errors.load(Ordering::Relaxed),
            receive_errors: self.receive_errors.load(Ordering::Relaxed),
            closed: self.closed.load(Ordering::Relaxed),
        }
    }
}
