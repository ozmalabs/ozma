// SPDX-License-Identifier: AGPL-3.0-only
//! Per-seat audio sink provisioning.
//!
//! The multi-seat supervisor is synchronous, while [`ozma_audio::AudioRouter`]
//! is async. [`SeatAudioRouter`] is the small synchronous seam the supervisor
//! calls before a seat starts and after it stops. [`OzmaAudioRouter`] adapts the
//! real router without changing `ozma-audio`: each call runs its future on a
//! dedicated current-thread runtime in a short-lived worker thread, so it is
//! safe to call from the CLI's Tokio runtime without nesting runtimes.
//!
//! This module never invents a sink. [`OzmaAudioRouter::ensure_sink`] delegates
//! to `pactl`/`pipewire-pulse` through `ozma-audio` and returns its typed error
//! when the audio server or helper is missing. [`crate::fake::FakeSeatAudioRouter`]
//! records calls for tests and can inject a typed failure.

use std::future::Future;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::error::MultiseatError;

/// Description prefix used for a seat null sink. The value is a single token so
/// it is valid both as a PulseAudio module argument and as an `ozma-audio`
/// `AudioSinkSpec` description.
pub const SEAT_AUDIO_DESCRIPTION_PREFIX: &str = "Ozma";

/// Default null-sink sample rate, matching `ozma-audio`.
pub const SEAT_AUDIO_RATE: u32 = 48_000;

/// Default stereo null-sink channel count.
pub const SEAT_AUDIO_CHANNELS: u16 = 2;

/// The sink a seat needs before its node starts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatAudioSpec {
    /// Sink name, normally `ozma-seat-N`.
    pub name: String,
    /// Single-token `device.description`.
    pub description: String,
    /// Channel count requested at creation.
    pub channels: u16,
    /// Sample rate requested at creation.
    pub rate: u32,
    /// Whether the seat intends to capture `<name>.monitor`.
    pub monitor: bool,
}

impl SeatAudioSpec {
    /// Build the conventional spec for a planned seat.
    pub fn for_seat(seat_id: &str, sink: impl Into<String>, channels: u16) -> Self {
        let channels = if channels == 0 {
            SEAT_AUDIO_CHANNELS
        } else {
            channels
        };
        Self {
            name: sink.into(),
            description: format!("{SEAT_AUDIO_DESCRIPTION_PREFIX}-{seat_id}"),
            channels,
            rate: SEAT_AUDIO_RATE,
            monitor: true,
        }
    }

    /// Conventional monitor object name for this sink.
    pub fn monitor_target(&self) -> String {
        ozma_audio::AudioTarget::monitor_name(&self.name)
    }

    /// Validate and convert to the `ozma-audio` sink request.
    pub fn to_audio_spec(&self) -> Result<ozma_audio::AudioSinkSpec, MultiseatError> {
        ozma_audio::AudioSinkSpec::new(&self.name, &self.description)
            .and_then(|spec| spec.with_channels(self.channels))
            .and_then(|spec| spec.with_rate(self.rate))
            .map(|spec| spec.with_monitor(self.monitor))
            .map_err(|error| MultiseatError::Audio(error.to_string()))
    }
}

/// Result of ensuring a seat sink.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatAudioState {
    /// Sink name reported by the backend.
    pub name: String,
    /// Conventional `<sink>.monitor` capture target.
    pub monitor_target: String,
    /// Whether this call actually created a new null sink.
    pub created: bool,
}

/// Synchronous per-seat audio seam used by the supervisor.
pub trait SeatAudioRouter: Send {
    /// Create or reuse the seat's null sink.
    fn ensure_sink(&self, spec: &SeatAudioSpec) -> Result<SeatAudioState, MultiseatError>;

    /// Remove the seat's null sink. An unknown sink is a no-op in `ozma-audio`.
    fn remove_sink(&self, name: &str) -> Result<(), MultiseatError>;
}

/// Adapter from the async [`ozma_audio::AudioRouter`] to [`SeatAudioRouter`].
pub struct OzmaAudioRouter {
    router: Arc<dyn ozma_audio::AudioRouter>,
}

impl OzmaAudioRouter {
    /// Wrap an injected audio router.
    pub fn new(router: Arc<dyn ozma_audio::AudioRouter>) -> Self {
        Self { router }
    }

    /// Production PipeWire host: `pactl` against `pipewire-pulse`.
    pub fn pipewire() -> Self {
        Self::new(Arc::new(ozma_audio::PipeWireAudioRouter::production()))
    }

    /// Production PulseAudio host.
    pub fn pulseaudio() -> Self {
        Self::new(Arc::new(ozma_audio::PulseAudioAudioRouter::production()))
    }
}

impl SeatAudioRouter for OzmaAudioRouter {
    fn ensure_sink(&self, spec: &SeatAudioSpec) -> Result<SeatAudioState, MultiseatError> {
        let audio_spec = spec.to_audio_spec()?;
        let router = Arc::clone(&self.router);
        let state = run_audio_future(async move { router.ensure_sink(audio_spec).await })?;
        Ok(SeatAudioState {
            name: state.name,
            monitor_target: state.monitor_target,
            created: state.created,
        })
    }

    fn remove_sink(&self, name: &str) -> Result<(), MultiseatError> {
        let name = name.to_owned();
        let router = Arc::clone(&self.router);
        run_audio_future(async move { router.remove_sink(&name).await })
    }
}

/// Run one audio-router future without nesting it on the caller's runtime.
fn run_audio_future<F, T>(future: F) -> Result<T, MultiseatError>
where
    F: Future<Output = Result<T, ozma_audio::AudioError>> + Send + 'static,
    T: Send + 'static,
{
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let worker = std::thread::Builder::new()
        .name("ozma-multiseat-audio".to_owned())
        .spawn(move || {
            let result = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime.block_on(future).map_err(|error| error.to_string()),
                Err(error) => Err(format!("could not build audio runtime: {error}")),
            };
            let _ = sender.send(result);
        })
        .map_err(|error| {
            MultiseatError::Audio(format!("could not start audio worker thread: {error}"))
        })?;

    let result = receiver
        .recv()
        .map_err(|_| MultiseatError::Audio("audio worker exited without a result".to_owned()))?;
    let _ = worker.join();
    result.map_err(MultiseatError::Audio)
}
