// SPDX-License-Identifier: AGPL-3.0-only
//! The audio capture and encode boundaries.
//!
//! Both traits are asynchronous and object-safe, mirroring the video
//! `CaptureSource`/`Encoder` pair in `ozma-media`. An [`AudioSource`] owns its
//! device and yields frames until it reports [`AudioError::EndOfStream`]; an
//! [`AudioEncoder`] consumes one frame at a time and returns exactly one
//! independently decodable packet.

use async_trait::async_trait;

use crate::error::AudioError;
use crate::frame::{AudioFormat, AudioFrame};

/// An asynchronous source of captured audio frames.
///
/// Implementations must produce monotonically increasing `sequence` and
/// `pts_micros` values and non-empty sample buffers. The pipeline treats
/// [`AudioError::EndOfStream`] as a clean shutdown and every other error as a
/// failure that stops the track.
#[async_trait]
pub trait AudioSource: Send {
    /// Stable identifier, for example `default`, `hw:1,0`, or `fake0`.
    fn id(&self) -> &str;

    /// Format of the frames this source will produce.
    fn format(&self) -> AudioFormat;

    /// Wait for the next frame.
    ///
    /// Implementations must not return an empty frame. A frame with a partial
    /// period is allowed at the source boundary; the encoder rejects it.
    async fn next_frame(&mut self) -> Result<AudioFrame, AudioError>;

    /// Stop the device and release resources. Must be idempotent.
    async fn stop(&mut self) -> Result<(), AudioError>;
}

/// The properties an [`AudioEncoder`] is configured with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncoderConfig {
    /// The format of frames this encoder accepts. Bitrate describes the
    /// encoder target, not the input.
    pub format: AudioFormat,
    /// Duration of one encoded packet in microseconds, normally 20 ms.
    pub frame_micros: u32,
}

impl EncoderConfig {
    /// The canonical interactive config: Opus, 48 kHz, stereo, 20 ms.
    pub fn opus() -> Self {
        Self {
            format: AudioFormat::opus(),
            frame_micros: crate::frame::FRAME_MICROS,
        }
    }

    /// Samples per channel this encoder requires in each frame.
    pub fn samples_per_frame(&self) -> usize {
        crate::frame::samples_per_frame(self.format.sample_rate, self.frame_micros).unwrap_or(0)
    }
}

impl Default for EncoderConfig {
    fn default() -> Self {
        Self::opus()
    }
}

/// One encoded audio packet with the metadata the packetiser needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncodedAudio {
    /// Presentation timestamp inherited from the captured frame.
    pub pts_micros: u64,
    /// Sequence number inherited from the captured frame.
    pub sequence: u64,
    /// Sample rate of the decoded audio, in Hz.
    pub sample_rate: u32,
    /// Channel count of the decoded audio.
    pub channels: u8,
    /// Encoded packet bytes, non-empty.
    pub data: Vec<u8>,
    /// Whether this packet can be decoded without any earlier packet.
    pub independent: bool,
    /// Duration of the packet in microseconds.
    pub duration_micros: u64,
}

impl EncodedAudio {
    /// Build a packet, rejecting an empty payload.
    pub fn new(
        pts_micros: u64,
        sequence: u64,
        sample_rate: u32,
        channels: u8,
        data: Vec<u8>,
        independent: bool,
        duration_micros: u64,
    ) -> Result<Self, AudioError> {
        if data.is_empty() {
            return Err(AudioError::InvalidFrame(
                "encoded audio packet must not be empty".to_owned(),
            ));
        }
        Ok(Self {
            pts_micros,
            sequence,
            sample_rate,
            channels,
            data,
            independent,
            duration_micros,
        })
    }

    /// Whether a decoder can start at this packet.
    ///
    /// Every Opus packet is independently decodable, so Opus encoders set this
    /// to `true`. It exists so a packetiser can make the same decision for
    /// audio as it does for video keyframes.
    pub fn is_independent(&self) -> bool {
        self.independent
    }

    /// Alias for [`EncodedAudio::is_independent`], for callers that reuse a
    /// video-shaped `is_keyframe` check.
    pub fn is_keyframe(&self) -> bool {
        self.independent
    }

    /// Payload length in bytes.
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Always `false`; packets are non-empty by construction.
    pub fn is_empty(&self) -> bool {
        false
    }
}

/// An asynchronous audio encoder.
///
/// The encoder declares the exact [`EncoderConfig`] it accepts so the pipeline
/// can reject a mis-negotiated capture path before any frame is encoded.
#[async_trait]
pub trait AudioEncoder: Send {
    /// Stable identifier, for example `opus` or `fake-opus`.
    fn id(&self) -> &str;

    /// The format and packet duration this encoder accepts and produces.
    fn config(&self) -> EncoderConfig;

    /// Encode one frame.
    ///
    /// Implementations must preserve `pts_micros` and `sequence`, and must
    /// return [`AudioError::FormatMismatch`] for a frame whose rate, channel
    /// count, or sample count differs from [`AudioEncoder::config`].
    async fn encode(&mut self, frame: AudioFrame) -> Result<EncodedAudio, AudioError>;

    /// Drain any buffered packets at end of stream.
    ///
    /// Opus encodes one frame into one packet with no encoder-side buffering,
    /// so the standard implementation returns an empty vector. The method is
    /// part of the trait so a future encoder with lookahead can use it.
    async fn flush(&mut self) -> Result<Vec<EncodedAudio>, AudioError> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{CHANNELS, FRAME_SAMPLES, SAMPLE_RATE};

    #[test]
    fn opus_config_matches_the_frame_constants() {
        let config = EncoderConfig::opus();
        assert_eq!(config.format.sample_rate, SAMPLE_RATE);
        assert_eq!(config.format.channels, CHANNELS);
        assert_eq!(config.frame_micros, 20_000);
        assert_eq!(config.samples_per_frame(), FRAME_SAMPLES);
    }

    #[test]
    fn encoded_packets_must_be_non_empty() {
        let error = EncodedAudio::new(0, 0, 48_000, 2, Vec::new(), true, 20_000).unwrap_err();
        assert!(matches!(error, AudioError::InvalidFrame(_)));
    }

    #[test]
    fn independence_is_reported_two_ways() {
        let packet = EncodedAudio::new(5, 1, 48_000, 2, vec![1, 2, 3], true, 20_000).unwrap();
        assert!(packet.is_independent());
        assert!(packet.is_keyframe());
        assert_eq!(packet.len(), 3);
        assert!(!packet.is_empty());
    }
}
