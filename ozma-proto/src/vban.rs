//! VBAN V0.3 wire-format types.
//!
//! Reference: <https://vb-audio.com/Voicemeeter/VBANProtocol_Specifications.pdf>
//!
//! # Frame layout
//! ```text
//! Offset  Len  Field
//! ------  ---  -----
//!  0       4   magic  b"VBAN"
//!  4       1   sr_sub_proto   bits[4:0]=sample_rate_index, bits[7:5]=sub_protocol
//!  5       1   samples_per_frame_minus1   (N-1, so 0→1 sample)
//!  6       1   channels_minus1            (N-1, so 0→1 channel)
//!  7       1   data_format_codec          bits[2:0]=data_format, bits[7:3]=codec
//!  8      16   stream_name  (null-padded UTF-8, max 16 bytes)
//! 24       4   frame_counter  (little-endian u32)
//! 28       *   PCM payload
//! ```
//!
//! Total header size: **28 bytes**.

use serde::{Deserialize, Serialize};
use zerocopy::{AsBytes, FromBytes, FromZeroes};

// ---------------------------------------------------------------------------
// Sample-rate index table (VBAN spec §3.1)
// ---------------------------------------------------------------------------

/// VBAN sample-rate lookup table.  Index is stored in bits[4:0] of `sr_sub_proto`.
///
/// Indices 0-20 are defined by the spec; indices 21-31 are reserved.
pub const SAMPLE_RATES: [u32; 21] = [
     6_000,  12_000,  24_000,  48_000,  96_000, 192_000, 384_000,
     8_000,  16_000,  32_000,  64_000, 128_000, 256_000, 512_000,
    11_025,  22_050,  44_100,  88_200, 176_400, 352_800, 705_600,
];

/// Sub-protocol values stored in bits[7:5] of `sr_sub_proto`.
pub mod sub_proto {
    pub const AUDIO:      u8 = 0x00;
    pub const SERIAL:     u8 = 0x20;
    pub const TXT:        u8 = 0x40;
    pub const SERVICE:    u8 = 0x60;
    pub const UNDEFINED1: u8 = 0x80;
    pub const UNDEFINED2: u8 = 0xA0;
    pub const UNDEFINED3: u8 = 0xC0;
    pub const USER:       u8 = 0xE0;
}

/// Data-format values stored in bits[2:0] of `data_format_codec`.
pub mod data_format {
    /// 8-bit unsigned PCM
    pub const UINT8:   u8 = 0x00;
    /// 16-bit signed PCM (most common)
    pub const INT16:   u8 = 0x01;
    /// 24-bit signed PCM
    pub const INT24:   u8 = 0x02;
    /// 32-bit signed PCM
    pub const INT32:   u8 = 0x03;
    /// 32-bit IEEE float
    pub const FLOAT32: u8 = 0x04;
    /// 64-bit IEEE float
    pub const FLOAT64: u8 = 0x05;
    /// 12-bit signed PCM (stored as 16-bit words)
    pub const INT12:   u8 = 0x06;
    /// 10-bit signed PCM (stored as 16-bit words)
    pub const INT10:   u8 = 0x07;
}

/// Bytes per sample for each [`data_format`] constant.
pub fn bytes_per_sample(fmt: u8) -> Option<usize> {
    match fmt & 0x07 {
        data_format::UINT8   => Some(1),
        data_format::INT16   => Some(2),
        data_format::INT24   => Some(3),
        data_format::INT32   => Some(4),
        data_format::FLOAT32 => Some(4),
        data_format::FLOAT64 => Some(8),
        data_format::INT12   => Some(2),
        data_format::INT10   => Some(2),
        _                    => None,
    }
}

pub const VBAN_MAGIC: &[u8; 4] = b"VBAN";
pub const HEADER_SIZE: usize = 28;

// ---------------------------------------------------------------------------
// VbanHeader — 28-byte on-wire header
// ---------------------------------------------------------------------------

/// 28-byte VBAN V0.3 frame header.
///
/// The struct is `#[repr(C)]` and implements [`zerocopy`] traits so it can be
/// cast directly to/from a `[u8; 28]` without copying.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq,
    Serialize, Deserialize,
    AsBytes, FromBytes, FromZeroes,
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
    pub data_format_codec: u8,
    /// Bytes 8-23: stream name, null-padded UTF-8, max 16 bytes.
    pub stream_name: [u8; 16],
    /// Bytes 24-27: frame counter, little-endian.
    pub frame_counter: u32,
}

