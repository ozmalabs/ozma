// SPDX-License-Identifier: AGPL-3.0-only
//! A thin client-side audio decoder.
//!
//! Ozma audio is Opus at 48 kHz stereo. The node encodes one frame per packet
//! with `ozma-audio`'s libopus wrapper, so the client should reuse that same
//! FFI binding rather than carry a second one. [`AudioDecoder`] wraps
//! [`ozma_audio::OpusDecoder`] and exposes a decoded PCM type whose metadata is
//! suitable for a playback queue.
//!
//! This is not a jitter buffer or a resampler. Packets remain independently
//! decodable (every Opus packet is), and a playback pipeline is responsible for
//! ordering, concealment, and A/V sync.

pub use ozma_audio::{AudioError, AudioFrame, EncodedAudio, EncoderConfig, OpusDecoder};

use ozma_audio::FRAME_SAMPLES;

/// PCM produced by decoding one Opus packet.
///
/// Samples are interleaved S16, matching [`AudioFrame`]. The metadata is copied
/// from the encoded packet so a player can preserve PTS and sequence across the
/// decode boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodedAudio {
    /// Presentation timestamp copied from the packet.
    pub pts_micros: u64,
    /// Sequence number copied from the packet.
    pub sequence: u64,
    /// Sample rate of the decoded PCM, in Hz.
    pub sample_rate: u32,
    /// Interleaved channel count.
    pub channels: u8,
    /// Interleaved S16 samples, non-empty.
    pub samples: Vec<i16>,
}

impl DecodedAudio {
    /// Samples per channel in this block.
    pub fn frames_per_channel(&self) -> usize {
        if self.channels == 0 {
            0
        } else {
            self.samples.len() / usize::from(self.channels)
        }
    }

    /// Borrow the interleaved PCM samples.
    pub fn pcm_i16(&self) -> &[i16] {
        &self.samples
    }

    /// Duration of this block in microseconds, rounded down.
    pub fn duration_micros(&self) -> u64 {
        if self.sample_rate == 0 {
            return 0;
        }
        (self.frames_per_channel() as u64)
            .saturating_mul(1_000_000)
            .checked_div(u64::from(self.sample_rate))
            .unwrap_or(0)
    }

    /// Convert back to the capture-side frame type for reuse in `ozma-audio`.
    pub fn to_audio_frame(&self) -> Result<AudioFrame, AudioError> {
        AudioFrame::new(
            self.pts_micros,
            self.sequence,
            self.sample_rate,
            self.channels,
            self.samples.clone(),
        )
    }
}

impl From<AudioFrame> for DecodedAudio {
    fn from(frame: AudioFrame) -> Self {
        Self {
            pts_micros: frame.pts_micros,
            sequence: frame.sequence,
            sample_rate: frame.sample_rate,
            channels: frame.channels,
            samples: frame.samples,
        }
    }
}

/// A libopus decoder configured for the interactive Ozma audio format.
#[derive(Debug)]
pub struct AudioDecoder {
    inner: OpusDecoder,
}

impl AudioDecoder {
    /// Build a decoder for Opus, 48 kHz stereo, 20 ms packets.
    ///
    /// # Errors
    ///
    /// Returns [`AudioError::Decode`] if libopus cannot create a decoder.
    pub fn new() -> Result<Self, AudioError> {
        Self::with_config(EncoderConfig::opus())
    }

    /// Build a decoder for an explicit config.
    ///
    /// The config is validated by `ozma-audio`; only Opus at 48 kHz stereo with
    /// a legal packet duration is accepted.
    pub fn with_config(config: EncoderConfig) -> Result<Self, AudioError> {
        Ok(Self {
            inner: OpusDecoder::new(config)?,
        })
    }

    /// The config this decoder was built for.
    pub fn config(&self) -> EncoderConfig {
        self.inner.config()
    }

    /// Decode one [`EncodedAudio`] packet, preserving PTS and sequence.
    pub fn decode(&mut self, packet: &EncodedAudio) -> Result<DecodedAudio, AudioError> {
        self.inner.decode(packet).map(DecodedAudio::from)
    }

    /// Decode one raw Opus packet received from the transport.
    ///
    /// This builds the [`EncodedAudio`] wrapper with the decoder's own sample
    /// rate, channel count, and frame duration. Empty payloads are rejected by
    /// the wrapper's constructor.
    pub fn decode_bytes(
        &mut self,
        data: &[u8],
        pts_micros: u64,
        sequence: u64,
    ) -> Result<DecodedAudio, AudioError> {
        let config = self.config();
        let packet = EncodedAudio::new(
            pts_micros,
            sequence,
            config.format.sample_rate,
            config.format.channels,
            data.to_vec(),
            true,
            u64::from(config.frame_micros),
        )?;
        self.decode(&packet)
    }
}

/// The canonical client audio format: Opus, 48 kHz, stereo, 20 ms.
pub fn format() -> EncoderConfig {
    EncoderConfig::opus()
}

/// Why a decoded block has the size it does.
pub const EXPECTED_FRAME_SAMPLES: usize = FRAME_SAMPLES;

#[cfg(test)]
mod tests {
    use super::*;
    use ozma_audio::{CHANNELS, SAMPLE_RATE};

    fn decoded() -> DecodedAudio {
        DecodedAudio {
            pts_micros: 20_000,
            sequence: 4,
            sample_rate: SAMPLE_RATE,
            channels: CHANNELS,
            samples: vec![1, -1, 2, -2],
        }
    }

    #[test]
    fn pcm_access_and_metadata_are_stable() {
        let audio = decoded();
        assert_eq!(audio.pcm_i16(), &[1, -1, 2, -2]);
        assert_eq!(audio.frames_per_channel(), 2);
        assert_eq!(audio.duration_micros(), 41); // 2 samples at 48 kHz, rounded down
        assert_eq!(audio.to_audio_frame().unwrap().frames_per_channel(), 2);
    }

    #[test]
    fn the_canonical_format_matches_ozma_audio() {
        let config = format();
        assert_eq!(config, EncoderConfig::opus());
        assert_eq!(EXPECTED_FRAME_SAMPLES, FRAME_SAMPLES);
    }

    #[test]
    fn empty_packets_are_rejected_structurally() {
        let mut decoder = AudioDecoder::new().unwrap();
        let error = decoder.decode_bytes(&[], 0, 0).unwrap_err();
        assert!(matches!(error, AudioError::InvalidFrame(_)));
    }
}
