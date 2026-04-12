//! VBAN V0.3 wire-format types.
//!
//! Reference: <https://vb-audio.com/Voicemeeter/VBANProtocol_Specifications.pdf>
//!
//! # VbanHeader byte layout (28 bytes, little-endian)
//! ```text
//! Offset  Size  Field
//! ──────  ────  ─────────────────────────────────────────────────────────────
//!  0       4    "VBAN" magic (0x56 0x42 0x41 0x4E)
//!  4       1    bits[4:0] = sample_rate_index | bits[7:5] = sub_protocol
//!  5       1    samples_per_frame - 1  (0 → 1 sample, 255 → 256 samples)
//!  6       1    channels - 1           (0 → 1 ch, 255 → 256 ch)
//!  7       1    data_format            (bits[2:0] = format, bits[7:3] = codec)
//!  8      16    stream_name (null-padded ASCII, max 16 bytes)
//! 24       4    frame_counter (u32 little-endian, wraps freely)
//! 28       N    audio payload
//! ```
//! Total header: **28 bytes**.

use bytemuck::{Pod, Zeroable};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use zerocopy::{AsBytes, FromBytes, FromZeroes};

// ── Constants ─────────────────────────────────────────────────────────────────

/// VBAN magic bytes: ASCII "VBAN" = 0x56 0x42 0x41 0x4E.
pub const VBAN_MAGIC: [u8; 4] = *b"VBAN";

// ── Sub-protocol identifiers (bits 7:5 of byte 4) ────────────────────────────

/// VBAN sub-protocol identifiers (upper 3 bits of `sr_sub_proto`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum VbanSubProtocol {
    Audio      = 0x00,
    Serial     = 0x20,
    Txt        = 0x40,
    Service    = 0x60,
    Undefined1 = 0x80,
    Undefined2 = 0xA0,
    Undefined3 = 0xC0,
    User       = 0xE0,
}

impl VbanSubProtocol {
    pub fn from_byte(b: u8) -> Self {
        match b & 0xE0 {
            0x00 => Self::Audio,
            0x20 => Self::Serial,
            0x40 => Self::Txt,
            0x60 => Self::Service,
            0x80 => Self::Undefined1,
            0xA0 => Self::Undefined2,
            0xC0 => Self::Undefined3,
            _    => Self::User,
        }
    }
}

// ── Sample-rate table (bits 4:0 of byte 4) ───────────────────────────────────

/// VBAN sample-rate index → Hz (VBAN spec table, indices 0-20).
///
/// Index is stored in the lower 5 bits of byte 4.
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
    Hz705600 = 20,
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
            Self::Hz705600 => 705_600,
        }
    }

    /// Try to construct from a raw index byte (lower 5 bits used).
    pub fn from_index(idx: u8) -> Option<Self> {
        match idx & 0x1F {
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
            20 => Some(Self::Hz705600),
            _  => None,
        }
    }
}

// ── Data format / codec byte (byte 7) ────────────────────────────────────────

/// VBAN audio data format (byte 7, bits 2:0 = sample format).
///
/// The VBAN spec names these differently from the old `VbanCodec` naming;
/// this enum uses the spec's PCM-centric names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum VbanDataFormat {
    /// 8-bit unsigned PCM
    Uint8   = 0x00,
    /// 16-bit signed PCM (most common)
    Int16   = 0x01,
    /// 24-bit signed PCM
    Int24   = 0x02,
    /// 32-bit signed PCM
    Int32   = 0x03,
    /// 32-bit IEEE float
    Float32 = 0x04,
    /// 64-bit IEEE float
    Float64 = 0x05,
    /// 12-bit signed PCM (stored as 16-bit)
    Int12   = 0x06,
    /// 10-bit signed PCM (stored as 16-bit)
    Int10   = 0x07,
}

impl VbanDataFormat {
    pub fn from_byte(b: u8) -> Option<Self> {
        match b & 0x07 {
            0x00 => Some(Self::Uint8),
            0x01 => Some(Self::Int16),
            0x02 => Some(Self::Int24),
            0x03 => Some(Self::Int32),
            0x04 => Some(Self::Float32),
            0x05 => Some(Self::Float64),
            0x06 => Some(Self::Int12),
            0x07 => Some(Self::Int10),
            _    => None,
        }
    }

