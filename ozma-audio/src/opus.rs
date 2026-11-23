// SPDX-License-Identifier: AGPL-3.0-only
//! Opus encoding (and a small decoder for roundtrip tests).
//!
//! [`OpusEncoder`] wraps `audiopus` (libopus) and encodes one 20 ms, 48 kHz,
//! stereo S16 frame into exactly one Opus packet. Opus packets are
//! independently decodable: there is no inter-packet prediction, so every
//! [`EncodedAudio`] this encoder produces reports `independent == true`. A
//! decoder can start at any packet, which is exactly the "keyframe" behaviour
//! the packetiser needs for audio.
//!
//! The encoder is deliberately thin: rate control and packetisation belong to
//! the pipeline and transport, not here. It does not resample or remix; a
//! frame with the wrong rate, channel count, or sample count is rejected with
//! [`AudioError::FormatMismatch`].

use std::convert::TryFrom;

use async_trait::async_trait;
use audiopus::{
    coder::{Decoder, Encoder},
    packet::Packet,
    Application, Bitrate, Channels, MutSignals, SampleRate,
};

use crate::error::AudioError;
use crate::frame::{format_label, AudioFrame, CHANNELS, SAMPLE_RATE};
use crate::traits::{AudioEncoder, EncodedAudio, EncoderConfig};

/// Stable encoder identifier.
pub const OPUS_ENCODER_ID: &str = "opus";

/// Upper bound on an Opus packet for one frame. libopus will never emit more
/// than 1275 bytes for a 20 ms stereo frame; the larger buffer is harmless and
/// leaves room for the 60 ms mode.
pub const MAX_OPUS_PACKET_BYTES: usize = 4_000;

/// Packet durations Opus defines, in microseconds.
const OPUS_FRAME_MICROS: [u32; 6] = [2_500, 5_000, 10_000, 20_000, 40_000, 60_000];

/// The interactive encoder: Opus, 48 kHz, stereo, 20 ms packets, 128 kbps.
pub fn default_config() -> EncoderConfig {
    EncoderConfig::opus()
}

fn config_error(config: &EncoderConfig) -> Option<AudioError> {
    let actual = format!(
        "{} over {} us",
        format_label(&config.format),
        config.frame_micros
    );
    let expected = "opus 48000 Hz 2 ch at 2.5/5/10/20/40/60 ms".to_owned();
    if config.format.codec != ozma_protocol::Codec::Opus
        || config.format.sample_rate != SAMPLE_RATE
        || config.format.channels != CHANNELS
        || !OPUS_FRAME_MICROS.contains(&config.frame_micros)
    {
        return Some(AudioError::FormatMismatch { expected, actual });
    }
    if config.samples_per_frame() == 0 {
        return Some(AudioError::FormatMismatch { expected, actual });
    }
    None
}

fn validate_frame(
    frame: &AudioFrame,
    config: &EncoderConfig,
    expected_samples: usize,
) -> Result<(), AudioError> {
    let actual = frame.format_label();
    let expected = format!(
        "{} Hz {} ch {} samples",
        config.format.sample_rate, config.format.channels, expected_samples
    );
    if frame.sample_rate != config.format.sample_rate
        || frame.channels != config.format.channels
        || frame.frames_per_channel() != expected_samples
    {
        return Err(AudioError::FormatMismatch { expected, actual });
    }
    Ok(())
}

/// A libopus-backed encoder for 20 ms 48 kHz stereo frames.
#[derive(Debug)]
pub struct OpusEncoder {
    encoder: Encoder,
    config: EncoderConfig,
}

impl OpusEncoder {
    /// Build an encoder for `config`.
    ///
    /// Fails with [`AudioError::FormatMismatch`] when the config is not Opus
    /// at 48 kHz stereo with a legal Opus packet duration, and with
    /// [`AudioError::Encode`] when libopus refuses to initialise.
    pub fn new(config: EncoderConfig) -> Result<Self, AudioError> {
        if let Some(error) = config_error(&config) {
            return Err(error);
        }

        let mut encoder = Encoder::new(SampleRate::Hz48000, Channels::Stereo, Application::Audio)
            .map_err(|error| {
            AudioError::encode(
                OPUS_ENCODER_ID,
                format!("failed to initialise libopus: {error}"),
            )
        })?;

        let bitrate = if config.format.bitrate_kbps == 0 {
            Bitrate::Auto
        } else {
            let bits = u64::from(config.format.bitrate_kbps)
                .saturating_mul(1_000)
                .min(i32::MAX as u64) as i32;
            Bitrate::BitsPerSecond(bits)
        };
        encoder.set_bitrate(bitrate).map_err(|error| {
            AudioError::encode(OPUS_ENCODER_ID, format!("set_bitrate failed: {error}"))
        })?;
        // Interactive audio: full complexity is affordable at 48 kHz stereo
        // and keeps quality high on modest node CPUs.
        encoder.set_complexity(10).map_err(|error| {
            AudioError::encode(OPUS_ENCODER_ID, format!("set_complexity failed: {error}"))
        })?;
        // Loss-tolerant defaults; the transport owns the actual FEC policy.
        encoder.set_inband_fec(true).map_err(|error| {
            AudioError::encode(OPUS_ENCODER_ID, format!("set_inband_fec failed: {error}"))
        })?;
        encoder.set_packet_loss_perc(10).map_err(|error| {
            AudioError::encode(
                OPUS_ENCODER_ID,
                format!("set_packet_loss_perc failed: {error}"),
            )
        })?;

        Ok(Self { encoder, config })
    }

