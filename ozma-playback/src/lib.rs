// SPDX-License-Identifier: AGPL-3.0-only
//! Playback sinks for Ozma clients.
//!
//! This crate is the presenting half of the M1 media path in
//! `docs/node-hermes-parity.md` §8: a client receives decoded video and audio
//! from `ozma-decode` and needs somewhere to show and play it without pulling a
//! GUI toolkit into the library.
//!
//! | Type | Role |
//! |---|---|
//! | [`AudioSink`] | object-safe audio boundary: `open`, `play`, `flush`, `stop` |
//! | [`AlsaAudioSink`] | 48 kHz stereo S16_LE ALSA output on a blocking worker thread |
//! | [`NullAudioSink`] | headless sink that records frames and bytes |
//! | [`VideoSink`] | object-safe video boundary: `present`, `flush`, `stop` |
//! | [`NullVideoSink`] | headless sink that records dimensions and frame metadata |
//! | [`PpmVideoSink`] | writes the first frame or numbered PPM files |
//! | `WindowVideoSink` | `minifb` window, only with `--features window` |
//! | [`PlaybackEngine`] | bounded queues and worker threads feeding the sinks |
//!
//! # Example
//!
//! ```no_run
//! use ozma_playback::{
//!     AlsaAudioSink, AlsaAudioSinkConfig, AudioSink, NullVideoSink, PlaybackEngine,
//! };
//!
//! # fn run(audio_block: ozma_decode::DecodedAudio) -> Result<(), Box<dyn std::error::Error>> {
//! let audio = AlsaAudioSink::connect(AlsaAudioSinkConfig::for_device("default"))?;
//! let mut engine = PlaybackEngine::new(Box::new(NullVideoSink::new()), Some(Box::new(audio)))?;
//! engine.play_audio(audio_block)?;
//! engine.stop()?;
//! # Ok(())
//! # }
//! ```
//!
//! # Deliberate limits
//!
//! * **No A/V synchronisation.** The engine presents frames in arrival order;
//!   lip sync, jitter buffering, and clock recovery belong to the session layer.
//! * **One video sink, one audio sink.** Multi-track presentation is a client
//!   concern: create one engine per track or implement a fan-out sink.
//! * **ALSA only** for device output on Linux. The safe `alsa` crate is used;
//!   there is no PulseAudio, PipeWire, CoreAudio, or WASAPI backend yet.
//! * **Audio output is fixed format**: 48 kHz stereo S16_LE. Other formats are
//!   rejected rather than resampled.
//! * **The window sink is optional and basic.** It is for native bring-up, not
//!   colour management, scaling policy, or HDR.
//!
//! The crate is `#![forbid(unsafe_code)]`; device and window work stays inside
//! the safe `alsa` and `minifb` wrappers.

#![forbid(unsafe_code)]

pub mod audio;
#[cfg(target_os = "linux")]
pub mod pipewire;
pub mod engine;
pub mod error;
pub mod video;

#[cfg(feature = "window")]
pub mod window;

pub use audio::{
    validate_audio_block, validate_audio_format, AlsaAudioSink, AlsaAudioSinkConfig, AudioRecord,
    AudioSink, NullAudioSink, DEFAULT_BUFFER_PERIODS as DEFAULT_AUDIO_BUFFER_PERIODS,
    DEFAULT_PERIOD_FRAMES as DEFAULT_AUDIO_PERIOD_FRAMES,
    DEFAULT_QUEUE_CAPACITY as DEFAULT_AUDIO_QUEUE_CAPACITY, PLAYBACK_CHANNELS,
    PLAYBACK_SAMPLE_RATE,
};
#[cfg(target_os = "linux")]
pub use pipewire::PipeWireAudioSink;
pub use engine::{EngineConfig, PlaybackEngine, PlaybackStats};
pub use error::{playback_codes, PlaybackError};
pub use video::{NullVideoSink, PpmMode, PpmVideoSink, VideoRecord, VideoSink};

#[cfg(feature = "window")]
pub use window::WindowVideoSink;
