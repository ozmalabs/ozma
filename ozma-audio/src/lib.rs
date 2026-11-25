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
//! | [`AudioTarget`] | named default/object/node/sink-monitor selection |
//! | [`AlsaCapture`] | real ALSA capture, blocking worker + bounded channel |
//! | [`PipeWireCapture`] | native PipeWire capture by target (Linux) |
//! | [`FakeAudioSource`] | deterministic 440 Hz/silence source for CI |
//! | [`AudioEncoder`] | async encode boundary |
//! | [`OpusEncoder`] | libopus, 20 ms 48 kHz stereo, one packet per frame |
//! | [`OpusDecoder`] | small libopus decoder for tests and local checks |
//! | [`AudioRouter`] | per-VM/seat sink, monitor, stream, profile, and playback control |
//!
//! # Routing scope
//!
//! [`AudioRouter`] controls an **existing** PipeWire/PulseAudio graph. It does
//! not start or replace the audio server, and it does not implement room
//! correction, equalisation, convolution, or any other DSP. Concretely, the
//! router can:
//!
//! - create or reuse a named `module-null-sink` and report its
//!   `<sink>.monitor` capture target,
//! - remove a null sink it owns (and refuse to unload a non-Ozma sink),
//! - move a playback stream onto a sink,
//! - apply sink volume/mute presets, and
//! - play and stop a reference track.
//!
//! Graph commands go through an injectable [`CommandRunner`] that takes an
//! argv vector and optional stdin, never a shell. Default tests use a fake
//! runner, so they need no live PipeWire or PulseAudio session. A missing
//! helper binary or a non-zero exit is a typed [`AudioError`], never a silent
//! success.
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
//! - [`AudioTarget::SinkMonitor`] names `<sink>.monitor` but does not create
//!   it. Use [`AudioRouter::ensure_sink`] to create the null sink and monitor,
//!   then hand [`AudioSinkState::monitor_target`] back to
//!   [`PipeWireCapture::open_sink_monitor`].
//! - The router is not a room-correction engine: [`AudioProfile`] carries
//!   only sink, volume, and mute. Source routing is rejected with a typed
//!   error instead of being ignored.
//! - `audiopus` and `alsa` are native dependencies (libopus, ALSA). No OpenSSL
//!   or other TLS stack is involved. On non-Linux platforms the
//!   [`PipeWireCapture`] constructors fail with `AudioError::Unavailable`
//!   instead of pretending to open a stream.
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

#[cfg(target_os = "linux")]
pub mod alsa;
pub mod error;
pub mod fake;
pub mod frame;
pub mod opus;
pub mod pipewire;
pub mod router;
pub mod target;
pub mod traits;

#[cfg(target_os = "linux")]
pub use alsa::{
    AlsaCapture, AlsaCaptureConfig, DEFAULT_BUFFER_PERIODS, DEFAULT_PERIOD_FRAMES,
    DEFAULT_START_TIMEOUT, FRAME_CHANNEL_CAPACITY,
};
pub use error::{audio_codes, AudioError};
pub use fake::{FakeAudioSource, DEFAULT_FRAME_LIMIT, DEFAULT_TONE_HZ};
pub use frame::{
    format_label, opus_format, samples_per_frame, AudioFormat, AudioFrame, AudioReframer, CHANNELS,
    FRAME_MICROS, FRAME_SAMPLES, SAMPLE_RATE,
};
pub use opus::{default_config, OpusDecoder, OpusEncoder, MAX_OPUS_PACKET_BYTES, OPUS_ENCODER_ID};
pub use pipewire::PipeWireCapture;
pub use router::{
    AudioPlaybackSpec, AudioProfile, AudioRouter, AudioSinkSpec, AudioSinkState, AudioStreamRef,
    ChildId, CommandOutput, CommandRunner, PipeWireAudioRouter, PulseAudioAudioRouter,
    RouterConfig, TokioCommandRunner, DEFAULT_SINK_CHANNELS, DEFAULT_SINK_RATE,
    DEFAULT_VOLUME_PERCENT, MAX_SINK_CHANNELS, MAX_SINK_RATE, MIN_SINK_RATE, NULL_SINK_MODULE,
    PACTL, PAPLAY, PW_PLAY,
};
pub use target::{AudioTarget, MONITOR_PREFIX, NODE_PREFIX, OBJECT_PREFIX};
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