    /// Encoder identifier, always [`OPUS_ENCODER_ID`].
    pub fn name(&self) -> &'static str {
        OPUS_ENCODER_ID
    }
}

#[async_trait]
impl AudioEncoder for OpusEncoder {
    fn id(&self) -> &str {
        OPUS_ENCODER_ID
    }

    fn config(&self) -> EncoderConfig {
        self.config.clone()
    }

    async fn encode(&mut self, frame: AudioFrame) -> Result<EncodedAudio, AudioError> {
        let expected_samples = self.config.samples_per_frame();
        validate_frame(&frame, &self.config, expected_samples)?;

        let mut output = vec![0u8; MAX_OPUS_PACKET_BYTES];
        let written = self
            .encoder
            .encode(&frame.samples, &mut output)
            .map_err(|error| {
                AudioError::encode(OPUS_ENCODER_ID, format!("libopus encode failed: {error}"))
            })?;
        if written == 0 {
            return Err(AudioError::encode(
                OPUS_ENCODER_ID,
                "libopus produced an empty packet",
            ));
        }
        output.truncate(written);

        EncodedAudio::new(
            frame.pts_micros,
            frame.sequence,
            frame.sample_rate,
            frame.channels,
            output,
            true, // Opus packets never depend on earlier packets.
            frame.duration_micros(),
        )
    }

    async fn flush(&mut self) -> Result<Vec<EncodedAudio>, AudioError> {
        // One input frame produces one packet; libopus keeps no packet-sized
        // buffer that needs draining at end of stream.
        Ok(Vec::new())
    }
}

/// A libopus-backed decoder, used by tests and local monitoring.
///
/// This is intentionally small: it does not do packet-loss concealment,
/// jitter buffering, or resampling. Those belong to a playback pipeline.
#[derive(Debug)]
pub struct OpusDecoder {
    decoder: Decoder,
    config: EncoderConfig,
}

impl OpusDecoder {
    /// Build a decoder matching `config`.
    pub fn new(config: EncoderConfig) -> Result<Self, AudioError> {
        if let Some(error) = config_error(&config) {
            return Err(error);
        }
        let decoder = Decoder::new(SampleRate::Hz48000, Channels::Stereo).map_err(|error| {
            AudioError::Decode {
                message: format!("failed to initialise libopus decoder: {error}"),
            }
        })?;
        Ok(Self { decoder, config })
    }

    /// Decode one packet into an [`AudioFrame`], preserving PTS and sequence.
    pub fn decode(&mut self, packet: &EncodedAudio) -> Result<AudioFrame, AudioError> {
        if packet.sample_rate != self.config.format.sample_rate
            || packet.channels != self.config.format.channels
        {
            return Err(AudioError::FormatMismatch {
                expected: format!(
                    "{} Hz {} ch",
                    self.config.format.sample_rate, self.config.format.channels
                ),
                actual: format!("{} Hz {} ch", packet.sample_rate, packet.channels),
            });
        }

        let input =
            Packet::try_from(packet.data.as_slice()).map_err(|error| AudioError::Decode {
                message: format!("invalid Opus packet: {error}"),
            })?;
        let channels = usize::from(packet.channels);
        let mut output = vec![0i16; self.config.samples_per_frame() * channels];
        let signals =
            MutSignals::try_from(output.as_mut_slice()).map_err(|error| AudioError::Decode {
                message: format!("invalid decode buffer: {error}"),
            })?;
        let decoded = self
            .decoder
            .decode(Some(input), signals, false)
            .map_err(|error| AudioError::Decode {
                message: format!("libopus decode failed: {error}"),
            })?;
        if decoded == 0 {
            return Err(AudioError::Decode {
                message: "libopus returned an empty frame".to_owned(),
            });
        }
        output.truncate(decoded * channels);

        AudioFrame::new(
            packet.pts_micros,
            packet.sequence,
            packet.sample_rate,
            packet.channels,
            output,
        )
    }

