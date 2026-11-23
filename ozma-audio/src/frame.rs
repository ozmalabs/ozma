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

/// Re-blocks capture frames into fixed encoder-sized frames.
///
/// PipeWire and ALSA deliver whatever buffer size the graph or device chose
/// (commonly 1024 samples), while Opus is configured for 960-sample (20 ms)
/// packets. This adapter buffers interleaved samples and emits exact frames;
/// it also upmixes mono to stereo. A sample-rate mismatch is rejected rather
/// than silently resampled.
#[derive(Debug)]
pub struct AudioReframer {
    sample_rate: u32,
    channels: u8,
    frame_samples: usize,
    frame_micros: u32,
    pending: Vec<i16>,
    next_pts_micros: u64,
    next_sequence: u64,
}

impl AudioReframer {
    /// Create a reframer for `sample_rate`/`channels` and `frame_micros`
    /// packets.
    pub fn new(sample_rate: u32, channels: u8, frame_micros: u32) -> Result<Self, AudioError> {
        let frame_samples = samples_per_frame(sample_rate, frame_micros)
            .filter(|samples| *samples > 0)
            .ok_or_else(|| {
                AudioError::InvalidFrame(format!(
                    "invalid audio frame size for {sample_rate} Hz / {frame_micros} us"
                ))
            })?;
        if channels == 0 {
            return Err(AudioError::InvalidFrame(
                "channel count must be non-zero".to_owned(),
            ));
        }
        Ok(Self {
            sample_rate,
            channels,
            frame_samples,
            frame_micros,
            pending: Vec::new(),
            next_pts_micros: 0,
            next_sequence: 0,
        })
    }

    /// Create a reframer matching an encoder's configured format.
    pub fn from_config(config: &crate::traits::EncoderConfig) -> Result<Self, AudioError> {
        Self::new(
            config.format.sample_rate,
            config.format.channels,
            config.frame_micros,
        )
    }

    /// Number of interleaved samples held for the next frame.
    pub fn buffered_samples(&self) -> usize {
        self.pending.len() / usize::from(self.channels)
    }

    /// Append a capture frame and return every complete encoder frame.
    pub fn push(&mut self, frame: AudioFrame) -> Result<Vec<AudioFrame>, AudioError> {
        if frame.sample_rate != self.sample_rate {
            return Err(AudioError::InvalidFrame(format!(
                "cannot reframe {} Hz audio for a {} Hz encoder",
                frame.sample_rate, self.sample_rate
            )));
        }
        let was_empty = self.pending.is_empty();
        if was_empty {
            self.next_pts_micros = frame.pts_micros;
        }
        match (frame.channels, self.channels) {
            (source, target) if source == target => {
                self.pending.extend_from_slice(&frame.samples);
            }
            (1, 2) => {
                for sample in frame.samples {
                    self.pending.push(sample);
                    self.pending.push(sample);
                }
            }
            (source, target) => {
                return Err(AudioError::InvalidFrame(format!(
                    "cannot convert {source}-channel audio to {target} channels"
                )));
            }
        }

        let frame_len = self.frame_samples * usize::from(self.channels);
        let mut frames = Vec::new();
        while self.pending.len() >= frame_len {
            let samples: Vec<i16> = self.pending.drain(..frame_len).collect();
            frames.push(AudioFrame::new(
                self.next_pts_micros,
                self.next_sequence,
                self.sample_rate,
                self.channels,
                samples,
            )?);
            self.next_sequence += 1;
            self.next_pts_micros = self.next_pts_micros.saturating_add(u64::from(self.frame_micros));
        }
        Ok(frames)
    }

    /// Zero-pad and emit the remaining partial frame, if any.
    pub fn flush(&mut self) -> Option<AudioFrame> {
        if self.pending.is_empty() {
            return None;
        }
        let frame_len = self.frame_samples * usize::from(self.channels);
        self.pending.resize(frame_len, 0);
        let samples: Vec<i16> = std::mem::take(&mut self.pending);
        let frame = AudioFrame::new(
            self.next_pts_micros,
            self.next_sequence,
            self.sample_rate,
            self.channels,
            samples,
        )
        .ok()?;
        self.next_sequence += 1;
        self.next_pts_micros = self.next_pts_micros.saturating_add(u64::from(self.frame_micros));
        Some(frame)
    }
}

#[cfg(test)]
mod reframer_tests {
    use super::*;

    fn frame(sample_rate: u32, channels: u8, samples: Vec<i16>, pts: u64) -> AudioFrame {
        AudioFrame::new(pts, 0, sample_rate, channels, samples).unwrap()
    }

    #[test]
    fn rebocks_1024_into_960() {
        let mut reframer = AudioReframer::new(48_000, 2, 20_000).unwrap();
        let first: Vec<i16> = (0..1024).flat_map(|value| [value, value]).collect();
        let frames = reframer.push(frame(48_000, 2, first, 100)).unwrap();
        // 1024 samples per channel is one 960 frame plus 64 buffered.
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].samples.len(), 960 * 2);
        assert_eq!(frames[0].pts_micros, 100);
        assert_eq!(reframer.buffered_samples(), 64);

        let second: Vec<i16> = (0..1024).flat_map(|value| [value + 1, value + 1]).collect();
        let frames = reframer.push(frame(48_000, 2, second, 20_100)).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].pts_micros, 20_100);
        assert_eq!(reframer.buffered_samples(), 128);
    }

    #[test]
    fn upmixes_mono_to_stereo() {
        let mut reframer = AudioReframer::new(48_000, 2, 20_000).unwrap();
        let frames = reframer
            .push(frame(48_000, 1, vec![7; 960], 5))
            .unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].samples.len(), 1920);
        assert_eq!(&frames[0].samples[..4], &[7, 7, 7, 7]);
    }

    #[test]
    fn rejects_a_sample_rate_change() {
        let mut reframer = AudioReframer::new(48_000, 2, 20_000).unwrap();
        assert!(reframer.push(frame(44_100, 2, vec![0; 882], 0)).is_err());
    }
}
