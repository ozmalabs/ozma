// SPDX-License-Identifier: AGPL-3.0-only
//! Stable framing codec for the reliable Ozma **Display** channel.
//!
//! Every Display frame is a one-byte message tag followed by a protobuf
//! (`prost`) payload:
//!
//! ```text
//! +---------+----------------------------+
//! | tag (1) | prost payload (0..=8 MiB)  |
//! +---------+----------------------------+
//! ```
//!
//! # No additional length prefix
//!
//! The reliable transport frame already carries the payload length as a
//! big-endian `u32` (`[channel: u8][length: u32 BE][payload]`). The transport
//! length covers the tag and the protobuf payload together, so this codec does
//! **not** add another length prefix. Adding one would be redundant and would
//! create two competing lengths for the same frame.
//!
//! # Stability
//!
//! Tag values are part of the wire contract: they are documented, tested, and
//! must never be reused for a different message. New message kinds receive new
//! tags. The protobuf payloads come from [`ozma_protocol_proto::v1`], whose
//! `.proto` files are the source of truth.
//!
//! Decoding uses proto3 semantics: unknown fields inside a known payload are
//! skipped, which keeps older readers compatible with newer writers. An
//! unknown *tag* is rejected because it cannot be associated with a Rust type.
//!
//! # Limits
//!
//! A payload may be at most [`MAX_PAYLOAD_LEN`] (8 MiB). The limit applies to
//! the protobuf payload, excluding the tag, and is enforced on both encode and
//! decode. The transport may impose a smaller bulk limit; this check protects
//! the codec when it is used directly.

#![forbid(unsafe_code)]

use prost::Message;
use std::fmt;

pub mod convert;
pub mod scene_convert;
pub mod session;

pub use convert::ConvertError;
pub use scene_convert::SceneConvertError;
pub use session::{DisplayEvent, DisplaySession, SessionError, DEFAULT_MAX_DEFINITION_BYTES};

/// Re-export of the generated protocol types used by this codec.
pub use ozma_protocol_proto::v1 as proto;

/// Tag for [`DisplayMessage::Definition`].
pub const TAG_DEFINITION: u8 = 0x01;
/// Tag for [`DisplayMessage::DefinitionRef`].
pub const TAG_DEFINITION_REF: u8 = 0x02;
/// Tag for [`DisplayMessage::DefinitionPatch`].
pub const TAG_DEFINITION_PATCH: u8 = 0x03;
/// Tag for [`DisplayMessage::SceneCommit`].
pub const TAG_SCENE_COMMIT: u8 = 0x04;
/// Tag for [`DisplayMessage::DataUpdate`].
pub const TAG_DATA_UPDATE: u8 = 0x05;
/// Tag for [`DisplayMessage::ActionInvoke`].
pub const TAG_ACTION_INVOKE: u8 = 0x06;
/// Tag for [`DisplayMessage::InteractionEvent`].
pub const TAG_INTERACTION_EVENT: u8 = 0x07;
/// Tag for [`DisplayMessage::SceneSnapshot`].
pub const TAG_SCENE_SNAPSHOT: u8 = 0x08;

/// Number of bytes occupied by the message tag.
pub const HEADER_LEN: usize = 1;

/// Maximum allowed protobuf payload length, excluding the tag (8 MiB).
pub const MAX_PAYLOAD_LEN: usize = 8 * 1024 * 1024;

/// Maximum allowed encoded frame length, including the tag.
pub const MAX_FRAME_LEN: usize = HEADER_LEN + MAX_PAYLOAD_LEN;

/// The message kind carried by a Display frame.
///
/// This is the type-level view of the one-byte wire tag and can be used to
/// route a frame before decoding its payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DisplayMessageKind {
    /// A full display definition document and field/action table.
    Definition,
    /// A reference to a cached definition, used during reconnect.
    DefinitionRef,
    /// A structural patch against an existing definition.
    DefinitionPatch,
    /// A scene revision (ordered node operations).
    SceneCommit,
    /// Changed field values for a definition version.
    DataUpdate,
    /// A request to execute an action.
    ActionInvoke,
    /// Pointer, key, semantic, or action interaction from a device.
    InteractionEvent,
    /// A complete, self-contained scene at one revision.
    SceneSnapshot,
}

