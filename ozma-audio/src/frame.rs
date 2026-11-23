// SPDX-License-Identifier: AGPL-3.0-only
//! The audio frame and format vocabulary used by capture and encoding.
//!
//! [`AudioFormat`] is the protocol type from `ozma-protocol`; this crate adds
//! the canonical interactive values (Opus, 48 kHz, stereo, 20 ms packets) and
//! a validated [`AudioFrame`] carrying interleaved S16 samples.

pub use ozma_protocol::AudioFormat;

use crate::error::AudioError;

/// Interactive audio sample rate. Opus is defined at 48 kHz and resamples
/// everything else internally, so capture and encode both use it directly.
pub const SAMPLE_RATE: u32 = 48_000;

/// Interactive channel count: stereo.
pub const CHANNELS: u8 = 2;

/// Duration of one audio packet in microseconds (20 ms).
pub const FRAME_MICROS: u32 = 20_000;

/// Frames per channel in a 20 ms packet at [`SAMPLE_RATE`].
pub const FRAME_SAMPLES: usize = 960;

/// Number of samples (per channel) for a frame of `frame_micros` at
/// `sample_rate`. Returns `None` for a zero rate or an overflowing product.
pub fn samples_per_frame(sample_rate: u32, frame_micros: u32) -> Option<usize> {
    if sample_rate == 0 || frame_micros == 0 {
        return None;
    }
    let product = u64::from(sample_rate).checked_mul(u64::from(frame_micros))?;
    usize::try_from(product / 1_000_000).ok()
}

/// The canonical interactive audio format: Opus, 48 kHz, stereo.
pub fn opus_format() -> AudioFormat {
    AudioFormat::opus()
}

/// Human-readable format label for logs and error messages.
pub fn format_label(format: &AudioFormat) -> String {
    format!(
        "{} {} Hz {} ch {} kbps",
        format.codec.as_str(),
        format.sample_rate,
        format.channels,
        format.bitrate_kbps
    )
}

/// One non-empty block of interleaved S16 samples with capture metadata.
///
/// `samples` is interleaved: for stereo, `[L0, R0, L1, R1, ...]`. The number of
/// samples is always a non-zero multiple of `channels`. Build frames with
/// [`AudioFrame::new`] so an empty or ragged buffer is rejected at the
/// boundary rather than reaching the encoder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioFrame {
    /// Presentation timestamp in microseconds on the capture clock.
    pub pts_micros: u64,
    /// Monotonic sequence number assigned by the source.
    pub sequence: u64,
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Interleaved channel count.
    pub channels: u8,
    /// Interleaved S16 samples, non-empty.
    pub samples: Vec<i16>,
}

impl AudioFrame {
    /// Build a frame, rejecting zero rates, zero channels, empty sample
    /// buffers, and buffers that are not a whole number of interleaved frames.
    pub fn new(
        pts_micros: u64,
        sequence: u64,
        sample_rate: u32,
        channels: u8,
        samples: Vec<i16>,
    ) -> Result<Self, AudioError> {
        if sample_rate == 0 {
            return Err(AudioError::InvalidFrame(
                "sample rate must be non-zero".to_owned(),
            ));
        }
        if channels == 0 {
            return Err(AudioError::InvalidFrame(
                "channel count must be non-zero".to_owned(),
            ));
        }
        if samples.is_empty() {
            return Err(AudioError::InvalidFrame(
                "sample buffer must not be empty".to_owned(),
            ));
        }
        if samples.len() % usize::from(channels) != 0 {
            return Err(AudioError::InvalidFrame(format!(
                "{} samples is not a whole number of {channels}-channel frames",
                samples.len()
            )));
        }
        Ok(Self {
            pts_micros,
            sequence,
            sample_rate,
            channels,
            samples,
        })
    }

    /// Build a silent frame of `frames_per_channel` samples.
    pub fn silent(
        pts_micros: u64,
        sequence: u64,
        sample_rate: u32,
        channels: u8,
        frames_per_channel: usize,
    ) -> Result<Self, AudioError> {
        let len = frames_per_channel
            .checked_mul(usize::from(channels))
            .ok_or_else(|| AudioError::InvalidFrame("sample count overflow".to_owned()))?;
        Self::new(pts_micros, sequence, sample_rate, channels, vec![0; len])
    }

    /// Samples per channel in this frame.
    pub fn frames_per_channel(&self) -> usize {
        self.samples.len() / usize::from(self.channels)
    }

    /// Duration of this frame in microseconds, rounded down.
    pub fn duration_micros(&self) -> u64 {
        let samples = self.frames_per_channel() as u64;
        samples
            .saturating_mul(1_000_000)
            .checked_div(u64::from(self.sample_rate))
            .unwrap_or(0)
    }

    /// Always `false`; frames are non-empty by construction.
    pub fn is_empty(&self) -> bool {
        false
    }

    /// Format description without a codec or bitrate, for error messages.
    pub fn format_label(&self) -> String {
        format!(
            "{} Hz {} ch {} samples",
            self.sample_rate,
            self.channels,
            self.frames_per_channel()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_counts_match_20ms() {
        assert_eq!(samples_per_frame(48_000, 20_000), Some(960));
        assert_eq!(samples_per_frame(48_000, 10_000), Some(480));
        assert_eq!(samples_per_frame(0, 20_000), None);
        assert!(samples_per_frame(u32::MAX, u32::MAX).is_some());
    }

    #[test]
    fn empty_and_ragged_frames_are_rejected() {
        assert!(matches!(
            AudioFrame::new(0, 0, 48_000, 2, Vec::new()),
            Err(AudioError::InvalidFrame(_))
        ));
        assert!(matches!(
            AudioFrame::new(0, 0, 48_000, 2, vec![0, 1, 2]),
            Err(AudioError::InvalidFrame(_))
        ));
        assert!(matches!(
            AudioFrame::new(0, 0, 0, 2, vec![0, 1]),
            Err(AudioError::InvalidFrame(_))
        ));
        assert!(matches!(
            AudioFrame::new(0, 0, 48_000, 0, vec![0, 1]),
            Err(AudioError::InvalidFrame(_))
        ));
    }

    #[test]
    fn stereo_frame_metadata_is_correct() {
        let frame = AudioFrame::silent(20_000, 3, 48_000, 2, FRAME_SAMPLES).unwrap();
        assert_eq!(frame.frames_per_channel(), 960);
        assert_eq!(frame.samples.len(), 1920);
        assert_eq!(frame.duration_micros(), 20_000);
        assert!(!frame.is_empty());
        assert_eq!(frame.format_label(), "48000 Hz 2 ch 960 samples");
    }

    #[test]
    fn opus_format_is_the_canonical_value() {
        let format = opus_format();
        assert_eq!(format.sample_rate, SAMPLE_RATE);
        assert_eq!(format.channels, CHANNELS);
        assert_eq!(format.codec.as_str(), "opus");
        assert_eq!(format_label(&format), "opus 48000 Hz 2 ch 128 kbps");
    }
}
