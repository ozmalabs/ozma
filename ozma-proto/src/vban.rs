//! VBAN V0.3 wire format.
//!
//! Reference: <http://vban.org/> and the `rusty-vban` crate.
//!
//! # Packet layout
//! ```text
//! Offset  Size  Field
//! ──────  ────  ─────────────────────────────────────────────────────────────
//!  0       4    Magic "VBAN" (0x4E414256 little-endian)
//!  4       1    sample-rate index (bits 0-4) | sub-protocol (bits 5-7)
//!  5       1    samples per frame - 1  (0 → 1 sample, 255 → 256 samples)
//!  6       1    channel count - 1      (0 → 1 ch, 255 → 256 ch)
//!  7       1    data format / codec
//!  8      16    stream name (null-padded ASCII)
//! 24       4    frame counter (little-endian u32)
//! 28       N    audio payload
//! ```
//!
//! Total header: **28 bytes**.

use bytemuck::{Pod, Zeroable};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use zerocopy::{AsBytes, FromBytes, FromZeroes};

// ── Constants ─────────────────────────────────────────────────────────────────

/// VBAN magic bytes: ASCII "VBAN" in little-endian u32 = 0x4E414256.
pub const VBAN_MAGIC: [u8; 4] = *b"VBAN";

/// Maximum stream-name length (including null terminator).
pub const STREAM_NAME_LEN: usize = 16;

// ── Sub-protocol identifiers (bits 5-7 of byte 4) ────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum VbanSubProtocol {
    Audio   = 0x00,
    Serial  = 0x20,
    Txt     = 0x40,
    Service = 0x60,
    /// Undefined / user-defined
    Undefined = 0x80,
}

impl VbanSubProtocol {
    fn from_byte(b: u8) -> Self {
        match b & 0xE0 {
            0x00 => Self::Audio,
            0x20 => Self::Serial,
            0x40 => Self::Txt,
            0x60 => Self::Service,
            _    => Self::Undefined,
        }
    }
}

// ── Sample-rate table (VBAN spec, index 0-17) ─────────────────────────────────

/// VBAN sample-rate index → Hz.
///
/// Index is stored in bits 0-4 of header byte 4.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum VbanSampleRate {
    Hz6000   = 0,
    Hz12000  = 1,
    Hz24000  = 2,
    Hz48000  = 3,
    Hz96000  = 4,
    Hz192000 = 5,
    Hz384000 = 6,
    Hz8000   = 7,
    Hz16000  = 8,
    Hz32000  = 9,
    Hz64000  = 10,
    Hz128000 = 11,
    Hz256000 = 12,
    Hz512000 = 13,
    Hz11025  = 14,
    Hz22050  = 15,
    Hz44100  = 16,
    Hz88200  = 17,
    Hz176400 = 18,
    Hz352800 = 19,
}

impl VbanSampleRate {
    /// Return the sample rate in Hz.
    pub fn hz(self) -> u32 {
        match self {
            Self::Hz6000   => 6_000,
            Self::Hz12000  => 12_000,
            Self::Hz24000  => 24_000,
            Self::Hz48000  => 48_000,
            Self::Hz96000  => 96_000,
            Self::Hz192000 => 192_000,
            Self::Hz384000 => 384_000,
            Self::Hz8000   => 8_000,
            Self::Hz16000  => 16_000,
            Self::Hz32000  => 32_000,
            Self::Hz64000  => 64_000,
            Self::Hz128000 => 128_000,
            Self::Hz256000 => 256_000,
            Self::Hz512000 => 512_000,
            Self::Hz11025  => 11_025,
            Self::Hz22050  => 22_050,
            Self::Hz44100  => 44_100,
            Self::Hz88200  => 88_200,
            Self::Hz176400 => 176_400,
            Self::Hz352800 => 352_800,
        }
    }