impl DisplayMessageKind {
    /// Returns the stable wire tag for this message kind.
    pub const fn as_tag(self) -> u8 {
        match self {
            Self::Definition => TAG_DEFINITION,
            Self::DefinitionRef => TAG_DEFINITION_REF,
            Self::DefinitionPatch => TAG_DEFINITION_PATCH,
            Self::SceneCommit => TAG_SCENE_COMMIT,
            Self::DataUpdate => TAG_DATA_UPDATE,
            Self::ActionInvoke => TAG_ACTION_INVOKE,
            Self::InteractionEvent => TAG_INTERACTION_EVENT,
            Self::SceneSnapshot => TAG_SCENE_SNAPSHOT,
        }
    }

    /// Returns the message kind for a wire tag, or `None` if it is unknown.
    pub const fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            TAG_DEFINITION => Some(Self::Definition),
            TAG_DEFINITION_REF => Some(Self::DefinitionRef),
            TAG_DEFINITION_PATCH => Some(Self::DefinitionPatch),
            TAG_SCENE_COMMIT => Some(Self::SceneCommit),
            TAG_DATA_UPDATE => Some(Self::DataUpdate),
            TAG_ACTION_INVOKE => Some(Self::ActionInvoke),
            TAG_INTERACTION_EVENT => Some(Self::InteractionEvent),
            TAG_SCENE_SNAPSHOT => Some(Self::SceneSnapshot),
            _ => None,
        }
    }
}

impl From<DisplayMessageKind> for u8 {
    fn from(kind: DisplayMessageKind) -> Self {
        kind.as_tag()
    }
}

impl TryFrom<u8> for DisplayMessageKind {
    type Error = u8;

    fn try_from(tag: u8) -> Result<Self, Self::Error> {
        Self::from_tag(tag).ok_or(tag)
    }
}

impl fmt::Display for DisplayMessageKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Definition => "definition",
            Self::DefinitionRef => "definition_ref",
            Self::DefinitionPatch => "definition_patch",
            Self::SceneCommit => "scene_commit",
            Self::DataUpdate => "data_update",
            Self::ActionInvoke => "action_invoke",
            Self::InteractionEvent => "interaction_event",
            Self::SceneSnapshot => "scene_snapshot",
        };
        formatter.write_str(name)
    }
}

/// Errors produced while encoding or decoding a Display frame.
#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    /// The frame contained no tag byte.
    #[error("empty display frame: expected a 1-byte message tag")]
    EmptyFrame,

    /// The payload exceeds [`MAX_PAYLOAD_LEN`].
    #[error("display payload is {actual} bytes, exceeding the {limit} byte limit")]
    OversizedPayload {
        /// Configured maximum payload length.
        limit: usize,
        /// Actual payload length.
        actual: usize,
    },

    /// The tag does not map to a known [`DisplayMessageKind`].
    #[error("unknown display message tag 0x{0:02x}")]
    UnknownTag(u8),

    /// The payload could not be encoded.
    #[error("failed to encode {kind} payload")]
    Encode {
        /// Message kind being encoded.
        kind: DisplayMessageKind,
        /// Underlying protobuf error.
        #[source]
        source: prost::EncodeError,
    },

    /// The payload could not be decoded.
    #[error("failed to decode {kind} payload")]
    Decode {
        /// Message kind being decoded.
        kind: DisplayMessageKind,
        /// Underlying protobuf error.
        #[source]
        source: prost::DecodeError,
    },
}

