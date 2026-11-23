// SPDX-License-Identifier: AGPL-3.0-only
//! Media vocabulary shared by capture, encode, transport, and clients.

use serde::{Deserialize, Serialize};

/// Codecs the protocol can negotiate. `MJPEG`, `Opus`, `AAC`, and PCM are
/// included because they are useful fallbacks/audio formats, not because every
/// node must support every one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Codec {
    H264,
    Hevc,
    Av1,
    Mjpeg,
    Opus,
    Aac,
    #[serde(rename = "pcm_s16le")]
    PcmS16Le,
}

impl Codec {
    pub fn is_video(self) -> bool {
        matches!(self, Codec::H264 | Codec::Hevc | Codec::Av1 | Codec::Mjpeg)
    }

    pub fn is_audio(self) -> bool {
        matches!(self, Codec::Opus | Codec::Aac | Codec::PcmS16Le)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Codec::H264 => "h264",
            Codec::Hevc => "hevc",
            Codec::Av1 => "av1",
            Codec::Mjpeg => "mjpeg",
            Codec::Opus => "opus",
            Codec::Aac => "aac",
            Codec::PcmS16Le => "pcm_s16le",
        }
    }
}

/// Colour/HDR state of a stream. The protocol uses this instead of a bare
/// `hdr: bool` so that metadata can be negotiated without changing the shape
/// of every message.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColorFormat {
    #[default]
    Sdr,
    Hdr10,
    Hdr10Plus,
    DolbyVision,
}

impl ColorFormat {
    pub fn is_hdr(&self) -> bool {
        !matches!(self, ColorFormat::Sdr)
    }
}

/// A video format request or capability.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaFormat {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    #[serde(default)]
    pub color: ColorFormat,
    #[serde(default)]
    pub hdr: bool,
}

impl MediaFormat {
    pub fn new(codec: Codec, width: u32, height: u32, fps: u32, bitrate_kbps: u32) -> Self {
        Self {
            codec,
            width,
            height,
            fps,
            bitrate_kbps,
            color: ColorFormat::Sdr,
            hdr: false,
        }
    }

    pub fn label(&self) -> String {
        format!(
            "{}x{}@{} {} {}kbps",
            self.width,
            self.height,
            self.fps,
            self.codec.as_str(),
            self.bitrate_kbps
        )
    }
}

/// An audio format request or capability.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioFormat {
    pub codec: Codec,
    pub sample_rate: u32,
    pub channels: u8,
    pub bitrate_kbps: u32,
}

impl AudioFormat {
    pub fn opus() -> Self {
        Self {
            codec: Codec::Opus,
            sample_rate: 48_000,
            channels: 2,
            bitrate_kbps: 128,
        }
    }

    pub fn aac() -> Self {
        Self {
            codec: Codec::Aac,
            sample_rate: 48_000,
            channels: 2,
            bitrate_kbps: 128,
        }
    }
}

impl Default for AudioFormat {
    fn default() -> Self {
        Self::opus()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_names_are_stable() {
        assert_eq!(serde_json::to_string(&Codec::H264).unwrap(), "\"h264\"");
        assert_eq!(
            serde_json::to_string(&Codec::PcmS16Le).unwrap(),
            "\"pcm_s16le\""
        );
    }

    #[test]
    fn media_format_roundtrips() {
        let format = MediaFormat::new(Codec::Hevc, 2560, 1440, 120, 20_000);
        let json = serde_json::to_string(&format).unwrap();
        let back: MediaFormat = serde_json::from_str(&json).unwrap();
        assert_eq!(back, format);
    }
}

/// Version of the video fragmentation header used on unreliable datagram
/// channels. A video access unit may exceed one QUIC datagram or UDP packet,
/// so it is split into fragments and reassembled by the client.
pub const VIDEO_FRAGMENT_VERSION: u8 = 1;

/// Header length for a video fragment:
/// `[version u8][flags u8][track u8][frame_sequence u64 LE][index u16 LE][count u16 LE]`.
pub const VIDEO_FRAGMENT_HEADER_LEN: usize = 15;

/// One fragment of an encoded video access unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoFragmentHeader {
    pub version: u8,
    pub keyframe: bool,
    /// Logical track index (0 for a single-track session).
    pub track_id: u8,
    /// Sequence number of the complete access unit.
    pub frame_sequence: u64,
    /// Zero-based fragment index.
    pub index: u16,
    /// Total fragments in this access unit.
    pub count: u16,
}

impl VideoFragmentHeader {
    /// Build the header for `frame_sequence` fragment `index` of `count`.
    pub fn new(keyframe: bool, track_id: u8, frame_sequence: u64, index: u16, count: u16) -> Self {
        Self {
            version: VIDEO_FRAGMENT_VERSION,
            keyframe,
            track_id,
            frame_sequence,
            index,
            count,
        }
    }

    /// Append this header and `payload` to a datagram.
    pub fn encode_into(self, payload: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(VIDEO_FRAGMENT_HEADER_LEN + payload.len());
        bytes.push(self.version);
        bytes.push(u8::from(self.keyframe));
        bytes.push(self.track_id);
        bytes.extend_from_slice(&self.frame_sequence.to_le_bytes());
        bytes.extend_from_slice(&self.index.to_le_bytes());
        bytes.extend_from_slice(&self.count.to_le_bytes());
        bytes.extend_from_slice(payload);
        bytes
    }

    /// Parse a datagram, returning the header and the fragment payload.
    pub fn parse(bytes: &[u8]) -> Option<(Self, &[u8])> {
        if bytes.len() < VIDEO_FRAGMENT_HEADER_LEN {
            return None;
        }
        let version = bytes[0];
        if version != VIDEO_FRAGMENT_VERSION {
            return None;
        }
        let keyframe = bytes[1] & 1 != 0;
        let track_id = bytes[2];
        let frame_sequence = u64::from_le_bytes(bytes[3..11].try_into().ok()?);
        let index = u16::from_le_bytes(bytes[11..13].try_into().ok()?);
        let count = u16::from_le_bytes(bytes[13..15].try_into().ok()?);
        if count == 0 || index >= count {
            return None;
        }
        Some((
            Self {
                version,
                keyframe,
                track_id,
                frame_sequence,
                index,
                count,
            },
            &bytes[VIDEO_FRAGMENT_HEADER_LEN..],
        ))
    }
}

#[cfg(test)]
mod fragment_tests {
    use super::*;

    #[test]
    fn fragment_header_roundtrips() {
        let header = VideoFragmentHeader::new(true, 2, 42, 1, 3);
        let bytes = header.encode_into(b"payload");
        let (parsed, payload) = VideoFragmentHeader::parse(&bytes).unwrap();
        assert_eq!(parsed, header);
        assert_eq!(payload, b"payload");
    }

    #[test]
    fn malformed_fragments_are_rejected() {
        assert!(VideoFragmentHeader::parse(&[]).is_none());
        let mut bytes = VideoFragmentHeader::new(false, 0, 1, 0, 2).encode_into(b"x");
        bytes[0] = 99;
        assert!(VideoFragmentHeader::parse(&bytes).is_none());
        let mut bytes = VideoFragmentHeader::new(false, 0, 1, 2, 2).encode_into(b"x");
        bytes[13..15].copy_from_slice(&2u16.to_le_bytes());
        assert!(VideoFragmentHeader::parse(&bytes).is_none());
    }
}