    /// Bytes per sample (Int12/Int10 are stored as 16-bit words).
    pub fn bytes_per_sample(self) -> usize {
        match self {
            Self::Uint8   => 1,
            Self::Int16   => 2,
            Self::Int24   => 3,
            Self::Int32   => 4,
            Self::Float32 => 4,
            Self::Float64 => 8,
            Self::Int12   => 2,
            Self::Int10   => 2,
        }
    }
}

// ── Errors ────────────────────────────────────────────────────────────────────

#[derive(Debug, Error, PartialEq, Eq)]
pub enum VbanHeaderError {
    #[error("buffer too short: need at least 28 bytes, got {0}")]
    TooShort(usize),
    #[error("invalid VBAN magic: expected b\"VBAN\", got {0:?}")]
    BadMagic([u8; 4]),
    #[error("unknown sample-rate index {0}")]
    UnknownSampleRate(u8),
}

// ── VbanHeader ────────────────────────────────────────────────────────────────

/// 28-byte VBAN packet header (little-endian, `#[repr(C)]`).
///
/// Implements [`zerocopy::AsBytes`] / [`zerocopy::FromBytes`] for zero-copy
/// casting and [`bytemuck::Pod`] for safe transmutation.
///
/// Verified against the VBAN V0.3 specification byte layout.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq,
    AsBytes, FromBytes, FromZeroes,
    Pod, Zeroable,
)]
#[repr(C)]
pub struct VbanHeader {
    /// Bytes 0-3: magic `b"VBAN"`.
    pub magic: [u8; 4],
    /// Byte 4: bits[4:0] = sample_rate_index, bits[7:5] = sub_protocol.
    pub sr_sub_proto: u8,
    /// Byte 5: samples_per_frame - 1 (0 → 1 sample, 255 → 256 samples).
    pub samples_per_frame_minus1: u8,
    /// Byte 6: channels - 1 (0 → 1 ch, 255 → 256 ch).
    pub channels_minus1: u8,
    /// Byte 7: bits[2:0] = data_format, bits[7:3] = codec (0 = PCM).
    pub data_format: u8,
    /// Bytes 8-23: stream name, null-padded ASCII, max 16 bytes.
    pub stream_name: [u8; 16],
    /// Bytes 24-27: frame counter (u32 little-endian, wraps freely).
    pub frame_counter: u32,
}

// Compile-time size assertion.
const _: () = assert!(core::mem::size_of::<VbanHeader>() == 28);

impl VbanHeader {
    /// Construct a new VBAN audio header.
    ///
    /// - `samples_per_frame`: 1..=256
    /// - `channels`: 1..=256
    /// - `stream_name`: truncated to 16 bytes, null-padded
    pub fn new(
        stream_name: &str,
        sample_rate: VbanSampleRate,
        channels: u8,
        samples_per_frame: u8,
        format: VbanDataFormat,
        frame_counter: u32,
    ) -> Self {
        let mut name_buf = [0u8; 16];
        let name_bytes = stream_name.as_bytes();
        let copy_len = name_bytes.len().min(15); // leave room for NUL
        name_buf[..copy_len].copy_from_slice(&name_bytes[..copy_len]);

        Self {
            magic: VBAN_MAGIC,
            sr_sub_proto: (VbanSubProtocol::Audio as u8) | (sample_rate as u8),
            samples_per_frame_minus1: samples_per_frame.saturating_sub(1),
            channels_minus1: channels.saturating_sub(1),
            data_format: format as u8,
            stream_name: name_buf,
            frame_counter,
        }
    }

    /// Validate the magic bytes.
    #[inline]
    pub fn is_valid(&self) -> bool {
        self.magic == VBAN_MAGIC
    }

    /// Decode the sub-protocol from `sr_sub_proto`.
    pub fn sub_protocol(&self) -> VbanSubProtocol {
        VbanSubProtocol::from_byte(self.sr_sub_proto)
    }

    /// Decode the sample rate from `sr_sub_proto`.
    pub fn sample_rate(&self) -> Option<VbanSampleRate> {
        VbanSampleRate::from_index(self.sr_sub_proto)
    }

    /// Actual samples per frame (stored as value - 1).
    pub fn samples_per_frame(&self) -> u16 {
        self.samples_per_frame_minus1 as u16 + 1
    }

    /// Actual channel count (stored as value - 1).
    pub fn channels(&self) -> u8 {
        self.channels_minus1 + 1
    }

    /// Decode the data format.
    pub fn format(&self) -> Option<VbanDataFormat> {
        VbanDataFormat::from_byte(self.data_format)
    }