/// A typed message on the reliable Display channel.
#[derive(Debug, Clone, PartialEq)]
pub enum DisplayMessage {
    /// A full definition document with its compiled field and action tables.
    Definition(proto::DisplayDefinition),
    /// A reference to a definition the peer may already have cached.
    DefinitionRef(proto::DefinitionRef),
    /// A JSON Patch (RFC 6902) against an existing definition.
    DefinitionPatch(proto::DefinitionPatch),
    /// An ordered set of scene operations advancing a revision.
    SceneCommit(proto::SceneCommit),
    /// Only changed field values for a definition version.
    DataUpdate(proto::DataUpdate),
    /// A request to execute an action.
    ActionInvoke(proto::ActionInvoke),
    /// A pointer, key, semantic, or action interaction from a device.
    InteractionEvent(proto::InteractionEvent),
    /// A complete, self-contained scene at one revision.
    SceneSnapshot(proto::SceneSnapshot),
}

impl DisplayMessage {
    /// Returns the message kind, and therefore the wire tag, of this value.
    pub fn kind(&self) -> DisplayMessageKind {
        match self {
            Self::Definition(_) => DisplayMessageKind::Definition,
            Self::DefinitionRef(_) => DisplayMessageKind::DefinitionRef,
            Self::DefinitionPatch(_) => DisplayMessageKind::DefinitionPatch,
            Self::SceneCommit(_) => DisplayMessageKind::SceneCommit,
            Self::DataUpdate(_) => DisplayMessageKind::DataUpdate,
            Self::ActionInvoke(_) => DisplayMessageKind::ActionInvoke,
            Self::InteractionEvent(_) => DisplayMessageKind::InteractionEvent,
            Self::SceneSnapshot(_) => DisplayMessageKind::SceneSnapshot,
        }
    }

    /// Returns the stable one-byte tag for this value.
    pub fn tag(&self) -> u8 {
        self.kind().as_tag()
    }

    /// Returns the protobuf payload length, excluding the tag.
    pub fn payload_len(&self) -> usize {
        match self {
            Self::Definition(message) => message.encoded_len(),
            Self::DefinitionRef(message) => message.encoded_len(),
            Self::DefinitionPatch(message) => message.encoded_len(),
            Self::SceneCommit(message) => message.encoded_len(),
            Self::DataUpdate(message) => message.encoded_len(),
            Self::ActionInvoke(message) => message.encoded_len(),
            Self::InteractionEvent(message) => message.encoded_len(),
            Self::SceneSnapshot(message) => message.encoded_len(),
        }
    }

    /// Returns the full encoded frame length: tag plus protobuf payload.
    pub fn encoded_len(&self) -> usize {
        HEADER_LEN + self.payload_len()
    }

    /// Encodes this message as `[tag: u8][protobuf payload]`.
    ///
    /// No length prefix is added; the reliable transport carries the length.
    ///
    /// # Errors
    ///
    /// Returns [`CodecError::OversizedPayload`] when the protobuf payload is
    /// larger than [`MAX_PAYLOAD_LEN`], and [`CodecError::Encode`] if prost
    /// rejects the payload.
    pub fn encode(&self) -> Result<Vec<u8>, CodecError> {
        let payload_len = self.payload_len();
        if payload_len > MAX_PAYLOAD_LEN {
            return Err(CodecError::OversizedPayload {
                limit: MAX_PAYLOAD_LEN,
                actual: payload_len,
            });
        }

        let mut frame = Vec::with_capacity(HEADER_LEN + payload_len);
        frame.push(self.tag());
        self.encode_payload(&mut frame)?;
        Ok(frame)
    }