    fn from_index(idx: u8) -> Option<Self> {
        match idx {
            0  => Some(Self::Hz6000),
            1  => Some(Self::Hz12000),
            2  => Some(Self::Hz24000),
            3  => Some(Self::Hz48000),
            4  => Some(Self::Hz96000),
            5  => Some(Self::Hz192000),
            6  => Some(Self::Hz384000),
            7  => Some(Self::Hz8000),
            8  => Some(Self::Hz16000),
            9  => Some(Self::Hz32000),
            10 => Some(Self::Hz64000),
            11 => Some(Self::Hz128000),
            12 => Some(Self::Hz256000),
            13 => Some(Self::Hz512000),
            14 => Some(Self::Hz11025),
            15 => Some(Self::Hz22050),
            16 => Some(Self::Hz44100),
            17 => Some(Self::Hz88200),
            18 => Some(Self::Hz176400),
            19 => Some(Self::Hz352800),
            _  => None,
        }
    }
}

// ── Codec / data-format byte ──────────────────────────────────────────────────

/// VBAN audio codec / data-format identifier (byte 7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum VbanCodec {
    /// 16-bit signed PCM (most common)
    Pcm16  = 0x00,
    /// 24-bit signed PCM
    Pcm24  = 0x01,
    /// 32-bit signed PCM
    Pcm32  = 0x02,
    /// 32-bit IEEE float
    Float32 = 0x03,
    /// 64-bit IEEE float
    Float64 = 0x04,
    /// 12-bit signed PCM
    Pcm12  = 0x05,
    /// 10-bit signed PCM
    Pcm10  = 0x06,
    /// 8-bit signed PCM
    Pcm8   = 0x07,
    /// VBAN compressed (proprietary)
    Vbca   = 0x10,
    /// VBAN compressed lossless
    Vbcl   = 0x11,
}

impl VbanCodec {
    /// Bytes per sample for PCM codecs; `None` for compressed codecs.
    pub fn bytes_per_sample(self) -> Option<usize> {
        match self {
            Self::Pcm8   => Some(1),
            Self::Pcm10  => Some(2), // stored as 16-bit
            Self::Pcm12  => Some(2), // stored as 16-bit
            Self::Pcm16  => Some(2),
            Self::Pcm24  => Some(3),
            Self::Pcm32  => Some(4),
            Self::Float32 => Some(4),
            Self::Float64 => Some(8),
            Self::Vbca | Self::Vbcl => None,
        }
    }

    fn from_byte(b: u8) -> Option<Self> {
        match b {
            0x00 => Some(Self::Pcm16),
            0x01 => Some(Self::Pcm24),
            0x02 => Some(Self::Pcm32),
            0x03 => Some(Self::Float32),
            0x04 => Some(Self::Float64),
            0x05 => Some(Self::Pcm12),
            0x06 => Some(Self::Pcm10),
            0x07 => Some(Self::Pcm8),
            0x10 => Some(Self::Vbca),
            0x11 => Some(Self::Vbcl),
            _    => None,
        }
    }
}

// ── Errors ────────────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum VbanHeaderError {
    #[error("buffer too short: need at least 28 bytes, got {0}")]
    TooShort(usize),
    #[error("invalid VBAN magic: expected b\"VBAN\", got {0:?}")]
    BadMagic([u8; 4]),
    #[error("unknown sample-rate index {0}")]
    UnknownSampleRate(u8),
    #[error("unknown codec byte 0x{0:02X}")]
    UnknownCodec(u8),
}

// ── VbanHeader ────────────────────────────────────────────────────────────────

/// 28-byte VBAN packet header (little-endian, `#[repr(C)]`).
///
/// Implements [`zerocopy::AsBytes`] / [`zerocopy::FromBytes`] for zero-copy
/// casting and [`bytemuck::Pod`] for safe transmutation.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq,
    AsBytes, FromBytes, FromZeroes,
    Pod, Zeroable,
)]
#[repr(C)]
pub struct VbanHeader {
    /// Magic bytes: `b"VBAN"`.
    pub magic: [u8; 4],
    /// `(sub_protocol << 5) | sample_rate_index`
    pub format_sr: u8,
    /// Samples per frame minus 1.
    pub samples_per_frame_m1: u8,
    /// Channel count minus 1.
    pub channels_m1: u8,
    /// Data format / codec byte.
    pub format_bit: u8,
    /// Stream name, null-padded ASCII, 16 bytes.
    pub stream_name: [u8; 16],
    /// Frame counter (little-endian u32).
    pub frame_counter: u32,
}