    /// Stream name as a `&str` (strips trailing NUL bytes).
    pub fn stream_name_str(&self) -> &str {
        let end = self.stream_name.iter().position(|&b| b == 0).unwrap_or(16);
        core::str::from_utf8(&self.stream_name[..end]).unwrap_or("")
    }

    /// Serialise to the 28-byte wire format.
    pub fn to_bytes(self) -> [u8; 28] {
        let mut buf = [0u8; 28];
        buf[0..4].copy_from_slice(&self.magic);
        buf[4] = self.sr_sub_proto;
        buf[5] = self.samples_per_frame_minus1;
        buf[6] = self.channels_minus1;
        buf[7] = self.data_format;
        buf[8..24].copy_from_slice(&self.stream_name);
        buf[24..28].copy_from_slice(&self.frame_counter.to_le_bytes());
        buf
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

        let mut stream_name = [0u8; 16];
        stream_name.copy_from_slice(&buf[8..24]);
        let frame_counter = u32::from_le_bytes(buf[24..28].try_into().unwrap());

        Ok(Self {
            magic,
            sr_sub_proto: buf[4],
            samples_per_frame_minus1: buf[5],
            channels_minus1: buf[6],
            data_format: buf[7],
            stream_name,
            frame_counter,
        })
    }
}

// ── VbanAudioFrame ────────────────────────────────────────────────────────────

/// A complete VBAN audio packet: 28-byte header + raw PCM payload.
///
/// Serialisable via serde; the header fields are mirrored into a
/// serde-friendly struct since `VbanHeader` uses zerocopy derives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VbanAudioFrame {
    /// Serde-friendly copy of the header fields.
    pub header: VbanHeaderFields,
    pub payload: Vec<u8>,
}

/// Serde-friendly mirror of [`VbanHeader`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VbanHeaderFields {
    pub magic: [u8; 4],
    pub sr_sub_proto: u8,
    pub samples_per_frame_minus1: u8,
    pub channels_minus1: u8,
    pub data_format: u8,
    pub stream_name: [u8; 16],
    pub frame_counter: u32,
}

impl From<VbanHeader> for VbanHeaderFields {
    fn from(h: VbanHeader) -> Self {
        Self {
            magic: h.magic,
            sr_sub_proto: h.sr_sub_proto,
            samples_per_frame_minus1: h.samples_per_frame_minus1,
            channels_minus1: h.channels_minus1,
            data_format: h.data_format,
            stream_name: h.stream_name,
            frame_counter: h.frame_counter,
        }
    }
}

impl From<VbanHeaderFields> for VbanHeader {
    fn from(f: VbanHeaderFields) -> Self {
        Self {
            magic: f.magic,
            sr_sub_proto: f.sr_sub_proto,
            samples_per_frame_minus1: f.samples_per_frame_minus1,
            channels_minus1: f.channels_minus1,
            data_format: f.data_format,
            stream_name: f.stream_name,
            frame_counter: f.frame_counter,
        }
    }
}

impl VbanAudioFrame {
    /// Build a frame from a [`VbanHeader`] and raw PCM bytes.
    pub fn new(header: VbanHeader, payload: Vec<u8>) -> Self {
        Self { header: header.into(), payload }
    }

    /// Serialise to wire bytes (header bytes || payload).
    pub fn to_bytes(&self) -> Vec<u8> {
        let h: VbanHeader = self.header.clone().into();
        let mut buf = Vec::with_capacity(28 + self.payload.len());
        buf.extend_from_slice(&h.to_bytes());
        buf.extend_from_slice(&self.payload);
        buf
    }

    /// Deserialise from wire bytes.
    pub fn from_bytes(buf: &[u8]) -> Result<Self, VbanHeaderError> {
        let header = VbanHeader::from_bytes(buf)?;
        Ok(Self {
            header: header.into(),
            payload: buf[28..].to_vec(),
        })
    }

