// SPDX-License-Identifier: AGPL-3.0-only
//! A deterministic in-process [`AudioSource`] for CI and encoder tests.

use async_trait::async_trait;

use crate::error::AudioError;
use crate::frame::{samples_per_frame, AudioFormat, AudioFrame, FRAME_MICROS};
use crate::traits::AudioSource;

/// Default tone frequency, in Hz.
pub const DEFAULT_TONE_HZ: f64 = 440.0;

/// Number of frames a default fake source produces before ending the stream.
pub const DEFAULT_FRAME_LIMIT: u64 = 10;

/// A deterministic audio source: a sine tone or silence, sampled at a fixed
/// rate, with strictly increasing PTS and sequence numbers.
///
/// The waveform is generated from the absolute sample index, so it is
/// continuous across frames and identical on every run. This makes it useful
/// for Opus roundtrip tests without a sound card.
#[derive(Clone, Debug)]
pub struct FakeAudioSource {
    id: String,
    format: AudioFormat,
    frame_micros: u32,
    tone_hz: Option<f64>,
    frame_limit: Option<u64>,
    start_sequence: u64,
    next_sequence: u64,
    pts_origin: Option<u64>,
    stopped: bool,
}

impl Default for FakeAudioSource {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeAudioSource {
    /// A 48 kHz stereo 440 Hz source producing 10 frames of 20 ms each.
    pub fn new() -> Self {
        Self {
            id: "fake0".to_owned(),
            format: AudioFormat::opus(),
            frame_micros: FRAME_MICROS,
            tone_hz: Some(DEFAULT_TONE_HZ),
            frame_limit: Some(DEFAULT_FRAME_LIMIT),
            start_sequence: 0,
            next_sequence: 0,
            pts_origin: None,
            stopped: false,
        }
    }

    /// Override the stable source id.
    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = id.into();
        self
    }

    /// Stop after `count` frames with [`AudioError::EndOfStream`].
    ///
    /// `count == 0` makes the very first call end the stream.
    pub fn with_frame_limit(mut self, count: u64) -> Self {
        self.frame_limit = Some(count);
        self
    }

    /// Produce frames forever.
    pub fn unlimited(mut self) -> Self {
        self.frame_limit = None;
        self
    }

    /// Emit silence instead of a tone.
    pub fn with_silence(mut self) -> Self {
        self.tone_hz = None;
        self
    }

    /// Emit a sine tone at `hz`. A non-finite or non-positive value falls back
    /// to silence so a test can never generate NaNs.
    pub fn with_tone_hz(mut self, hz: f64) -> Self {
        self.tone_hz = (hz.is_finite() && hz > 0.0).then_some(hz);
        self
    }

    /// Begin sequence numbers at `sequence` instead of zero.
    ///
    /// PTS starts at `sequence * frame_duration`, keeping PTS and sequence
    /// aligned, unless [`FakeAudioSource::with_pts_start`] overrides it.
    pub fn with_start_sequence(mut self, sequence: u64) -> Self {
        self.start_sequence = sequence;
        self.next_sequence = sequence;
        self
    }

    /// Start timestamps at `pts_micros` instead of deriving them from the
    /// starting sequence number.
    pub fn with_pts_start(mut self, pts_micros: u64) -> Self {
        self.pts_origin = Some(pts_micros);
        self
    }

    /// Override the sample rate used for generated frames.
    pub fn with_sample_rate(mut self, sample_rate: u32) -> Self {
        self.format.sample_rate = sample_rate;
        self
    }

    /// Override the channel count used for generated frames.
    pub fn with_channels(mut self, channels: u8) -> Self {
        self.format.channels = channels;
        self
    }

    /// Override the frame duration in microseconds.
    pub fn with_frame_micros(mut self, frame_micros: u32) -> Self {
        self.frame_micros = frame_micros;
        self
    }

    /// Number of frames emitted so far.
    pub fn frames_emitted(&self) -> u64 {
        self.next_sequence.saturating_sub(self.start_sequence)
    }

    fn frame_samples(&self) -> Result<usize, AudioError> {
        match samples_per_frame(self.format.sample_rate, self.frame_micros) {
            Some(samples) if samples > 0 => Ok(samples),
            _ => Err(AudioError::InvalidFrame(format!(
                "cannot generate frames at {} Hz over {} us",
                self.format.sample_rate, self.frame_micros
            ))),
        }
    }
}

#[async_trait]
impl AudioSource for FakeAudioSource {
    fn id(&self) -> &str {
        &self.id
    }

    fn format(&self) -> AudioFormat {
        self.format.clone()
    }

