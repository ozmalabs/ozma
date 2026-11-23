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
    SceneCommit { commit: SceneCommit },
    Input { event: InputEvent },
    Event { event: SessionEvent },
    Ping { nonce: u64 },
    Pong { nonce: u64 },
    Error { error: ApiError },
    Bye { reason: EndReason },
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
}