    /// The config this decoder was built for.
    pub fn config(&self) -> EncoderConfig {
        self.config.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeAudioSource;
    use crate::frame::{FRAME_MICROS, FRAME_SAMPLES};
    use crate::traits::AudioSource;

    #[tokio::test]
    async fn encodes_non_empty_packets_and_preserves_metadata() {
        let mut source = FakeAudioSource::new().with_frame_limit(3);
        let mut encoder = OpusEncoder::new(default_config()).unwrap();
        assert_eq!(encoder.id(), OPUS_ENCODER_ID);
        assert_eq!(encoder.config().frame_micros, FRAME_MICROS);

        let mut previous_sequence = None;
        for expected_sequence in 0..3 {
            let frame = source.next_frame().await.unwrap();
            let pts = frame.pts_micros;
            let packet = encoder.encode(frame).await.unwrap();
            assert!(!packet.data.is_empty());
            assert!(!packet.is_empty());
            assert_eq!(packet.sequence, expected_sequence);
            assert_eq!(packet.pts_micros, pts);
            assert_eq!(packet.sample_rate, SAMPLE_RATE);
            assert_eq!(packet.channels, CHANNELS);
            assert_eq!(packet.duration_micros, 20_000);
            assert!(packet.is_independent());
            assert!(packet.is_keyframe());
            if let Some(previous) = previous_sequence {
                assert_eq!(packet.sequence, previous + 1);
            }
            previous_sequence = Some(packet.sequence);
        }

        assert!(encoder.flush().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn decode_roundtrips_metadata_and_frame_size() {
        let mut source = FakeAudioSource::new().with_frame_limit(1);
        let mut encoder = OpusEncoder::new(default_config()).unwrap();
        let mut decoder = OpusDecoder::new(default_config()).unwrap();

        let frame = source.next_frame().await.unwrap();
        let original_pts = frame.pts_micros;
        let packet = encoder.encode(frame).await.unwrap();
        let decoded = decoder.decode(&packet).unwrap();

        assert_eq!(decoded.sequence, packet.sequence);
        assert_eq!(decoded.pts_micros, original_pts);
        assert_eq!(decoded.sample_rate, SAMPLE_RATE);
        assert_eq!(decoded.channels, CHANNELS);
        assert_eq!(decoded.frames_per_channel(), FRAME_SAMPLES);
        assert!(decoded.samples.iter().any(|sample| *sample != 0));
    }

    #[tokio::test]
    async fn wrong_sample_rate_is_rejected() {
        let mut source = FakeAudioSource::new()
            .with_sample_rate(44_100)
            .with_frame_limit(1);
        let mut encoder = OpusEncoder::new(default_config()).unwrap();
        let frame = source.next_frame().await.unwrap();
        let error = encoder.encode(frame).await.unwrap_err();
        assert!(matches!(error, AudioError::FormatMismatch { .. }));
    }

    #[tokio::test]
    async fn wrong_channel_count_is_rejected() {
        let mut source = FakeAudioSource::new().with_channels(1).with_frame_limit(1);
        let mut encoder = OpusEncoder::new(default_config()).unwrap();
        let frame = source.next_frame().await.unwrap();
        let error = encoder.encode(frame).await.unwrap_err();
        assert!(matches!(error, AudioError::FormatMismatch { .. }));
    }

    #[tokio::test]
    async fn wrong_sample_count_is_rejected() {
        let mut encoder = OpusEncoder::new(default_config()).unwrap();
        let frame = AudioFrame::silent(0, 0, SAMPLE_RATE, CHANNELS, FRAME_SAMPLES / 2).unwrap();
        let error = encoder.encode(frame).await.unwrap_err();
        match error {
            AudioError::FormatMismatch { expected, actual } => {
                assert!(expected.contains("960"));
                assert!(actual.contains("480"));
            }
            other => panic!("expected FormatMismatch, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn empty_frame_is_rejected_by_the_encoder_contract() {
        let mut encoder = OpusEncoder::new(default_config()).unwrap();
        // Public fields allow constructing a frame that bypasses `new`; the
        // encoder still refuses it.
        let frame = AudioFrame {
            pts_micros: 0,
            sequence: 0,
            sample_rate: SAMPLE_RATE,
            channels: CHANNELS,
            samples: Vec::new(),
        };
        assert!(matches!(
            encoder.encode(frame).await.unwrap_err(),
            AudioError::FormatMismatch { .. }
        ));
    }

    #[test]
    fn unsupported_encoder_configs_are_rejected() {
        let mut config = default_config();
        config.format.sample_rate = 44_100;
        assert!(matches!(
            OpusEncoder::new(config).unwrap_err(),
            AudioError::FormatMismatch { .. }
        ));

        let mut config = default_config();
        config.frame_micros = 12_345;
        assert!(matches!(
            OpusEncoder::new(config).unwrap_err(),
            AudioError::FormatMismatch { .. }
        ));
    }

    #[test]
    fn decoder_rejects_mismatched_packet_format() {
        let mut decoder = OpusDecoder::new(default_config()).unwrap();
        let packet = EncodedAudio::new(0, 0, 44_100, 2, vec![0u8; 40], true, 20_000).unwrap();
        assert!(matches!(
            decoder.decode(&packet).unwrap_err(),
            AudioError::FormatMismatch { .. }
        ));
    }
}