impl VbanHeader {
    /// Construct a new header with sensible defaults.
    ///
    /// - sub-protocol: Audio
    /// - sample rate: 48 000 Hz
    /// - codec: PCM 16-bit
    pub fn new(
        stream_name: &str,
        sample_rate: VbanSampleRate,
        channels: u8,
        samples_per_frame: u8,
        codec: VbanCodec,
        frame_counter: u32,
    ) -> Self {
        let mut name_buf = [0u8; 16];
        let bytes = stream_name.as_bytes();
        let len = bytes.len().min(15); // leave room for null terminator
        name_buf[..len].copy_from_slice(&bytes[..len]);

        Self {
            magic: VBAN_MAGIC,
            format_sr: (VbanSubProtocol::Audio as u8) | (sample_rate as u8),
            samples_per_frame_m1: samples_per_frame.saturating_sub(1),
            channels_m1: channels.saturating_sub(1),
            format_bit: codec as u8,
            stream_name: name_buf,
            frame_counter,
        }
    }

    /// Deserialise from a byte slice (must be ≥ 28 bytes).
    pub fn from_bytes(buf: &[u8]) -> Result<Self, VbanHeaderError> {
        if buf.len() < 28 {
            return Err(VbanHeaderError::TooShort(buf.len()));
        }
        let magic: [u8; 4] = buf[0..4].try_into().unwrap();
        if magic != VBAN_MAGIC {
            return Err(VbanHeaderError::BadMagic(magic));
        }
        let sr_idx = buf[4] & 0x1F;
        VbanSampleRate::from_index(sr_idx)
            .ok_or(VbanHeaderError::UnknownSampleRate(sr_idx))?;
        VbanCodec::from_byte(buf[7])
            .ok_or(VbanHeaderError::UnknownCodec(buf[7]))?;

        let mut stream_name = [0u8; 16];
        stream_name.copy_from_slice(&buf[8..24]);
        let frame_counter = u32::from_le_bytes(buf[24..28].try_into().unwrap());

        Ok(Self {
            magic,
            format_sr: buf[4],
            samples_per_frame_m1: buf[5],
            channels_m1: buf[6],
            format_bit: buf[7],
            stream_name,
            frame_counter,
        })
    }

    /// Serialise to a 28-byte array.
    pub fn to_bytes(self) -> [u8; 28] {
        let mut out = [0u8; 28];
        out[0..4].copy_from_slice(&self.magic);
        out[4] = self.format_sr;
        out[5] = self.samples_per_frame_m1;
        out[6] = self.channels_m1;
        out[7] = self.format_bit;
        out[8..24].copy_from_slice(&self.stream_name);
        out[24..28].copy_from_slice(&self.frame_counter.to_le_bytes());
        out
    }

    /// Decoded sample rate.
    pub fn sample_rate(&self) -> Option<VbanSampleRate> {
        VbanSampleRate::from_index(self.format_sr & 0x1F)
    }

    /// Decoded sub-protocol.
    pub fn sub_protocol(&self) -> VbanSubProtocol {
        VbanSubProtocol::from_byte(self.format_sr)
    }

    /// Decoded codec.
    pub fn codec(&self) -> Option<VbanCodec> {
        VbanCodec::from_byte(self.format_bit)
    }

    /// Samples per frame (decoded from `samples_per_frame_m1 + 1`).
    pub fn samples_per_frame(&self) -> u16 {
        self.samples_per_frame_m1 as u16 + 1
    }

    /// Channel count (decoded from `channels_m1 + 1`).
    pub fn channels(&self) -> u8 {
        self.channels_m1 + 1
    }

    /// Stream name as a `&str` (strips null padding).
    pub fn stream_name_str(&self) -> &str {
        let end = self.stream_name.iter().position(|&b| b == 0).unwrap_or(16);
        std::str::from_utf8(&self.stream_name[..end]).unwrap_or("")
    }
}