// Compile-time size assertion.
const _: () = assert!(core::mem::size_of::<VbanHeader>() == HEADER_SIZE);

impl VbanHeader {
    /// Construct a standard PCM audio header.
    ///
    /// * `sample_rate_index` — index into [`SAMPLE_RATES`] (0–20)
    /// * `samples`           — samples per frame (1–256; stored as N-1)
    /// * `channels`          — channel count (1–256; stored as N-1)
    /// * `data_fmt`          — one of the [`data_format`] constants
    /// * `stream_name`       — up to 16 bytes; truncated/null-padded
    /// * `frame_counter`     — monotonically increasing frame number
    pub fn new_audio(
        sample_rate_index: u8,
        samples: u8,
        channels: u8,
        data_fmt: u8,
        stream_name: &str,
        frame_counter: u32,
    ) -> Self {
        let mut name = [0u8; 16];
        let bytes = stream_name.as_bytes();
        let len = bytes.len().min(16);
        name[..len].copy_from_slice(&bytes[..len]);

        Self {
            magic: *VBAN_MAGIC,
            sr_sub_proto: sub_proto::AUDIO | (sample_rate_index & 0x1F),
            samples_per_frame_minus1: samples.saturating_sub(1),
            channels_minus1: channels.saturating_sub(1),
            data_format_codec: data_fmt & 0x07, // codec bits[7:3] = 0 → PCM
            stream_name: name,
            frame_counter,
        }
    }

    /// Encode to the 28-byte on-wire representation (little-endian).
    pub fn to_bytes(self) -> [u8; HEADER_SIZE] {
        let mut out = [0u8; HEADER_SIZE];
        out[0..4].copy_from_slice(&self.magic);
        out[4] = self.sr_sub_proto;
        out[5] = self.samples_per_frame_minus1;
        out[6] = self.channels_minus1;
        out[7] = self.data_format_codec;
        out[8..24].copy_from_slice(&self.stream_name);
        out[24..28].copy_from_slice(&self.frame_counter.to_le_bytes());
        out
    }

    /// Decode from a 28-byte array.  Returns `None` if the magic is wrong.
    pub fn from_bytes(b: [u8; HEADER_SIZE]) -> Option<Self> {
        if &b[0..4] != VBAN_MAGIC.as_ref() {
            return None;
        }
        let mut stream_name = [0u8; 16];
        stream_name.copy_from_slice(&b[8..24]);
        let frame_counter = u32::from_le_bytes([b[24], b[25], b[26], b[27]]);
        Some(Self {
            magic: *VBAN_MAGIC,
            sr_sub_proto: b[4],
            samples_per_frame_minus1: b[5],
            channels_minus1: b[6],
            data_format_codec: b[7],
            stream_name,
            frame_counter,
        })
    }

    /// Return the sample rate in Hz, or `None` if the index is out of range.
    pub fn sample_rate_hz(&self) -> Option<u32> {
        let idx = (self.sr_sub_proto & 0x1F) as usize;
        SAMPLE_RATES.get(idx).copied()
    }

    /// Return the sub-protocol nibble (bits[7:5]).
    pub fn sub_protocol(&self) -> u8 {
        self.sr_sub_proto & 0xE0
    }

    /// Return the number of samples per frame (1–256).
    pub fn samples_per_frame(&self) -> u16 {
        self.samples_per_frame_minus1 as u16 + 1
    }

    /// Return the number of channels (1–256).
    pub fn channels(&self) -> u16 {
        self.channels_minus1 as u16 + 1
    }

    /// Return the data format (bits[2:0] of `data_format_codec`).
    pub fn data_format(&self) -> u8 {
        self.data_format_codec & 0x07
    }

    /// Return the stream name as a `&str`, trimming trailing NUL bytes.
    pub fn stream_name_str(&self) -> &str {
        let end = self.stream_name.iter().position(|&b| b == 0).unwrap_or(16);
        core::str::from_utf8(&self.stream_name[..end]).unwrap_or("")
    }
}

