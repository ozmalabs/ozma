//! Platform-independent PCM types, conversion helpers, and the bounded queue.
//!
//! The native PipeWire capture path always exposes interleaved `i16` samples,
//! even when the graph negotiated `F32LE`.  Playback accepts `i16` and, if the
//! graph insists on `F32LE`, converts on the way out.
//!
//! Keeping this module free of PipeWire symbols means the conversion and queue
//! logic is unit-tested on every platform.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use crate::error::PipeWireError;

/// Sample layout delivered by a native PipeWire stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SampleFormat {
    /// Signed 16-bit little-endian, interleaved.
    S16Le,
    /// 32-bit IEEE float little-endian, interleaved.
    F32Le,
}

impl SampleFormat {
    pub const fn bytes_per_sample(self) -> usize {
        match self {
            Self::S16Le => 2,
            Self::F32Le => 4,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::S16Le => "S16LE",
            Self::F32Le => "F32LE",
        }
    }
}

/// Format actually negotiated with the PipeWire graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NegotiatedFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub sample_format: SampleFormat,
}

impl NegotiatedFormat {
    pub const TARGET_RATE: u32 = 48_000;
    pub const TARGET_CHANNELS: u16 = 2;

    /// Validate rate/channel bounds for a negotiated format.
    pub fn new(
        sample_rate: u32,
        channels: u16,
        sample_format: SampleFormat,
    ) -> Result<Self, PipeWireError> {
        if !(8_000..=192_000).contains(&sample_rate) {
            return Err(PipeWireError::format_mismatch(
                format!("{sample_rate} Hz"),
                "8000..192000 Hz",
            ));
        }
        if !(1..=32).contains(&channels) {
            return Err(PipeWireError::format_mismatch(
                format!("{channels} channels"),
                "1..32 channels",
            ));
        }
        Ok(Self {
            sample_rate,
            channels,
            sample_format,
        })
    }

    /// Bytes per interleaved frame (all channels).
    pub const fn frame_stride(self) -> usize {
        self.channels as usize * self.sample_format.bytes_per_sample()
    }

    /// Duration of `frames` frames in microseconds (rounded down).
    pub const fn frames_to_micros(self, frames: u64) -> u64 {
        if self.sample_rate == 0 {
            0
        } else {
            frames.saturating_mul(1_000_000) / self.sample_rate as u64
        }
    }
}

/// A deinterleaved, normalised PCM frame captured from PipeWire.
///
/// `pts_micros` is measured from stream start using `std::time::Instant`
/// (CLOCK_MONOTONIC).  It is monotonic and suitable for A/V sync; it is not a
/// graph-global PipeWire clock value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PcmFrame {
    /// Monotonic presentation timestamp, microseconds since stream start.
    pub pts_micros: u64,
    /// Monotonically increasing frame counter for this stream.
    pub sequence: u64,
    pub sample_rate: u32,
    pub channels: u16,
    /// Interleaved signed 16-bit samples (`frames * channels` entries).
    pub samples: Vec<i16>,
}

impl PcmFrame {
    /// Number of sample frames (not interleaved samples).
    pub fn frames(&self) -> usize {
        if self.channels == 0 {
            0
        } else {
            self.samples.len() / self.channels as usize
        }
    }

    /// Duration represented by the samples at the frame's own rate.
    pub fn duration_micros(&self) -> u64 {
        if self.sample_rate == 0 {
            0
        } else {
            (self.frames() as u64).saturating_mul(1_000_000) / self.sample_rate as u64
        }
    }

    /// True when the sample count is an exact multiple of the channel count.
    pub fn is_aligned(&self) -> bool {
        self.channels != 0 && self.samples.len() % self.channels as usize == 0
    }
}

/// Convert interleaved S16LE or F32LE bytes into interleaved `i16`.
///
/// Trailing bytes that do not form a complete sample are ignored.  `F32LE`
/// values are clamped to `[-1.0, 1.0]`; NaN becomes silence.
pub fn convert_to_s16(bytes: &[u8], format: SampleFormat) -> Result<Vec<i16>, PipeWireError> {
    match format {
        SampleFormat::S16Le => {
            let count = bytes.len() / 2;
            let mut out = Vec::with_capacity(count);
            for chunk in bytes[..count * 2].chunks_exact(2) {
                out.push(i16::from_le_bytes([chunk[0], chunk[1]]));
            }
            Ok(out)
        }
        SampleFormat::F32Le => {
            let count = bytes.len() / 4;
            let mut out = Vec::with_capacity(count);
            for chunk in bytes[..count * 4].chunks_exact(4) {
                let f = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                out.push(f32_to_i16(f));
            }
            Ok(out)
        }
    }
}

/// Convert one float sample to signed 16-bit with clamping and rounding.
pub fn f32_to_i16(value: f32) -> i16 {
    if value.is_nan() {
        return 0;
    }
    let scaled = (value.clamp(-1.0, 1.0) * 32_767.0).round();
    scaled.clamp(i16::MIN as f32, i16::MAX as f32) as i16
}