    /// Decodes a frame produced by [`DisplayMessage::encode`].
    ///
    /// The input is the complete transport frame payload: the one-byte tag
    /// followed by the protobuf payload. Unknown fields inside a known payload
    /// are skipped by proto3 semantics.
    ///
    /// # Errors
    ///
    /// Returns [`CodecError::EmptyFrame`] for an empty slice,
    /// [`CodecError::UnknownTag`] for an unrecognised tag,
    /// [`CodecError::OversizedPayload`] for payloads over
    /// [`MAX_PAYLOAD_LEN`], and [`CodecError::Decode`] for malformed or
    /// truncated protobuf payloads.
    pub fn decode(frame: &[u8]) -> Result<Self, CodecError> {
        let (&tag, payload) = frame.split_first().ok_or(CodecError::EmptyFrame)?;
        if payload.len() > MAX_PAYLOAD_LEN {
            return Err(CodecError::OversizedPayload {
                limit: MAX_PAYLOAD_LEN,
                actual: payload.len(),
            });
        }

        let kind = DisplayMessageKind::from_tag(tag).ok_or(CodecError::UnknownTag(tag))?;
        let message = match kind {
            DisplayMessageKind::Definition => Self::Definition(decode_payload(payload, kind)?),
            DisplayMessageKind::DefinitionRef => {
                Self::DefinitionRef(decode_payload(payload, kind)?)
            }
            DisplayMessageKind::DefinitionPatch => {
                Self::DefinitionPatch(decode_payload(payload, kind)?)
            }
            DisplayMessageKind::SceneCommit => Self::SceneCommit(decode_payload(payload, kind)?),
            DisplayMessageKind::DataUpdate => Self::DataUpdate(decode_payload(payload, kind)?),
            DisplayMessageKind::ActionInvoke => Self::ActionInvoke(decode_payload(payload, kind)?),
            DisplayMessageKind::InteractionEvent => {
                Self::InteractionEvent(decode_payload(payload, kind)?)
            }
            DisplayMessageKind::SceneSnapshot => {
                Self::SceneSnapshot(decode_payload(payload, kind)?)
            }
        };
        Ok(message)
    }

    fn encode_payload(&self, buffer: &mut Vec<u8>) -> Result<(), CodecError> {
        let kind = self.kind();
        match self {
            Self::Definition(message) => encode_payload(message, buffer, kind),
            Self::DefinitionRef(message) => encode_payload(message, buffer, kind),
            Self::DefinitionPatch(message) => encode_payload(message, buffer, kind),
            Self::SceneCommit(message) => encode_payload(message, buffer, kind),
            Self::DataUpdate(message) => encode_payload(message, buffer, kind),
            Self::ActionInvoke(message) => encode_payload(message, buffer, kind),
            Self::InteractionEvent(message) => encode_payload(message, buffer, kind),
            Self::SceneSnapshot(message) => encode_payload(message, buffer, kind),
        }
    }
}

impl From<proto::DisplayDefinition> for DisplayMessage {
    fn from(message: proto::DisplayDefinition) -> Self {
        Self::Definition(message)
    }
}

impl From<proto::DefinitionRef> for DisplayMessage {
    fn from(message: proto::DefinitionRef) -> Self {
        Self::DefinitionRef(message)
    }
}

impl From<proto::DefinitionPatch> for DisplayMessage {
    fn from(message: proto::DefinitionPatch) -> Self {
        Self::DefinitionPatch(message)
    }
}

impl From<proto::SceneCommit> for DisplayMessage {
    fn from(message: proto::SceneCommit) -> Self {
        Self::SceneCommit(message)
    }
}

impl From<proto::DataUpdate> for DisplayMessage {
    fn from(message: proto::DataUpdate) -> Self {
        Self::DataUpdate(message)
    }
}

impl From<proto::ActionInvoke> for DisplayMessage {
    fn from(message: proto::ActionInvoke) -> Self {
        Self::ActionInvoke(message)
    }
}

impl From<proto::InteractionEvent> for DisplayMessage {
    fn from(message: proto::InteractionEvent) -> Self {
        Self::InteractionEvent(message)
    }
}

impl From<proto::SceneSnapshot> for DisplayMessage {
    fn from(message: proto::SceneSnapshot) -> Self {
        Self::SceneSnapshot(message)
    }
}

fn encode_payload<M: Message>(
    message: &M,
    buffer: &mut Vec<u8>,
    kind: DisplayMessageKind,
) -> Result<(), CodecError> {
    message
        .encode(buffer)
        .map_err(|source| CodecError::Encode { kind, source })
}

fn decode_payload<M: Message + Default>(
    payload: &[u8],
    kind: DisplayMessageKind,
) -> Result<M, CodecError> {
    M::decode(payload).map_err(|source| CodecError::Decode { kind, source })
}