// ---------------------------------------------------------------------------
// VbanAudioFrame — header + PCM payload
// ---------------------------------------------------------------------------

/// A complete VBAN audio frame: 28-byte header followed by a PCM payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VbanAudioFrame {
    /// Parsed header.
    pub header: VbanHeader,
    /// Raw PCM payload bytes.
    pub payload: Vec<u8>,
}

impl VbanAudioFrame {
    /// Encode the full frame (header + payload) to bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_SIZE + self.payload.len());
        out.extend_from_slice(&self.header.to_bytes());
        out.extend_from_slice(&self.payload);
        out
    }

    /// Decode a frame from a byte slice.
    ///
    /// Returns `None` if the slice is shorter than 28 bytes or the magic is wrong.
    pub fn from_bytes(data: &[u8]) -> Option<Self> {
        if data.len() < HEADER_SIZE {
            return None;
        }
        let mut hdr_bytes = [0u8; HEADER_SIZE];
        hdr_bytes.copy_from_slice(&data[..HEADER_SIZE]);
        let header = VbanHeader::from_bytes(hdr_bytes)?;
        let payload = data[HEADER_SIZE..].to_vec();
        Some(Self { header, payload })
    }

    /// Expected payload size in bytes given the header fields.
    pub fn expected_payload_size(&self) -> Option<usize> {
        let bps = bytes_per_sample(self.header.data_format())?;
        Some(self.header.samples_per_frame() as usize * self.header.channels() as usize * bps)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_header() -> VbanHeader {
        // index 3 → 48 000 Hz, 256 samples, stereo, Int16
        VbanHeader::new_audio(3, 255, 2, data_format::INT16, "Stream1", 0)
    }

    // --- VbanHeader ---

    #[test]
    fn header_size_is_28_bytes() {
        assert_eq!(core::mem::size_of::<VbanHeader>(), HEADER_SIZE);
    }

    #[test]
    fn header_magic() {
        let h = make_header();
        assert_eq!(&h.magic, b"VBAN");
    }

    #[test]
    fn header_round_trip() {
        let h = make_header();
        let decoded = VbanHeader::from_bytes(h.to_bytes()).expect("decode failed");
        assert_eq!(decoded, h);
    }

    #[test]
    fn header_bad_magic_returns_none() {
        let mut bytes = make_header().to_bytes();
        bytes[0] = b'X';
        assert!(VbanHeader::from_bytes(bytes).is_none());
    }

    #[test]
    fn header_sample_rate_hz() {
        assert_eq!(make_header().sample_rate_hz(), Some(48_000));
    }

    #[test]
    fn header_samples_per_frame() {
        // new_audio(samples=255) → stored as 254 → accessor returns 255
        assert_eq!(make_header().samples_per_frame(), 255);
    }

    #[test]
    fn header_channels() {
        assert_eq!(make_header().channels(), 2);
    }

    #[test]
    fn header_data_format() {
        assert_eq!(make_header().data_format(), data_format::INT16);
    }

    #[test]
    fn header_stream_name() {
        assert_eq!(make_header().stream_name_str(), "Stream1");
    }

    #[test]
    fn header_frame_counter_little_endian() {
        let h = VbanHeader::new_audio(16, 1, 1, data_format::FLOAT32, "fc", 0x01020304);
        let b = h.to_bytes();
        assert_eq!(b[24], 0x04);
        assert_eq!(b[25], 0x03);
        assert_eq!(b[26], 0x02);
        assert_eq!(b[27], 0x01);
    }

    #[test]
    fn header_sub_protocol_audio() {
        assert_eq!(make_header().sub_protocol(), sub_proto::AUDIO);
    }

    #[test]
    fn header_byte_offsets() {
        let h = make_header();
        let b = h.to_bytes();
        assert_eq!(&b[0..4], b"VBAN");
        assert_eq!(b[4], h.sr_sub_proto);
        assert_eq!(b[5], h.samples_per_frame_minus1);
        assert_eq!(b[6], h.channels_minus1);
        assert_eq!(b[7], h.data_format_codec);
        assert_eq!(&b[8..24], &h.stream_name);
    }

    #[test]
    fn header_round_trip_all_sample_rates() {
        for (idx, &hz) in SAMPLE_RATES.iter().enumerate() {
            let h = VbanHeader::new_audio(idx as u8, 1, 1, data_format::INT16, "t", 0);
            let decoded = VbanHeader::from_bytes(h.to_bytes()).unwrap();
            assert_eq!(decoded.sample_rate_hz(), Some(hz));
        }
    }

    #[test]
    fn header_round_trip_all_data_formats() {
        let fmts = [
            data_format::UINT8, data_format::INT16, data_format::INT24,
            data_format::INT32, data_format::FLOAT32, data_format::FLOAT64,
            data_format::INT12, data_format::INT10,
        ];
        for fmt in fmts {
            let h = VbanHeader::new_audio(3, 1, 1, fmt, "t", 0);
            let decoded = VbanHeader::from_bytes(h.to_bytes()).unwrap();
            assert_eq!(decoded.data_format(), fmt);
        }
    }

    // --- bytes_per_sample ---

    #[test]
    fn bytes_per_sample_table() {
        assert_eq!(bytes_per_sample(data_format::UINT8),   Some(1));
        assert_eq!(bytes_per_sample(data_format::INT16),   Some(2));
        assert_eq!(bytes_per_sample(data_format::INT24),   Some(3));
        assert_eq!(bytes_per_sample(data_format::INT32),   Some(4));
        assert_eq!(bytes_per_sample(data_format::FLOAT32), Some(4));
        assert_eq!(bytes_per_sample(data_format::FLOAT64), Some(8));
        assert_eq!(bytes_per_sample(data_format::INT12),   Some(2));
        assert_eq!(bytes_per_sample(data_format::INT10),   Some(2));
    }

    // --- VbanAudioFrame ---

    #[test]
    fn frame_round_trip() {
        let header = make_header();
        let payload: Vec<u8> = (0u8..=255).collect();
        let frame = VbanAudioFrame { header, payload: payload.clone() };
        let wire = frame.to_bytes();
        assert_eq!(wire.len(), HEADER_SIZE + payload.len());
        let decoded = VbanAudioFrame::from_bytes(&wire).expect("decode failed");
        assert_eq!(decoded, frame);
    }

    #[test]
    fn frame_empty_payload() {
        let frame = VbanAudioFrame { header: make_header(), payload: vec![] };
        let wire = frame.to_bytes();
        assert_eq!(wire.len(), HEADER_SIZE);
        let decoded = VbanAudioFrame::from_bytes(&wire).unwrap();
        assert_eq!(decoded.payload, Vec::<u8>::new());
    }

    #[test]
    fn frame_too_short_returns_none() {
        assert!(VbanAudioFrame::from_bytes(&[0u8; 10]).is_none());
    }

    #[test]
    fn frame_bad_magic_returns_none() {
        let mut wire = VbanAudioFrame { header: make_header(), payload: vec![1, 2, 3] }.to_bytes();
        wire[0] = b'X';
        assert!(VbanAudioFrame::from_bytes(&wire).is_none());
    }

    #[test]
    fn frame_expected_payload_size_int16_stereo_255() {
        // 255 samples × 2 channels × 2 bytes (Int16) = 1020
        assert_eq!(make_header().samples_per_frame(), 255);
        let frame = VbanAudioFrame { header: make_header(), payload: vec![] };
        assert_eq!(frame.expected_payload_size(), Some(255 * 2 * 2));
    }

    #[test]
    fn frame_expected_payload_size_float32_mono_128() {
        let h = VbanHeader::new_audio(16, 128, 1, data_format::FLOAT32, "mono", 0);
        let frame = VbanAudioFrame { header: h, payload: vec![] };
        // 128 × 1 × 4 = 512
        assert_eq!(frame.expected_payload_size(), Some(512));
    }

    // --- Serde round-trip ---

    #[test]
    fn header_serde_round_trip() {
        let h = make_header();
        let json = serde_json::to_string(&h).unwrap();
        let decoded: VbanHeader = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, h);
    }

    #[test]
    fn frame_serde_round_trip() {
        let frame = VbanAudioFrame {
            header: make_header(),
            payload: vec![0xAB, 0xCD, 0xEF],
        };
        let json = serde_json::to_string(&frame).unwrap();
        let decoded: VbanAudioFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, frame);
    }
}