    /// Expected payload size in bytes given the header fields.
    pub fn expected_payload_size(&self) -> Option<usize> {
        let h: VbanHeader = self.header.clone().into();
        let bps = h.format()?.bytes_per_sample();
        Some(h.samples_per_frame() as usize * h.channels() as usize * bps)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use zerocopy::AsBytes;

    fn make_header() -> VbanHeader {
        VbanHeader::new("Stream1", VbanSampleRate::Hz48000, 2, 256, VbanDataFormat::Int16, 0)
    }

    // ── Header size & magic ───────────────────────────────────────────────────

    #[test]
    fn header_size_is_28_bytes() {
        assert_eq!(core::mem::size_of::<VbanHeader>(), 28);
    }

    #[test]
    fn header_magic_is_vban() {
        let h = make_header();
        assert_eq!(&h.magic, b"VBAN");
        assert!(h.is_valid());
    }

    // ── Byte layout verification (against VBAN V0.3 spec) ────────────────────

    #[test]
    fn header_wire_layout_offsets() {
        let h = VbanHeader::new(
            "Test",
            VbanSampleRate::Hz48000,  // index 3
            2,                         // stored as 1
            256,                       // stored as 255
            VbanDataFormat::Int16,     // 0x01
            0x0000_0001,
        );
        let bytes = h.to_bytes();

        assert_eq!(&bytes[0..4], b"VBAN");          // magic
        assert_eq!(bytes[4], 0x03);                  // sr_index=3, sub_proto=Audio(0)
        assert_eq!(bytes[5], 255);                   // 256-1
        assert_eq!(bytes[6], 1);                     // 2-1
        assert_eq!(bytes[7], 0x01);                  // Int16
        assert_eq!(&bytes[8..12], b"Test");          // stream name
        assert_eq!(&bytes[12..24], &[0u8; 12]);      // NUL padding
        assert_eq!(&bytes[24..28], &[0x01, 0x00, 0x00, 0x00]); // frame_counter LE
    }

    #[test]
    fn header_round_trip() {
        let h = make_header();
        let decoded = VbanHeader::from_bytes(&h.to_bytes()).unwrap();
        assert_eq!(h, decoded);
    }

    #[test]
    fn header_zerocopy_as_bytes_matches_to_bytes() {
        let h = make_header();
        assert_eq!(h.as_bytes(), &h.to_bytes());
    }

    #[test]
    fn header_accessors() {
        let h = make_header();
        assert_eq!(h.sub_protocol(), VbanSubProtocol::Audio);
        assert_eq!(h.sample_rate(), Some(VbanSampleRate::Hz48000));
        assert_eq!(h.samples_per_frame(), 256);
        assert_eq!(h.channels(), 2);
        assert_eq!(h.format(), Some(VbanDataFormat::Int16));
        assert_eq!(h.stream_name_str(), "Stream1");
    }

    #[test]
    fn header_frame_counter_little_endian() {
        let h = VbanHeader::new(
            "fc", VbanSampleRate::Hz44100, 1, 1, VbanDataFormat::Float32, 0xDEAD_BEEF,
        );
        let bytes = h.to_bytes();
        assert_eq!(&bytes[24..28], &0xDEAD_BEEFu32.to_le_bytes());
        let decoded = VbanHeader::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.frame_counter, 0xDEAD_BEEF);
    }

    #[test]
    fn header_stream_name_truncated_to_15_plus_nul() {
        // new() leaves room for NUL, so max usable chars = 15
        let h = VbanHeader::new(
            "ABCDEFGHIJKLMNOPQRSTUVWXYZ",
            VbanSampleRate::Hz48000, 1, 1, VbanDataFormat::Uint8, 0,
        );
        assert_eq!(h.stream_name_str(), "ABCDEFGHIJKLMNO");
    }

    // ── VbanSampleRate ────────────────────────────────────────────────────────

    #[test]
    fn sample_rate_index_round_trip() {
        let cases = [
            (VbanSampleRate::Hz6000,  0u8,  6_000u32),
            (VbanSampleRate::Hz48000, 3,   48_000),
            (VbanSampleRate::Hz44100, 16,  44_100),
            (VbanSampleRate::Hz96000, 4,   96_000),
            (VbanSampleRate::Hz8000,  7,    8_000),
        ];
        for (variant, idx, hz) in cases {
            assert_eq!(VbanSampleRate::from_index(idx), Some(variant));
            assert_eq!(variant.hz(), hz);
            assert_eq!(variant as u8, idx);
        }
    }

