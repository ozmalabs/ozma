//! UDP wire format for ozma mesh packets.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Packet type tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum PacketKind {
    Keyboard = 0x01,
    Mouse    = 0x02,
    Ping     = 0x10,
    Pong     = 0x11,
}

/// Top-level framing envelope carried over UDP.
///
/// The payload is encrypted by `ozma-crypto` before transmission;
/// this struct represents the *plaintext* inner frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OzmaPacket {
    /// Monotonically increasing sequence number (wraps at u64::MAX).
    pub seq: u64,
    /// Packet type.
    pub kind: PacketKind,
    /// Raw payload bytes (serialised HID report or control message).
    pub payload: Vec<u8>,
}

/// Errors that can occur when encoding/decoding wire packets.
#[derive(Debug, Error)]
pub enum WireError {
    #[error("serialisation error: {0}")]
    Serialise(#[from] serde_json::Error),
    #[error("buffer too short: need {need} bytes, got {got}")]
    BufferTooShort { need: usize, got: usize },
}

impl OzmaPacket {
    /// Encode the packet to a `Vec<u8>` (JSON for now; swap for bincode later).
    pub fn encode(&self) -> Result<Vec<u8>, WireError> {
        Ok(serde_json::to_vec(self)?)
    }

    /// Decode a packet from raw bytes.
    pub fn decode(buf: &[u8]) -> Result<Self, WireError> {
        Ok(serde_json::from_slice(buf)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let pkt = OzmaPacket {
            seq: 42,
            kind: PacketKind::Ping,
            payload: vec![1, 2, 3],
        };
        let encoded = pkt.encode().unwrap();
        let decoded = OzmaPacket::decode(&encoded).unwrap();
        assert_eq!(decoded.seq, 42);
        assert_eq!(decoded.kind, PacketKind::Ping);
        assert_eq!(decoded.payload, vec![1, 2, 3]);
    }
}