// ── VbanAudioFrame ────────────────────────────────────────────────────────────

/// A complete VBAN audio frame: 28-byte header + raw PCM payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VbanAudioFrame {
    pub header: VbanHeader,
    pub payload: Vec<u8>,
}

impl VbanAudioFrame {
    /// Construct from header and raw payload bytes.
    pub fn new(header: VbanHeader, payload: Vec<u8>) -> Self {
        Self { header, payload }
    }

    /// Deserialise a complete VBAN UDP datagram.
    pub fn from_bytes(buf: &[u8]) -> Result<Self, VbanHeaderError> {
        let header = VbanHeader::from_bytes(buf)?;
        let payload = buf[28..].to_vec();
        Ok(Self { header, payload })
    }

    /// Serialise to a `Vec<u8>` suitable for sending as a UDP datagram.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(28 + self.payload.len());
        out.extend_from_slice(&self.header.to_bytes());
        out.extend_from_slice(&self.payload);
        out
    }

    /// Expected payload size in bytes for PCM codecs.
    ///
    /// Returns `None` for compressed codecs or unknown codecs.
    pub fn expected_payload_size(&self) -> Option<usize> {
        let bps = self.header.codec()?.bytes_per_sample()?;
        Some(self.header.samples_per_frame() as usize
            * self.header.channels() as usize
            * bps)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_header() -> VbanHeader {
        VbanHeader::new("Stream1", VbanSampleRate::Hz48000, 2, 256, VbanCodec::Pcm16, 0)
    }

    // ── Header size ───────────────────────────────────────────────────────────

    #[test]
    fn header_size_is_28() {
        assert_eq!(std::mem::size_of::<VbanHeader>(), 28);
    }

    // ── Magic ─────────────────────────────────────────────────────────────────

    #[test]
    fn header_magic_bytes() {
        let h = make_header();
        let bytes = h.to_bytes();
        assert_eq!(&bytes[0..4], b"VBAN");
    }

    // ── Round-trip ────────────────────────────────────────────────────────────

    #[test]
    fn header_round_trip() {
        let h = make_header();
        let bytes = h.to_bytes();
        let decoded = VbanHeader::from_bytes(&bytes).unwrap();
        assert_eq!(h, decoded);
    }

    #[test]
    fn header_round_trip_all_sample_rates() {
        let rates = [
            VbanSampleRate::Hz6000, VbanSampleRate::Hz8000, VbanSampleRate::Hz11025,
            VbanSampleRate::Hz16000, VbanSampleRate::Hz22050, VbanSampleRate::Hz24000,
            VbanSampleRate::Hz32000, VbanSampleRate::Hz44100, VbanSampleRate::Hz48000,
            VbanSampleRate::Hz64000, VbanSampleRate::Hz88200, VbanSampleRate::Hz96000,
            VbanSampleRate::Hz128000, VbanSampleRate::Hz176400, VbanSampleRate::Hz192000,
            VbanSampleRate::Hz352800, VbanSampleRate::Hz384000, VbanSampleRate::Hz512000,
        ];
        for rate in rates {
            let h = VbanHeader::new("test", rate, 1, 1, VbanCodec::Pcm16, 0);
            let decoded = VbanHeader::from_bytes(&h.to_bytes()).unwrap();
            assert_eq!(decoded.sample_rate(), Some(rate));
        }
    }

    #[test]
    fn header_round_trip_all_codecs() {
        let codecs = [
            VbanCodec::Pcm8, VbanCodec::Pcm10, VbanCodec::Pcm12,
            VbanCodec::Pcm16, VbanCodec::Pcm24, VbanCodec::Pcm32,
            VbanCodec::Float32, VbanCodec::Float64,
            VbanCodec::Vbca, VbanCodec::Vbcl,
        ];
        for codec in codecs {
            let h = VbanHeader::new("test", VbanSampleRate::Hz48000, 1, 1, codec, 0);
            let decoded = VbanHeader::from_bytes(&h.to_bytes()).unwrap();
            assert_eq!(decoded.codec(), Some(codec));
        }
    }

    // ── Field decoding ────────────────────────────────────────────────────────

    #[test]
    fn header_fields_decoded() {
        let h = make_header();
        assert_eq!(h.sample_rate(), Some(VbanSampleRate::Hz48000));
        assert_eq!(h.sub_protocol(), VbanSubProtocol::Audio);
        assert_eq!(h.codec(), Some(VbanCodec::Pcm16));
        assert_eq!(h.samples_per_frame(), 256);
        assert_eq!(h.channels(), 2);
        assert_eq!(h.stream_name_str(), "Stream1");
    }

    #[test]
    fn header_frame_counter_le() {
        let h = VbanHeader::new("test", VbanSampleRate::Hz48000, 1, 1, VbanCodec::Pcm16, 0x01020304);
        let bytes = h.to_bytes();
        // Little-endian: 0x04, 0x03, 0x02, 0x01
        assert_eq!(&bytes[24..28], &[0x04, 0x03, 0x02, 0x01]);
        let decoded = VbanHeader::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.frame_counter, 0x01020304);
    }

    #[test]
    fn header_stream_name_null_padded() {
        let h = VbanHeader::new("Hi", VbanSampleRate::Hz48000, 1, 1, VbanCodec::Pcm16, 0);
        let bytes = h.to_bytes();
        assert_eq!(bytes[8], b'H');
        assert_eq!(bytes[9], b'i');
        assert_eq!(bytes[10], 0x00); // null padding
        assert_eq!(h.stream_name_str(), "Hi");
    }

    // ── Error cases ───────────────────────────────────────────────────────────

    #[test]
    fn header_too_short() {
        assert!(matches!(
            VbanHeader::from_bytes(&[0u8; 27]),
            Err(VbanHeaderError::TooShort(27))
        ));
    }

    #[test]
    fn header_bad_magic() {
        let mut bytes = make_header().to_bytes();
        bytes[0] = b'X';
        assert!(matches!(
            VbanHeader::from_bytes(&bytes),
            Err(VbanHeaderError::BadMagic(_))
        ));
    }

    // ── VbanAudioFrame ────────────────────────────────────────────────────────

    #[test]
    fn audio_frame_round_trip() {
        let header = make_header();
        let payload: Vec<u8> = (0u8..=255).collect();
        let frame = VbanAudioFrame::new(header, payload.clone());
        let bytes = frame.to_bytes();
        assert_eq!(bytes.len(), 28 + 256);
        let decoded = VbanAudioFrame::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.header, header);
        assert_eq!(decoded.payload, payload);
    }

    #[test]
    fn audio_frame_empty_payload() {
        let header = make_header();
        let frame = VbanAudioFrame::new(header, vec![]);
        let bytes = frame.to_bytes();
        assert_eq!(bytes.len(), 28);
        let decoded = VbanAudioFrame::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.payload, Vec::<u8>::new());
    }

    #[test]
    fn audio_frame_expected_payload_size_pcm16_stereo_256() {
        // 256 samples × 2 channels × 2 bytes = 1024
        let frame = VbanAudioFrame::new(make_header(), vec![]);
        assert_eq!(frame.expected_payload_size(), Some(1024));
    }

    #[test]
    fn audio_frame_expected_payload_size_float32_mono() {
        let h = VbanHeader::new("mono", VbanSampleRate::Hz44100, 1, 128, VbanCodec::Float32, 0);
        let frame = VbanAudioFrame::new(h, vec![]);
        // 128 samples × 1 channel × 4 bytes = 512
        assert_eq!(frame.expected_payload_size(), Some(512));
    }

    #[test]
    fn sample_rate_hz_values() {
        assert_eq!(VbanSampleRate::Hz48000.hz(), 48_000);
        assert_eq!(VbanSampleRate::Hz44100.hz(), 44_100);
        assert_eq!(VbanSampleRate::Hz96000.hz(), 96_000);
    }
}