    #[test]
    fn sample_rate_unknown_index_returns_none() {
        assert_eq!(VbanSampleRate::from_index(21), None);
        assert_eq!(VbanSampleRate::from_index(31), None);
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
            VbanSampleRate::Hz705600,
        ];
        for rate in rates {
            let h = VbanHeader::new("t", rate, 1, 1, VbanDataFormat::Int16, 0);
            let decoded = VbanHeader::from_bytes(&h.to_bytes()).unwrap();
            assert_eq!(decoded.sample_rate(), Some(rate));
        }
    }

    // ── VbanSubProtocol ───────────────────────────────────────────────────────

    #[test]
    fn sub_protocol_round_trip() {
        let cases = [
            (VbanSubProtocol::Audio,   0x00u8),
            (VbanSubProtocol::Serial,  0x20),
            (VbanSubProtocol::Txt,     0x40),
            (VbanSubProtocol::Service, 0x60),
            (VbanSubProtocol::User,    0xE0),
        ];
        for (proto, byte) in cases {
            assert_eq!(proto as u8, byte);
            assert_eq!(VbanSubProtocol::from_byte(byte), proto);
        }
    }

    // ── VbanDataFormat ────────────────────────────────────────────────────────

    #[test]
    fn data_format_round_trip() {
        let cases = [
            (VbanDataFormat::Uint8,   0x00u8, 1usize),
            (VbanDataFormat::Int16,   0x01,   2),
            (VbanDataFormat::Int24,   0x02,   3),
            (VbanDataFormat::Int32,   0x03,   4),
            (VbanDataFormat::Float32, 0x04,   4),
            (VbanDataFormat::Float64, 0x05,   8),
            (VbanDataFormat::Int12,   0x06,   2),
            (VbanDataFormat::Int10,   0x07,   2),
        ];
        for (fmt, byte, bps) in cases {
            assert_eq!(VbanDataFormat::from_byte(byte), Some(fmt));
            assert_eq!(fmt.bytes_per_sample(), bps);
            assert_eq!(fmt as u8, byte);
        }
    }

    #[test]
    fn header_round_trip_all_formats() {
        let formats = [
            VbanDataFormat::Uint8, VbanDataFormat::Int16, VbanDataFormat::Int24,
            VbanDataFormat::Int32, VbanDataFormat::Float32, VbanDataFormat::Float64,
            VbanDataFormat::Int12, VbanDataFormat::Int10,
        ];
        for fmt in formats {
            let h = VbanHeader::new("t", VbanSampleRate::Hz48000, 1, 1, fmt, 0);
            let decoded = VbanHeader::from_bytes(&h.to_bytes()).unwrap();
            assert_eq!(decoded.format(), Some(fmt));
        }
    }

    // ── VbanAudioFrame ────────────────────────────────────────────────────────

    #[test]
    fn audio_frame_round_trip() {
        let header = make_header();
        let payload: Vec<u8> = (0u8..=255).collect();
        let frame = VbanAudioFrame::new(header, payload.clone());

        let wire = frame.to_bytes();
        assert_eq!(wire.len(), 28 + payload.len());

        let decoded = VbanAudioFrame::from_bytes(&wire).unwrap();
        assert_eq!(decoded, frame);
    }

    #[test]
    fn audio_frame_empty_payload_round_trip() {
        let frame = VbanAudioFrame::new(make_header(), vec![]);
        let wire = frame.to_bytes();
        assert_eq!(wire.len(), 28);
        let decoded = VbanAudioFrame::from_bytes(&wire).unwrap();
        assert_eq!(decoded.payload, Vec::<u8>::new());
    }

    #[test]
    fn audio_frame_bad_magic_returns_error() {
        let mut wire = VbanAudioFrame::new(make_header(), vec![0u8; 4]).to_bytes();
        wire[0] = b'X';
        assert!(matches!(
            VbanAudioFrame::from_bytes(&wire),
            Err(VbanHeaderError::BadMagic(_))
        ));
    }

    #[test]
    fn audio_frame_too_short_returns_error() {
        assert!(matches!(
            VbanAudioFrame::from_bytes(&[0u8; 27]),
            Err(VbanHeaderError::TooShort(27))
        ));
    }

    #[test]
    fn audio_frame_expected_payload_size_int16_stereo_256() {
        // 256 samples × 2 channels × 2 bytes (Int16) = 1024
        let frame = VbanAudioFrame::new(make_header(), vec![]);
        assert_eq!(frame.expected_payload_size(), Some(1024));
    }

    #[test]
    fn audio_frame_expected_payload_size_float32_mono_128() {
        let h = VbanHeader::new("mono", VbanSampleRate::Hz44100, 1, 128, VbanDataFormat::Float32, 0);
        let frame = VbanAudioFrame::new(h, vec![]);
        // 128 × 1 × 4 = 512
        assert_eq!(frame.expected_payload_size(), Some(512));
    }

    #[test]
    fn audio_frame_serde_round_trip() {
        let frame = VbanAudioFrame::new(make_header(), vec![0xAB, 0xCD, 0xEF]);
        let json = serde_json::to_string(&frame).unwrap();
        let decoded: VbanAudioFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(frame, decoded);
    }
}