    async fn next_frame(&mut self) -> Result<AudioFrame, AudioError> {
        if self.stopped {
            return Err(AudioError::Closed {
                id: self.id.clone(),
            });
        }
        if let Some(limit) = self.frame_limit {
            if self.frames_emitted() >= limit {
                return Err(AudioError::EndOfStream);
            }
        }

        let samples_per_channel = self.frame_samples()?;
        let channels = usize::from(self.format.channels);
        let sample_offset = self
            .frames_emitted()
            .saturating_mul(samples_per_channel as u64);

        let mut samples = Vec::with_capacity(samples_per_channel * channels);
        match self.tone_hz {
            None => samples.resize(samples_per_channel * channels, 0),
            Some(hz) => {
                let rate = f64::from(self.format.sample_rate);
                for index in 0..samples_per_channel {
                    let absolute = sample_offset.saturating_add(index as u64) as f64;
                    let value = (std::f64::consts::TAU * hz * absolute / rate).sin() * 0.25;
                    let sample = (value * f64::from(i16::MAX)).round() as i16;
                    for _ in 0..channels {
                        samples.push(sample);
                    }
                }
            }
        }

        let emitted = self.frames_emitted();
        let pts_base = self.pts_origin.unwrap_or_else(|| {
            self.start_sequence
                .saturating_mul(u64::from(self.frame_micros))
        });
        let frame = AudioFrame::new(
            pts_base.saturating_add(emitted.saturating_mul(u64::from(self.frame_micros))),
            self.next_sequence,
            self.format.sample_rate,
            self.format.channels,
            samples,
        )?;
        self.next_sequence = self.next_sequence.saturating_add(1);
        Ok(frame)
    }

    async fn stop(&mut self) -> Result<(), AudioError> {
        self.stopped = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{CHANNELS, SAMPLE_RATE};

    #[tokio::test]
    async fn frames_are_non_empty_and_strictly_monotonic() {
        let mut source = FakeAudioSource::new().with_frame_limit(5);
        let format = source.format();
        assert_eq!(format.sample_rate, SAMPLE_RATE);
        assert_eq!(format.channels, CHANNELS);

        let mut previous_pts = None;
        let mut previous_sequence = None;
        for expected_sequence in 0..5 {
            let frame = source.next_frame().await.unwrap();
            assert!(!frame.is_empty());
            assert_eq!(frame.samples.len(), 960 * 2);
            assert_eq!(frame.sequence, expected_sequence);
            if let Some(previous) = previous_pts {
                assert!(frame.pts_micros > previous, "PTS must increase");
            }
            if let Some(previous) = previous_sequence {
                assert!(frame.sequence > previous, "sequence must increase");
            }
            previous_pts = Some(frame.pts_micros);
            previous_sequence = Some(frame.sequence);
        }

        assert_eq!(source.frames_emitted(), 5);
        assert_eq!(
            source.next_frame().await.unwrap_err(),
            AudioError::EndOfStream
        );
    }

    #[tokio::test]
    async fn tone_is_deterministic_and_non_silent() {
        let mut first = FakeAudioSource::new().with_frame_limit(1);
        let mut second = FakeAudioSource::new().with_frame_limit(1);
        let a = first.next_frame().await.unwrap();
        let b = second.next_frame().await.unwrap();
        assert_eq!(a.samples, b.samples);
        assert!(a.samples.iter().any(|sample| *sample != 0));
    }

    #[tokio::test]
    async fn silence_is_all_zero() {
        let mut source = FakeAudioSource::new().with_silence().with_frame_limit(2);
        let frame = source.next_frame().await.unwrap();
        assert!(frame.samples.iter().all(|sample| *sample == 0));
    }

    #[tokio::test]
    async fn start_sequence_offsets_both_metadata_fields() {
        let mut source = FakeAudioSource::new()
            .with_start_sequence(41)
            .with_frame_limit(2);
        let first = source.next_frame().await.unwrap();
        let second = source.next_frame().await.unwrap();
        assert_eq!(first.sequence, 41);
        assert_eq!(first.pts_micros, 41 * 20_000);
        assert_eq!(second.sequence, 42);
        assert_eq!(second.pts_micros, 42 * 20_000);
    }

    #[tokio::test]
    async fn stop_is_idempotent_and_closes_the_source() {
        let mut source = FakeAudioSource::new();
        source.stop().await.unwrap();
        source.stop().await.unwrap();
        assert!(matches!(
            source.next_frame().await.unwrap_err(),
            AudioError::Closed { .. }
        ));
    }

    #[tokio::test]
    async fn zero_frame_limit_ends_immediately() {
        let mut source = FakeAudioSource::new().with_frame_limit(0);
        assert_eq!(
            source.next_frame().await.unwrap_err(),
            AudioError::EndOfStream
        );
    }

    #[tokio::test]
    async fn invalid_generation_parameters_are_structured_errors() {
        let mut source = FakeAudioSource::new().with_sample_rate(0);
        assert!(matches!(
            source.next_frame().await.unwrap_err(),
            AudioError::InvalidFrame(_)
        ));
    }
}
