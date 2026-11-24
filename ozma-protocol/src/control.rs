// SPDX-License-Identifier: AGPL-3.0-only
//! Control messages carried on the reliable `control` channel of a native
//! session. Media frames travel on video/audio channels; these messages set up
//! and supervise the session.

use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::ids::{SessionId, TrackId};
use crate::input::InputEvent;
use crate::scene::{DisplayCapabilities, DisplayProfile, SceneCommit};
use crate::session::{EndReason, Session, SessionEvent};
use crate::transport::WireFormat;

/// A JSON control frame on the reliable channel.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ControlMessage {
    /// First message from a client. The token carries the client identity and
    /// scoped permissions; the server must not trust a client id sent beside it.
    Hello {
        session_id: SessionId,
        token: String,
        /// Encodings the client supports, in preference order. A peer that
        /// predates negotiation omits the field, which parses as an empty list
        /// and selects the JSON compatibility path.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        wire_formats: Vec<WireFormat>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display: Option<DisplayCapabilities>,
    },
    HelloAck {
        session: Session,
        /// Encoding selected for the rest of the connection. Missing in older
        /// JSON payloads, which default to [`WireFormat::Json`].
        #[serde(default)]
        wire_format: WireFormat,
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
    /// Client -> node: begin sending browser camera and/or microphone media.
    CaptureStart {
        video: bool,
        audio: bool,
    },
    /// Client -> node: stop sending browser camera and/or microphone media.
    CaptureStop {
        video: bool,
        audio: bool,
    },
    /// Client -> node: the capture pipeline's state and negotiated video
    /// metadata. Optional fields default to `None` when the peer omits them.
    CaptureState {
        video: bool,
        audio: bool,
        #[serde(default)]
        video_codec: Option<String>,
        #[serde(default)]
        width: Option<u16>,
        #[serde(default)]
        height: Option<u16>,
        #[serde(default)]
        error: Option<ApiError>,
    },
    /// Ask the peer to produce an IDR/keyframe. Clients send this when their
    /// decoder loses sync; nodes may send it to a browser publisher.
    KeyframeRequest {
        #[serde(default)]
        track: Option<TrackId>,
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
    use crate::ids::ClientId;
    use crate::session::SessionSpec;

    fn session() -> Session {
        Session::new(
            SessionId::new("session-test"),
            SessionSpec {
                client_id: ClientId::new("client-test"),
                principal: "alice".into(),
                tracks: Vec::new(),
                audio: Default::default(),
                input: Default::default(),
                privacy: Default::default(),
                transport: Default::default(),
                ttl_seconds: 60,
            },
            Default::default(),
            1,
        )
    }

    #[test]
    fn hello_carries_wire_format_preferences() {
        let message = ControlMessage::Hello {
            session_id: SessionId::new("session-test"),
            token: "token-test".into(),
            wire_formats: vec![WireFormat::Protobuf, WireFormat::Json],
            display: None,
        };
        let encoded = serde_json::to_value(&message).unwrap();
        assert_eq!(
            encoded,
            serde_json::json!({
                "type": "hello",
                "data": {
                    "session_id": "session-test",
                    "token": "token-test",
                    "wire_formats": ["protobuf", "json"],
                },
            })
        );
        assert_eq!(
            serde_json::from_value::<ControlMessage>(encoded).unwrap(),
            message
        );
    }

    #[test]
    fn legacy_hello_without_wire_formats_parses_as_json() {
        let decoded: ControlMessage =
            serde_json::from_str(r#"{"type":"hello","data":{"session_id":"s","token":"t"}}"#)
                .unwrap();
        assert_eq!(
            decoded,
            ControlMessage::Hello {
                session_id: SessionId::new("s"),
                token: "t".into(),
                wire_formats: Vec::new(),
                display: None,
            }
        );
    }

    #[test]
    fn legacy_hello_ack_without_wire_format_parses_as_json() {
        let session = session();
        let decoded: ControlMessage = serde_json::from_value(serde_json::json!({
            "type": "hello_ack",
            "data": { "session": session },
        }))
        .unwrap();
        assert_eq!(
            decoded,
            ControlMessage::HelloAck {
                session,
                wire_format: WireFormat::Json,
                display: None,
            }
        );
    }

    #[test]
    fn hello_ack_carries_the_selected_wire_format() {
        let message = ControlMessage::HelloAck {
            session: session(),
            wire_format: WireFormat::Protobuf,
            display: None,
        };
        let encoded = serde_json::to_value(&message).unwrap();
        assert_eq!(encoded["data"]["wire_format"], "protobuf");
        assert_eq!(
            serde_json::from_value::<ControlMessage>(encoded).unwrap(),
            message
        );
    }

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

    #[test]
    fn browser_capture_messages_roundtrip() {
        let messages = [
            ControlMessage::CaptureStart {
                video: true,
                audio: false,
            },
            ControlMessage::CaptureStop {
                video: false,
                audio: true,
            },
            ControlMessage::CaptureState {
                video: true,
                audio: true,
                video_codec: Some("h264".to_owned()),
                width: Some(1280),
                height: Some(720),
                error: None,
            },
            ControlMessage::KeyframeRequest {
                track: Some(TrackId::new("video0")),
            },
        ];
        for message in messages {
            let json = serde_json::to_string(&message).unwrap();
            assert_eq!(
                serde_json::from_str::<ControlMessage>(&json).unwrap(),
                message,
                "wire form: {json}"
            );
        }
    }

    #[test]
    fn browser_capture_wire_shapes_are_stable() {
        assert_eq!(
            serde_json::to_value(ControlMessage::CaptureStart {
                video: true,
                audio: false,
            })
            .unwrap(),
            serde_json::json!({
                "type": "capture_start",
                "data": { "video": true, "audio": false },
            })
        );
        assert_eq!(
            serde_json::to_value(ControlMessage::KeyframeRequest { track: None }).unwrap(),
            serde_json::json!({
                "type": "keyframe_request",
                "data": { "track": null },
            })
        );
    }

    #[test]
    fn older_clients_without_capture_fields_still_parse() {
        // CaptureState's optional fields default to `None` when a peer built
        // against the earlier shape omits them.
        let decoded: ControlMessage =
            serde_json::from_str(r#"{"type":"capture_state","data":{"video":true,"audio":false}}"#)
                .unwrap();
        assert_eq!(
            decoded,
            ControlMessage::CaptureState {
                video: true,
                audio: false,
                video_codec: None,
                width: None,
                height: None,
                error: None,
            }
        );

        // A KeyframeRequest whose `track` is absent defaults to all tracks.
        let decoded: ControlMessage =
            serde_json::from_str(r#"{"type":"keyframe_request","data":{}}"#).unwrap();
        assert_eq!(decoded, ControlMessage::KeyframeRequest { track: None });
    }
}
