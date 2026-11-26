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
//! | [`AlsaUac2Sink`]/[`AlsaUac2Source`] | non-blocking ALSA adapters for the `ozma-usb` UAC2 bridge |
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
//! - create or reuse a named `module-remap-source` over an existing source or
//!   monitor, and remove it again,
//! - move a playback stream onto a sink or a record stream onto a source,
//! - apply sink/source volume and mute presets, and
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
//! - The router is not a room-correction engine: [`AudioProfile`] and
//!   [`AudioSourceProfile`] carry only graph selection, volume, and mute.
//!   Source capture is selection: a named source must already exist, and
//!   [`AudioRouter::ensure_source`] creates a remap of an existing master
//!   rather than a signal generator or DSP block.
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
pub mod eq;
pub mod error;
pub mod fake;
pub mod frame;
pub mod graph;
pub mod opus;
pub mod pipewire;
pub mod room;
pub mod router;
pub mod sweep;
pub mod target;
pub mod traits;
pub mod uac2;

#[cfg(target_os = "linux")]
pub use alsa::{
    AlsaCapture, AlsaCaptureConfig, DEFAULT_BUFFER_PERIODS, DEFAULT_PERIOD_FRAMES,
    DEFAULT_START_TIMEOUT, FRAME_CHANNEL_CAPACITY,
};
pub use eq::{
    eq_sink_name, eq_unsupported, next_eq_sink_name, validate_bands, RoomEqApplication, RoomEqSpec,
    TempEqConfig, DEFAULT_EQ_CHANNELS, DEFAULT_EQ_RATE, EQ_INSERTION, EQ_LOAD_TIMEOUT,
    EQ_MODULE_NAME, EQ_POLL_INTERVAL, EQ_SINK_PREFIX, MAX_ACTIVE_EQ_STATE_BYTES, MAX_EQ_CHANNELS,
    MAX_EQ_RATE, MIN_EQ_RATE, PIPEWIRE,
};
pub use error::{audio_codes, AudioError};
pub use fake::{FakeAudioSource, DEFAULT_FRAME_LIMIT, DEFAULT_TONE_HZ};
pub use frame::{
    format_label, opus_format, samples_per_frame, AudioFormat, AudioFrame, AudioReframer, CHANNELS,
    FRAME_MICROS, FRAME_SAMPLES, SAMPLE_RATE,
};
pub use graph::{
    read_graph, AudioGraphLink, AudioGraphNode, AudioGraphSnapshot, AudioGraphStream,
    GRAPH_TIMEOUT, MAX_GRAPH_NODES, MAX_GRAPH_STREAMS,
};
pub use opus::{default_config, OpusDecoder, OpusEncoder, MAX_OPUS_PACKET_BYTES, OPUS_ENCODER_ID};
pub use pipewire::PipeWireCapture;
pub use room::{
    validate_profile, RoomProfileStore, MAX_ROOM_PROFILE_BYTES, MAX_ROOM_PROFILE_TEXT,
    ROOM_EQ_STATE_FILE, ROOM_PROFILE_FILE,
};
pub use router::{
    AudioPlaybackSpec, AudioProfile, AudioRouter, AudioSinkSpec, AudioSinkState,
    AudioSourceProfile, AudioSourceRef, AudioSourceSpec, AudioSourceState, AudioStreamRef, ChildId,
    CommandOutput, CommandRunner, PipeWireAudioRouter, PulseAudioAudioRouter, RouterConfig,
    TokioCommandRunner, DEFAULT_SINK_CHANNELS, DEFAULT_SINK_RATE, DEFAULT_SOURCE_CHANNELS,
    DEFAULT_VOLUME_PERCENT, MAX_SINK_CHANNELS, MAX_SINK_RATE, MAX_SOURCE_CHANNELS, MIN_SINK_RATE,
    NULL_SINK_MODULE, PACTL, PAPLAY, PW_PLAY, REMAP_SOURCE_MODULE,
};
pub use sweep::{
    run_sweep, BandAnalysis, CaptureSession, PlaybackSession, RoomBand, SweepBand, SweepDescriptor,
    SweepIo, SweepMeasurement, SweepOutcome, SweepProfile, SweepRequest, SystemSweepIo,
    ANALYSIS_BAND_CENTRES_HZ, MAX_SUGGESTED_GAIN_DB, MAX_SWEEP_BANDS, MAX_SWEEP_CHANNELS,
    MAX_SWEEP_DURATION_SECS, MAX_SWEEP_HZ, MAX_SWEEP_SAMPLE_RATE, MAX_SWEEP_TAIL_SECS,
    MIN_SUGGESTED_CONFIDENCE, MIN_SWEEP_DURATION_SECS, MIN_SWEEP_HZ, MIN_SWEEP_SAMPLE_RATE,
    MIN_SWEEP_TAIL_SECS,
};
pub use target::{AudioTarget, MONITOR_PREFIX, NODE_PREFIX, OBJECT_PREFIX};
pub use traits::{AudioEncoder, AudioSource, EncodedAudio, EncoderConfig};
pub use uac2::{Uac2SinkHandle, Uac2SinkStats, Uac2SourceHandle, Uac2SourceStats};
#[cfg(target_os = "linux")]
pub use uac2::{
    AlsaPcmFactory, AlsaUac2Config, AlsaUac2Sink, AlsaUac2Source, Uac2PcmReader,
    Uac2PcmReaderFactory, Uac2PcmWriter, Uac2PcmWriterFactory, UAC2_DEFAULT_BUFFER_PERIODS,
    UAC2_DEFAULT_PERIOD_FRAMES,
};
#[cfg(target_os = "linux")]
pub use uac2::{PipeWireUac2Sink, PipeWireUac2Source};

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
