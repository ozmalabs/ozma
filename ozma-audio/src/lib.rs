// SPDX-License-Identifier: AGPL-3.0-only
//! Server-side audio capture and Opus encoding for Ozma nodes.
//!
//! The crate is one half of the media pipeline described in
//! `docs/node-hermes-parity.md` (§8): a capture source produces validated
//! [`AudioFrame`]s, and an [`AudioEncoder`] turns each frame into one
//! independently decodable [`EncodedAudio`] packet for the packetiser and
//! transport.
//!
//! | Type | Role |
//! |---|---|
//! | [`AudioSource`] | async capture boundary, monotonic PTS/sequence |
//! | [`AlsaCapture`] | real ALSA capture, blocking worker + bounded channel |
//! | [`FakeAudioSource`] | deterministic 440 Hz/silence source for CI |
//! | [`AudioEncoder`] | async encode boundary |
//! | [`OpusEncoder`] | libopus, 20 ms 48 kHz stereo, one packet per frame |
//! | [`OpusDecoder`] | small libopus decoder for tests and local checks |
//!
//! # Interactivity choices
//!
//! Capture and encode are fixed to Opus at 48 kHz stereo in 20 ms packets,
//! matching [`ozma_protocol::AudioFormat::opus`]. Opus packets have no
//! inter-packet prediction, so every packet is independently decodable and
//! [`EncodedAudio::is_independent`] is always true.
//!
//! # Limitations
//!
//! - PTS is derived from the capture sample clock, not from kernel ALSA
//!   timestamps. This is drift-free relative to the audio but does not by
//!   itself align audio to a video capture timestamp; a session layer must
//!   establish the A/V offset.
//! - [`AlsaCapture::stop`] joins the worker. The worker checks the stop flag
//!   between 100 ms readiness polls and between frames, so shutdown is
//!   normally bounded by that poll interval even on a stalled device. A drop
//!   without `stop` is best-effort and does not join.
//! - No resampling or downmixing: a frame with the wrong rate, channels, or
//!   sample count is rejected rather than silently reinterpreted.
//! - `audiopus` and `alsa` are native dependencies (libopus, ALSA). No OpenSSL
//!   or other TLS stack is involved.
//!
//! ```no_run
//! use ozma_audio::{AlsaCapture, AlsaCaptureConfig, AudioEncoder, AudioSource, OpusEncoder};
//! use ozma_audio::EncoderConfig;
//!
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let mut source = AlsaCapture::open(AlsaCaptureConfig::default())?;
//! let mut encoder = OpusEncoder::new(EncoderConfig::opus())?;
//! let frame = source.next_frame().await?;
//! let packet = encoder.encode(frame).await?;
//! # let _ = packet;
//! source.stop().await?;
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]

pub mod alsa;
pub mod error;
pub mod fake;
pub mod frame;
pub mod opus;
pub mod traits;

pub use alsa::{
    AlsaCapture, AlsaCaptureConfig, DEFAULT_BUFFER_PERIODS, DEFAULT_PERIOD_FRAMES,
    DEFAULT_START_TIMEOUT, FRAME_CHANNEL_CAPACITY,
};
pub use error::{audio_codes, AudioError};
pub use fake::{FakeAudioSource, DEFAULT_FRAME_LIMIT, DEFAULT_TONE_HZ};
pub use frame::{
    format_label, opus_format, samples_per_frame, AudioFormat, AudioFrame, CHANNELS, FRAME_MICROS,
    FRAME_SAMPLES, SAMPLE_RATE,
};
pub use opus::{default_config, OpusDecoder, OpusEncoder, MAX_OPUS_PACKET_BYTES, OPUS_ENCODER_ID};
pub use traits::{AudioEncoder, AudioSource, EncodedAudio, EncoderConfig};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traits_are_object_safe() {
        let _source: Box<dyn AudioSource> = Box::new(FakeAudioSource::new());
        let _encoder: Box<dyn AudioEncoder> = Box::new(OpusEncoder::new(default_config()).unwrap());
    }

    /// The full software path: fake capture → Opus encode → Opus decode.
    #[tokio::test]
    async fn fake_to_opus_pipeline_is_monotonic_and_decodable() {
        let mut source = FakeAudioSource::new().with_frame_limit(4);
        let mut encoder = OpusEncoder::new(default_config()).unwrap();
        let mut decoder = OpusDecoder::new(default_config()).unwrap();

        let mut last_pts = None;
        let mut last_sequence = None;
        for _ in 0..4 {
            let frame = source.next_frame().await.unwrap();
            let packet = encoder.encode(frame).await.unwrap();
            let decoded = decoder.decode(&packet).unwrap();

            assert!(!packet.data.is_empty());
            assert!(packet.is_independent());
            assert_eq!(decoded.sequence, packet.sequence);
            assert_eq!(decoded.pts_micros, packet.pts_micros);
            assert_eq!(decoded.frames_per_channel(), FRAME_SAMPLES);

            if let (Some(previous_pts), Some(previous_sequence)) = (last_pts, last_sequence) {
                assert!(packet.pts_micros > previous_pts);
                assert!(packet.sequence > previous_sequence);
            }
            last_pts = Some(packet.pts_micros);
            last_sequence = Some(packet.sequence);
        }

        assert_eq!(
            source.next_frame().await.unwrap_err(),
            AudioError::EndOfStream
        );
    }
}