/// Encode `i16` samples as little-endian bytes.
pub fn s16_to_le_bytes(samples: &[i16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 2);
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// Encode `i16` samples as little-endian `f32` bytes.
pub fn s16_to_f32_le_bytes(samples: &[i16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 4);
    for s in samples {
        let f = *s as f32 / 32_768.0;
        out.extend_from_slice(&f.to_le_bytes());
    }
    out
}

/// Bounded FIFO of interleaved samples shared between the async producer and
/// the realtime PipeWire process callback.
///
/// The queue never grows past its configured capacity.  [`PcmQueue::push`]
/// accepts as much as fits and reports how many samples were stored.
#[derive(Debug, Clone)]
pub struct PcmQueue {
    samples: VecDeque<i16>,
    capacity: usize,
}

impl PcmQueue {
    pub fn with_capacity(capacity_samples: usize) -> Self {
        let capacity = capacity_samples.max(1);
        Self {
            samples: VecDeque::with_capacity(capacity.min(1 << 16)),
            capacity,
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    pub fn available(&self) -> usize {
        self.capacity.saturating_sub(self.samples.len())
    }

    /// Append samples, returning the number accepted (may be a prefix).
    pub fn push(&mut self, samples: &[i16]) -> usize {
        let take = samples.len().min(self.available());
        self.samples.extend(samples[..take].iter().copied());
        take
    }

    /// Pop one sample.
    pub fn pop_one(&mut self) -> Option<i16> {
        self.samples.pop_front()
    }

    /// Pop up to `out.len()` samples into `out`, returning the count.
    pub fn pop_into(&mut self, out: &mut [i16]) -> usize {
        let take = out.len().min(self.samples.len());
        for slot in out.iter_mut().take(take) {
            *slot = self.samples.pop_front().expect("len checked");
        }
        take
    }

    pub fn clear(&mut self) {
        self.samples.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s16_passthrough_is_little_endian() {
        let bytes = [0x01, 0x00, 0xff, 0xff, 0x00, 0x80];
        let samples = convert_to_s16(&bytes, SampleFormat::S16Le).unwrap();
        assert_eq!(samples, vec![1, -1, i16::MIN]);
    }

    #[test]
    fn f32_conversion_clamps_and_rounds() {
        let values = [0.0f32, 0.5, -0.5, 1.0, -1.0, 2.0, -2.0, f32::NAN];
        let mut bytes = Vec::new();
        for v in values {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        let samples = convert_to_s16(&bytes, SampleFormat::F32Le).unwrap();
        assert_eq!(samples[0], 0);
        assert_eq!(samples[1], 16_384); // 0.5 * 32767 = 16383.5 -> 16384
        assert_eq!(samples[2], -16_384);
        assert_eq!(samples[3], i16::MAX);
        assert_eq!(samples[4], -i16::MAX);
        assert_eq!(samples[5], i16::MAX);
        assert_eq!(samples[6], -i16::MAX);
        assert_eq!(samples[7], 0);
    }

    #[test]
    fn conversion_ignores_trailing_partial_sample() {
        let bytes = [0x01, 0x00, 0x02];
        let samples = convert_to_s16(&bytes, SampleFormat::S16Le).unwrap();
        assert_eq!(samples, vec![1]);
        let bytes = [0u8; 7];
        let samples = convert_to_s16(&bytes, SampleFormat::F32Le).unwrap();
        assert_eq!(samples, vec![0]);
    }

    #[test]
    fn encode_s16_and_f32() {
        assert_eq!(s16_to_le_bytes(&[1, -1]), vec![1, 0, 0xff, 0xff]);
        let f32_bytes = s16_to_f32_le_bytes(&[0, 16_384]);
        assert_eq!(f32::from_le_bytes(f32_bytes[0..4].try_into().unwrap()), 0.0);
        assert_eq!(
            f32::from_le_bytes(f32_bytes[4..8].try_into().unwrap()),
            16384.0 / 32768.0
        );
    }

    #[test]
    fn format_bounds_are_enforced() {
        assert!(NegotiatedFormat::new(48_000, 2, SampleFormat::S16Le).is_ok());
        let e = NegotiatedFormat::new(0, 2, SampleFormat::S16Le).unwrap_err();
        assert_eq!(e.code(), "pipewire.format_mismatch");
        let e = NegotiatedFormat::new(48_000, 0, SampleFormat::S16Le).unwrap_err();
        assert_eq!(e.code(), "pipewire.format_mismatch");
        let e = NegotiatedFormat::new(48_000, 33, SampleFormat::S16Le).unwrap_err();
        assert_eq!(e.code(), "pipewire.format_mismatch");
    }

    #[test]
    fn frame_reports_frames_duration_alignment() {
        let f = PcmFrame {
            pts_micros: 0,
            sequence: 3,
            sample_rate: 48_000,
            channels: 2,
            samples: vec![0; 960],
        };
        assert_eq!(f.frames(), 480);
        assert_eq!(f.duration_micros(), 10_000);
        assert!(f.is_aligned());
        let odd = PcmFrame {
            samples: vec![0; 3],
            ..f.clone()
        };
        assert!(!odd.is_aligned());
    }

    #[test]
    fn frame_stride_matches_sample_size() {
        let s16 = NegotiatedFormat::new(48_000, 2, SampleFormat::S16Le).unwrap();
        let f32f = NegotiatedFormat::new(48_000, 2, SampleFormat::F32Le).unwrap();
        assert_eq!(s16.frame_stride(), 4);
        assert_eq!(f32f.frame_stride(), 8);
        assert_eq!(s16.frames_to_micros(48_000), 1_000_000);
    }

    #[test]
    fn queue_is_bounded_and_reports_partial_pushes() {
        let mut q = PcmQueue::with_capacity(4);
        assert_eq!(q.capacity(), 4);
        assert_eq!(q.push(&[1, 2, 3]), 3);
        assert_eq!(q.len(), 3);
        assert_eq!(q.available(), 1);
        assert_eq!(q.push(&[4, 5, 6]), 1, "only one sample fits");
        assert_eq!(q.len(), 4);
        assert_eq!(q.push(&[7]), 0);
        let mut out = [0i16; 3];
        assert_eq!(q.pop_into(&mut out), 3);
        assert_eq!(out, [1, 2, 3]);
        assert_eq!(q.pop_one(), Some(4));
        assert_eq!(q.pop_one(), None);
        q.push(&[9, 10]);
        q.clear();
        assert!(q.is_empty());
    }
}
