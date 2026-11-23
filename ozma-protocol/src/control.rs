// SPDX-License-Identifier: AGPL-3.0-only
//! Control messages carried on the reliable `control` channel of a native
//! session. Media frames travel on video/audio channels; these messages set up
//! and supervise the session.

use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::ids::SessionId;
use crate::input::InputEvent;
use crate::scene::{DisplayCapabilities, DisplayProfile, SceneCommit};
use crate::session::{EndReason, Session, SessionEvent};

/// A JSON control frame on the reliable channel.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ControlMessage {
    /// First message from a client. The token carries the client identity and
    /// scoped permissions; the server must not trust a client id sent beside it.
    Hello {
        session_id: SessionId,
        token: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display: Option<DisplayCapabilities>,
    },
    HelloAck {
        session: Session,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display: Option<DisplayProfile>,
    },
    SceneCommit {
        commit: SceneCommit,
    },
    Input {
        event: InputEvent,
    },
    /// A timed input probe. The node authorizes and applies `event` exactly
    /// like [`ControlMessage::Input`], then replies with
    /// [`ControlMessage::InputLatencyAck`]. This is a diagnostic path; it
    /// measures send-to-client-visible-encoded-frame latency, not display
    /// scanout.
    InputLatency {
        event: InputEvent,
        /// Client wall-clock time at send, in microseconds since the Unix
        /// epoch. Used for diagnostics and only for cross-clock timing when the
        /// two clocks are known to be close.
        client_sent_micros: u64,
        nonce: u64,
    },
    /// Reply to [`ControlMessage::InputLatency`] after the event was accepted.
    /// An unauthorized event receives [`ControlMessage::Error`] instead.
    InputLatencyAck {
        nonce: u64,
        /// Echo of the request timestamp, so a client can match the ack even
        /// when several probes are in flight.
        client_sent_micros: u64,
        /// Server wall-clock time when the probe was received, in microseconds
        /// since the Unix epoch.
        server_received_micros: u64,
        /// Highest encoded video frame sequence the server had sent when it
        /// received the probe. The client waits for a complete access unit at
        /// or above this sequence.
        latest_frame_sequence: u64,
    },
    Event {
        event: SessionEvent,
    },
    Ping {
        nonce: u64,
    },
    Pong {
        nonce: u64,
    },
    Error {
        error: ApiError,
    },
    Bye {
        reason: EndReason,
    },
}

impl ControlMessage {
    pub fn is_hello(&self) -> bool {
        matches!(self, ControlMessage::Hello { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_messages_roundtrip() {
        let message = ControlMessage::Ping { nonce: 42 };
        let json = serde_json::to_string(&message).unwrap();
        let back: ControlMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(back, message);
    }

    #[test]
    fn existing_control_messages_keep_their_wire_shape() {
        let encoded = serde_json::to_value(ControlMessage::Ping { nonce: 7 }).unwrap();
        assert_eq!(
            encoded,
            serde_json::json!({ "type": "ping", "data": { "nonce": 7 } })
        );

        let decoded: ControlMessage =
            serde_json::from_str(r#"{"type":"input","data":{"event":{"type":"release_all"}}}"#)
                .unwrap();
        assert_eq!(
            decoded,
            ControlMessage::Input {
                event: InputEvent::ReleaseAll
            }
        );
    }

    #[test]
    fn input_latency_messages_roundtrip() {
        let message = ControlMessage::InputLatency {
            event: InputEvent::PointerRelative { dx: -12, dy: 34 },
            client_sent_micros: 1_700_000_000_000_123,
            nonce: 99,
        };
        let json = serde_json::to_string(&message).unwrap();
        assert_eq!(
            serde_json::from_str::<ControlMessage>(&json).unwrap(),
            message
        );

        let ack = ControlMessage::InputLatencyAck {
            nonce: 99,
            client_sent_micros: 1_700_000_000_000_123,
            server_received_micros: 1_700_000_000_000_456,
            latest_frame_sequence: 1_234,
        };
        let json = serde_json::to_string(&ack).unwrap();
        assert_eq!(serde_json::from_str::<ControlMessage>(&json).unwrap(), ack);
    }
}
