// SPDX-License-Identifier: AGPL-3.0-only
//! Events produced by receiving from a [`crate::SessionTransport`].

use bytes::Bytes;
use ozma_protocol::ApiError;

use crate::channel::ChannelId;

/// Why a transport stopped carrying payloads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CloseReason {
    /// Local or peer shutdown with no error.
    Normal,
    /// This endpoint called `close`.
    LocalShutdown,
    /// The peer closed the connection.
    PeerClosed,
    /// A handshake, idle, or operation timeout expired.
    Timeout,
    /// The underlying network connection failed.
    TransportError,
    /// The peer violated framing or the channel map.
    ProtocolError,
}

impl CloseReason {
    /// Stable lower-case label suitable for logs and diagnostics.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::LocalShutdown => "local_shutdown",
            Self::PeerClosed => "peer_closed",
            Self::Timeout => "timeout",
            Self::TransportError => "transport_error",
            Self::ProtocolError => "protocol_error",
        }
    }

    /// Whether this close should be surfaced as an error to callers.
    pub const fn is_error(self) -> bool {
        !matches!(self, Self::Normal | Self::PeerClosed | Self::LocalShutdown)
    }
}

/// One item read from a transport.
///
/// Reliable channels produce [`TransportEvent::Payload`]; channels negotiated
/// as unreliable produce [`TransportEvent::Datagram`]. Receivers should treat
/// both identically unless they need delivery guarantees. `Error` events are
/// non-terminal diagnostics; `Closed` is terminal and is returned again on any
/// later `recv`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransportEvent {
    /// A reliable, ordered payload from a channel.
    Payload {
        /// Channel that carried the payload.
        channel: ChannelId,
        /// Opaque payload bytes; the channel's protocol interprets them.
        payload: Bytes,
    },
    /// An unreliable, possibly out-of-order or missing payload.
    Datagram {
        /// Channel that carried the payload.
        channel: ChannelId,
        /// Opaque payload bytes.
        payload: Bytes,
    },
    /// The transport is finished. This is terminal.
    Closed {
        /// Why the transport finished.
        reason: CloseReason,
    },
    /// A non-terminal transport or protocol error.
    Error {
        /// Structured error, safe to return over the API.
        error: ApiError,
    },
}

impl TransportEvent {
    /// Channel for payload/datagram events.
    pub fn channel(&self) -> Option<ChannelId> {
        match self {
            Self::Payload { channel, .. } | Self::Datagram { channel, .. } => Some(*channel),
            Self::Closed { .. } | Self::Error { .. } => None,
        }
    }

    /// Payload bytes for payload/datagram events.
    pub fn payload(&self) -> Option<&Bytes> {
        match self {
            Self::Payload { payload, .. } | Self::Datagram { payload, .. } => Some(payload),
            Self::Closed { .. } | Self::Error { .. } => None,
        }
    }

    /// True for the terminal `Closed` event.
    pub fn is_closed(&self) -> bool {
        matches!(self, Self::Closed { .. })
    }

    /// True when the event carries bytes.
    pub fn is_data(&self) -> bool {
        matches!(self, Self::Payload { .. } | Self::Datagram { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_expose_channel_and_payload() {
        let event = TransportEvent::Payload {
            channel: ChannelId::CONTROL,
            payload: Bytes::from_static(b"hello"),
        };
        assert_eq!(event.channel(), Some(ChannelId::CONTROL));
        assert_eq!(event.payload().unwrap().as_ref(), b"hello");
        assert!(event.is_data());
        assert!(!event.is_closed());

        let closed = TransportEvent::Closed {
            reason: CloseReason::PeerClosed,
        };
        assert!(closed.is_closed());
        assert!(!closed.is_data());
        assert!(!CloseReason::PeerClosed.is_error());
    }
}
