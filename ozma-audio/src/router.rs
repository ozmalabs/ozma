// SPDX-License-Identifier: AGPL-3.0-only
//! Per-VM and per-seat audio routing on an existing PipeWire/PulseAudio graph.
//!
//! This module is the routing half of the Phase 1/2 `ozma-audio` work: it
//! creates a named null sink (and therefore a `<sink>.monitor` capture
//! target), unloads it again, moves playback streams onto it, applies a
//! volume/mute profile, and plays a reference track through it.
//!
//! # Scope, honestly
//!
//! - It **controls an existing audio server**. It does not start, configure,
//!   or replace PipeWire/PulseAudio, and it does not create a new audio
//!   server for the node.
//! - The primary implementation is `pactl`, including on PipeWire hosts:
//!   `pipewire-pulse` provides the PulseAudio compatibility API that `pactl`
//!   drives. `pw-cli create-node adapter ...` is not used because it needs a
//!   graph adapter description that varies with PipeWire version and does not
//!   expose a module id for removal; `pactl load-module`/`unload-module` is
//!   stable and symmetric, which is what idempotent per-VM provisioning
//!   needs.
//! - This is **not room correction**. [`AudioProfile`] is a routing preset:
//!   sink/source selection plus optional volume and mute. Equalisation,
//!   convolution, filter chains, and other DSP are deliberately out of scope.
//! - Microphone/source support is selection and graph control, not capture:
//!   [`AudioSourceSpec`] creates or reuses a named `module-remap-source` from
//!   an existing source or `<sink>.monitor`, [`AudioSourceRef`] moves an
//!   existing source-output onto a named source, and [`AudioSourceProfile`]
//!   applies source volume/mute. The source is then captured by
//!   [`crate::PipeWireCapture::open_source`] or `open_named_source`.
//! - Every command is an argv vector. There is no shell, no string
//!   interpolation, and no quoting: a sink name such as `ozma-vm1` is exactly
//!   one argument.
//! - Default tests use a fake [`CommandRunner`], so no live PipeWire or
//!   PulseAudio session is required. A missing `pactl`/`pw-play`/`paplay`
//!   binary and a non-zero exit are typed errors, never a false `Ok`.
//!
//! # Commands
//!
//! | Operation | argv |
//! |---|---|
//! | create sink | `pactl load-module module-null-sink sink_name=<name> sink_properties=device.description=<description> rate=<rate> channels=<channels>` |
//! | find sink | `pactl list short sinks` |
//! | find module | `pactl list short modules` |
//! | remove sink | `pactl unload-module <module-id>` |
//! | move stream | `pactl move-sink-input <sink-input-index> <sink>` |
//! | create source | `pactl load-module module-remap-source master=<master> source_name=<name> source_properties=device.description=<description> channels=<channels>` |
//! | find source | `pactl list short sources` |
//! | remove source | `pactl unload-module <module-id>` (the `module-remap-source`) |
//! | move source-output | `pactl move-source-output <source-output-index> <source>` |
//! | volume | `pactl set-sink-volume <sink> <percent>%` |
//! | mute | `pactl set-sink-mute <sink> yes\|no` |
//! | source volume | `pactl set-source-volume <source> <percent>%` |
//! | source mute | `pactl set-source-mute <source> yes\|no` |
//! | play (PipeWire) | `pw-play --target <sink> <track-path>` |
//! | play (PulseAudio) | `paplay --device=<sink> <track-path>` |
//!
//! # Semantics
//!
//! - [`AudioRouter::ensure_sink`] asks `pactl list short sinks` on every call
//!   and reuses an existing sink with the same name, reporting
//!   `created: false`. Its description, rate, and channel count are **not**
//!   changed; those parameters only apply at creation. No module id is cached
//!   across calls, so a restarted audio server cannot produce a false
//!   "already present" or make removal unload an unrelated module.
//! - [`AudioRouter::remove_sink`] unloads the `module-null-sink` that owns the
//!   sink. An unknown sink is a successful no-op. A sink that exists but is
//!   not a `module-null-sink` is refused, so a hardware sink cannot be
//!   accidentally unloaded.
//! - [`AudioRouter::apply_profile`] applies volume and mute to the profile's
//!   sink and records which fields were applied.
//!   [`AudioRouter::remove_profile`] restores only those fields: an applied
//!   volume goes back to [`DEFAULT_VOLUME_PERCENT`], an applied mute becomes
//!   unmuted, and a profile that only recorded a sink runs no command. An
//!   unknown profile id is a successful no-op.
//! - [`AudioRouter::play_reference`] stops any previous playback, checks that
//!   the resolved track file exists and that the named sink exists, then spawns
//!   the player. The sink check is required because `pw-play --target` and
//!   `paplay --device` fall back to the default sink when the name is unknown.
//!   It returns once the child is running. [`AudioRouter::stop_playback`] kills
//!   and reaps the child and is a no-op when nothing is playing.

use std::{
    collections::HashMap,
    io,
    path::{Component, Path, PathBuf},
    process::Stdio,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, MutexGuard, PoisonError,
    },
};

use async_trait::async_trait;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::error::AudioError;
use crate::target::AudioTarget;

/// The PulseAudio control binary, also provided by `pipewire-pulse`.
pub const PACTL: &str = "pactl";
/// The PipeWire reference-track player.
pub const PW_PLAY: &str = "pw-play";
/// The PulseAudio reference-track player.
pub const PAPLAY: &str = "paplay";
/// PulseAudio module loaded for a named null sink.
pub const NULL_SINK_MODULE: &str = "module-null-sink";
/// PulseAudio module loaded for a named source remapped from a master.
pub const REMAP_SOURCE_MODULE: &str = "module-remap-source";
/// Lowest accepted null-sink sample rate.
pub const MIN_SINK_RATE: u32 = 8_000;
/// Highest accepted null-sink sample rate.
pub const MAX_SINK_RATE: u32 = 192_000;
/// Highest accepted null-sink channel count.
pub const MAX_SINK_CHANNELS: u16 = 32;
/// Highest accepted remap-source channel count.
pub const MAX_SOURCE_CHANNELS: u16 = 32;
/// Default remap-source channel count.
pub const DEFAULT_SOURCE_CHANNELS: u16 = 2;
/// Default null-sink sample rate, matching [`crate::frame::SAMPLE_RATE`].
pub const DEFAULT_SINK_RATE: u32 = 48_000;
/// Default null-sink channel count, matching [`crate::frame::CHANNELS`].
pub const DEFAULT_SINK_CHANNELS: u16 = 2;
/// Volume a profile removal restores.
pub const DEFAULT_VOLUME_PERCENT: u8 = 100;

const PACTL_REMEDIATION: &str = "Install pulseaudio-utils (or pipewire-pulse) so `pactl` is on \
                                 PATH, and ensure the PipeWire/PulseAudio server is running.";
const PW_PLAY_REMEDIATION: &str = "Install pipewire-bin so `pw-play` is on PATH, or use the \
                                   PulseAudio router with `paplay`.";
const PAPLAY_REMEDIATION: &str = "Install pulseaudio-utils so `paplay` is on PATH, or use the \
                                  PipeWire router with `pw-play`.";
const TRACK_REMEDIATION: &str = "Install the reference tracks under the configured track root, \
                                 or configure `RouterConfig::track_root` to the directory that \
                                 contains them.";

/// A request to create (or reuse) a named null sink.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioSinkSpec {
    /// PulseAudio/PipeWire sink name, for example `ozma-vm1`.
    pub name: String,
    /// `device.description` property, for example `Ozma-VM1`.
    ///
    /// Must be a single token: `pactl` joins module arguments with spaces, so
    /// a description containing whitespace cannot be represented unambiguously
    /// without a shell.
    pub description: String,
    /// Channel count used at creation.
    pub channels: u16,
    /// Sample rate used at creation.
    pub rate: u32,
    /// Whether the caller intends to capture `<name>.monitor`.
    ///
    /// A null sink always exposes a monitor, so this is recorded intent and
    /// does not change the command. [`AudioSinkState::monitor_target`] is
    /// always present.
    pub monitor: bool,
}

impl AudioSinkSpec {
    /// Build a validated spec with 48 kHz stereo defaults and monitor intent.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
    ) -> Result<Self, AudioError> {
        let spec = Self {
            name: name.into(),
            description: description.into(),
            channels: DEFAULT_SINK_CHANNELS,
            rate: DEFAULT_SINK_RATE,
            monitor: true,
        };
        spec.validate()?;
        Ok(spec)
    }

    /// Set the channel count, validating the result.
    pub fn with_channels(mut self, channels: u16) -> Result<Self, AudioError> {
        self.channels = channels;
        self.validate()?;
        Ok(self)
    }

    /// Set the sample rate, validating the result.
    pub fn with_rate(mut self, rate: u32) -> Result<Self, AudioError> {
        self.rate = rate;
        self.validate()?;
        Ok(self)
    }

    /// Set the monitor intent.
    pub fn with_monitor(mut self, monitor: bool) -> Self {
        self.monitor = monitor;
        self
    }

    /// The conventional capture target for this sink.
    pub fn monitor_target(&self) -> String {
        AudioTarget::monitor_name(&self.name)
    }

    /// Validate every field before any command runs.
    pub fn validate(&self) -> Result<(), AudioError> {
        validate_token("sink name", &self.name)?;
        validate_token("sink description", &self.description)?;
        if self.channels == 0 || self.channels > MAX_SINK_CHANNELS {
            return Err(invalid_spec(
                "sink channels",
                format!(
                    "must be between 1 and {MAX_SINK_CHANNELS}, got {}",
                    self.channels
                ),
            ));
        }
        if !(MIN_SINK_RATE..=MAX_SINK_RATE).contains(&self.rate) {
            return Err(invalid_spec(
                "sink rate",
                format!(
                    "must be between {MIN_SINK_RATE} and {MAX_SINK_RATE} Hz, got {}",
                    self.rate
                ),
            ));
        }
        Ok(())
    }
}

/// The result of ensuring a sink exists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioSinkState {
    /// Sink name, exactly as requested.
    pub name: String,
    /// Conventional monitor object, always `<name>.monitor`.
    pub monitor_target: String,
    /// `true` when this call loaded a new `module-null-sink`.
    pub created: bool,
}

/// A playback stream to move between sinks.
///
/// This names a PulseAudio *sink input* index, the numeric id printed by
/// `pactl list short sink-inputs`. Record streams (source outputs) are not
/// routed here.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AudioStreamRef {
    /// Sink-input index.
    pub index: u32,
}

impl AudioStreamRef {
    /// Build a reference for a sink-input index.
    pub fn new(index: u32) -> Self {
        Self { index }
    }
}

/// A request to create (or reuse) a named input source.
///
/// A PulseAudio source is created as a `module-remap-source` over an existing
/// source or a sink monitor. `master` is the existing object to remap:
/// `alsa_input.usb-mic` for a microphone, or `<sink>.monitor` for VM output.
/// When the source already exists, `master` is only reported and no module is
/// loaded; when it does not exist and `master` is `None`, creation is refused
/// instead of inventing a source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioSourceSpec {
    /// Source name, for example `ozma-vm1-mic`.
    pub name: String,
    /// `device.description` property for the remapped source.
    pub description: String,
    /// Existing source or `<sink>.monitor` to remap. Required for creation.
    pub master: Option<String>,
    /// Channel count requested from `module-remap-source`.
    pub channels: u16,
}

impl AudioSourceSpec {
    /// A validated request that only selects an existing source.
    ///
    /// [`AudioRouter::ensure_source`] returns the existing source, or a typed
    /// refusal when it is missing because there is no master to create from.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
    ) -> Result<Self, AudioError> {
        let spec = Self {
            name: name.into(),
            description: description.into(),
            master: None,
            channels: DEFAULT_SOURCE_CHANNELS,
        };
        spec.validate()?;
        Ok(spec)
    }

    /// A validated request that remaps `master` into a named source.
    pub fn from_master(
        name: impl Into<String>,
        description: impl Into<String>,
        master: impl Into<String>,
    ) -> Result<Self, AudioError> {
        let spec = Self {
            name: name.into(),
            description: description.into(),
            master: Some(master.into()),
            channels: DEFAULT_SOURCE_CHANNELS,
        };
        spec.validate()?;
        Ok(spec)
    }

    /// A validated request that remaps the monitor of `sink`.
    ///
    /// The resulting source is not the monitor itself; it is a named
    /// `module-remap-source` whose master is `<sink>.monitor`.
    pub fn monitor(
        name: impl Into<String>,
        description: impl Into<String>,
        sink: &str,
    ) -> Result<Self, AudioError> {
        validate_token("source monitor sink", sink)?;
        Self::from_master(name, description, AudioTarget::monitor_name(sink))
    }

    /// Set the master source or monitor, validating the result.
    pub fn with_master(mut self, master: impl Into<String>) -> Result<Self, AudioError> {
        self.master = Some(master.into());
        self.validate()?;
        Ok(self)
    }

    /// Set the channel count, validating the result.
    pub fn with_channels(mut self, channels: u16) -> Result<Self, AudioError> {
        self.channels = channels;
        self.validate()?;
        Ok(self)
    }

    /// Validate every field before any command runs.
    pub fn validate(&self) -> Result<(), AudioError> {
        validate_token("source name", &self.name)?;
        validate_token("source description", &self.description)?;
        if let Some(master) = &self.master {
            validate_token("source master", master)?;
        }
        if self.channels == 0 || self.channels > MAX_SOURCE_CHANNELS {
            return Err(invalid_spec(
                "source channels",
                format!(
                    "must be between 1 and {MAX_SOURCE_CHANNELS}, got {}",
                    self.channels
                ),
            ));
        }
        Ok(())
    }
}

/// The result of ensuring a source exists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioSourceState {
    /// Source name, exactly as requested.
    pub name: String,
    /// Master source or monitor recorded at creation/selection.
    pub master: Option<String>,
    /// `true` when this call loaded a new `module-remap-source`.
    pub created: bool,
}

/// A record stream to move between sources.
///
/// This names a PulseAudio *source-output* index, the numeric id printed by
/// `pactl list short source-outputs`. Playback (sink input) streams use
/// [`AudioStreamRef`] instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AudioSourceRef {
    /// Source-output index.
    pub index: u32,
}

impl AudioSourceRef {
    /// Build a reference for a source-output index.
    pub fn new(index: u32) -> Self {
        Self { index }
    }
}

/// A volume/mute preset for one input source.
///
/// This is not a capture or DSP profile. See the module documentation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioSourceProfile {
    /// Stable profile identifier.
    pub id: String,
    /// Source the profile applies to.
    pub source: String,
    /// Optional source volume in percent, `0..=100`.
    pub volume: Option<u8>,
    /// Optional mute state.
    pub muted: Option<bool>,
}

impl AudioSourceProfile {
    /// Build a validated source profile with no volume or mute.
    pub fn new(id: impl Into<String>, source: impl Into<String>) -> Result<Self, AudioError> {
        let profile = Self {
            id: id.into(),
            source: source.into(),
            volume: None,
            muted: None,
        };
        profile.validate()?;
        Ok(profile)
    }

    /// Set the source volume, validating the result.
    pub fn with_volume(mut self, volume: u8) -> Result<Self, AudioError> {
        self.volume = Some(volume);
        self.validate()?;
        Ok(self)
    }

    /// Set the mute state.
    pub fn with_muted(mut self, muted: bool) -> Self {
        self.muted = Some(muted);
        self
    }

    /// Validate every field before any command runs.
    pub fn validate(&self) -> Result<(), AudioError> {
        validate_token("source profile id", &self.id)?;
        validate_token("source profile source", &self.source)?;
        if let Some(volume) = self.volume {
            if volume > 100 {
                return Err(invalid_spec(
                    "source profile volume",
                    format!("must be 0..=100 percent, got {volume}"),
                ));
            }
        }
        Ok(())
    }
}

/// A routing/volume preset for one sink, optionally with a named source.
///
/// This is not an equalisation profile. See the module documentation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioProfile {
    /// Stable profile identifier.
    pub id: String,
    /// Sink the profile applies to. Empty only for a source-only profile.
    pub sink: String,
    /// Optional source name. When set, the same `volume`/`muted` values are
    /// applied to the named source as well. Moving a source-output needs
    /// [`AudioRouter::move_source_output`] with an explicit index.
    pub source: Option<String>,
    /// Optional sink/source volume in percent, `0..=100`.
    pub volume: Option<u8>,
    /// Optional sink/source mute state.
    pub muted: Option<bool>,
}

impl AudioProfile {
    /// Build a validated profile with no volume, mute, or source settings.
    pub fn new(id: impl Into<String>, sink: impl Into<String>) -> Result<Self, AudioError> {
        let profile = Self {
            id: id.into(),
            sink: sink.into(),
            source: None,
            volume: None,
            muted: None,
        };
        profile.validate()?;
        Ok(profile)
    }

    /// Build a validated source-only profile (no sink).
    pub fn for_source(
        id: impl Into<String>,
        source: impl Into<String>,
    ) -> Result<Self, AudioError> {
        let profile = Self {
            id: id.into(),
            sink: String::new(),
            source: Some(source.into()),
            volume: None,
            muted: None,
        };
        profile.validate()?;
        Ok(profile)
    }

    /// Attach a named source. The same `volume`/`muted` values apply to it.
    pub fn with_source(mut self, source: impl Into<String>) -> Result<Self, AudioError> {
        self.source = Some(source.into());
        self.validate()?;
        Ok(self)
    }

    /// Set the sink volume, validating the result.
    pub fn with_volume(mut self, volume: u8) -> Result<Self, AudioError> {
        self.volume = Some(volume);
        self.validate()?;
        Ok(self)
    }

    /// Set the mute state.
    pub fn with_muted(mut self, muted: bool) -> Self {
        self.muted = Some(muted);
        self
    }

    /// Whether this profile has no sink and only names a source.
    pub fn is_source_only(&self) -> bool {
        self.sink.is_empty() && self.source.is_some()
    }

    /// Validate every field before any command runs.
    pub fn validate(&self) -> Result<(), AudioError> {
        validate_token("profile id", &self.id)?;
        if self.sink.is_empty() {
            if self.source.is_none() {
                return Err(invalid_spec(
                    "profile sink",
                    "must not be empty unless a source is set",
                ));
            }
        } else {
            validate_token("profile sink", &self.sink)?;
        }
        if let Some(source) = &self.source {
            validate_token("profile source", source)?;
        }
        if let Some(volume) = self.volume {
            if volume > 100 {
                return Err(invalid_spec(
                    "profile volume",
                    format!("must be 0..=100 percent, got {volume}"),
                ));
            }
        }
        Ok(())
    }
}

/// A request to play a reference track through a sink.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioPlaybackSpec {
    /// A single file name, resolved under [`RouterConfig::track_root`].
    pub track: String,
    /// Sink to play through.
    pub sink: String,
}

impl AudioPlaybackSpec {
    /// Build a validated playback request.
    pub fn new(track: impl Into<String>, sink: impl Into<String>) -> Result<Self, AudioError> {
        let spec = Self {
            track: track.into(),
            sink: sink.into(),
        };
        spec.validate()?;
        Ok(spec)
    }

    /// Validate the track name and sink before touching the filesystem or
    /// spawning a player.
    pub fn validate(&self) -> Result<(), AudioError> {
        validate_token("track name", &self.track)?;
        let mut components = Path::new(&self.track).components();
        if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
            return Err(invalid_spec(
                "track name",
                "must be a single file name such as `sweep.flac`, not a path",
            ));
        }
        validate_token("playback sink", &self.sink)?;
        Ok(())
    }
}

/// Captured result of a command that ran to completion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandOutput {
    /// Exit status; `0` means success.
    pub status: i32,
    /// Captured standard output.
    pub stdout: String,
    /// Captured standard error, not trimmed.
    pub stderr: String,
}

impl CommandOutput {
    /// Build an output value, typically from a test fake.
    pub fn new(status: i32, stdout: impl Into<String>, stderr: impl Into<String>) -> Self {
        Self {
            status,
            stdout: stdout.into(),
            stderr: stderr.into(),
        }
    }

    /// Convenience constructor for a successful command.
    pub fn success(stdout: impl Into<String>) -> Self {
        Self::new(0, stdout, "")
    }

    /// Whether the process exited successfully.
    pub fn is_success(&self) -> bool {
        self.status == 0
    }
}

/// Opaque id for a child spawned by a [`CommandRunner`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ChildId(pub u64);

/// The injectable process boundary for audio routing.
///
/// Commands are passed as an argv vector whose first element is the program.
/// Implementations must never use a shell. A missing executable must become
/// [`AudioError::MissingCommand`]; a non-zero exit may be returned as a
/// [`CommandOutput`] and is checked by the router, and is also mapped to
/// [`AudioError::CommandFailed`] by [`TokioCommandRunner`].
#[async_trait]
pub trait CommandRunner: Send + Sync {
    /// Run `argv` to completion, optionally feeding `stdin`.
    async fn run(&self, argv: &[String], stdin: Option<&str>) -> Result<CommandOutput, AudioError>;

    /// Spawn a long-running child, such as `pw-play`.
    async fn spawn(&self, argv: &[String]) -> Result<ChildId, AudioError>;

    /// Kill and reap a child returned by [`CommandRunner::spawn`].
    ///
    /// Killing an already-exited child is a successful no-op.
    async fn kill(&self, child: ChildId) -> Result<(), AudioError>;
}

/// Production [`CommandRunner`] backed by [`tokio::process::Command`].
pub struct TokioCommandRunner {
    children: tokio::sync::Mutex<HashMap<u64, tokio::process::Child>>,
    next: AtomicU64,
}

impl std::fmt::Debug for TokioCommandRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokioCommandRunner").finish_non_exhaustive()
    }
}

impl Default for TokioCommandRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl TokioCommandRunner {
    /// A runner with no children.
    pub fn new() -> Self {
        Self {
            children: tokio::sync::Mutex::new(HashMap::new()),
            next: AtomicU64::new(0),
        }
    }
}

#[async_trait]
impl CommandRunner for TokioCommandRunner {
    async fn run(&self, argv: &[String], stdin: Option<&str>) -> Result<CommandOutput, AudioError> {
        let (program, arguments) = split_argv(argv)?;
        let mut command = Command::new(program);
        command
            .args(arguments)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if stdin.is_some() {
            command.stdin(Stdio::piped());
        } else {
            command.stdin(Stdio::null());
        }

        let mut child = command
            .spawn()
            .map_err(|error| map_spawn_error(program, error))?;
        if let Some(data) = stdin {
            if let Some(mut pipe) = child.stdin.take() {
                pipe.write_all(data.as_bytes())
                    .await
                    .map_err(|error| AudioError::Process {
                        command: program.to_owned(),
                        message: error.to_string(),
                    })?;
                pipe.shutdown().await.map_err(|error| AudioError::Process {
                    command: program.to_owned(),
                    message: error.to_string(),
                })?;
            }
        }

        let output = child
            .wait_with_output()
            .await
            .map_err(|error| AudioError::Process {
                command: render_command(argv),
                message: error.to_string(),
            })?;

        let status = output.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let captured = CommandOutput {
            status,
            stdout,
            stderr,
        };
        check_status(argv, captured)
    }

    async fn spawn(&self, argv: &[String]) -> Result<ChildId, AudioError> {
        let (program, arguments) = split_argv(argv)?;
        let child = Command::new(program)
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| map_spawn_error(program, error))?;
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        self.children.lock().await.insert(id, child);
        Ok(ChildId(id))
    }

    async fn kill(&self, child: ChildId) -> Result<(), AudioError> {
        let mut child = self.children.lock().await.remove(&child.0);
        match child.as_mut() {
            Some(child) => {
                // An already-exited child is not an error; wait reaps it.
                let _ = child.kill().await;
                child
                    .wait()
                    .await
                    .map(|_| ())
                    .map_err(|error| AudioError::Process {
                        command: "playback child".to_owned(),
                        message: error.to_string(),
                    })
            }
            None => Ok(()),
        }
    }
}

/// A routing backend that controls an existing PipeWire/PulseAudio graph.
#[async_trait]
pub trait AudioRouter: Send + Sync {
    /// Create or reuse a named null sink.
    async fn ensure_sink(&self, spec: AudioSinkSpec) -> Result<AudioSinkState, AudioError>;

    /// Unload the null-sink module that owns `name`.
    ///
    /// An unknown sink is a successful no-op; a non-null sink is refused.
    async fn remove_sink(&self, name: &str) -> Result<(), AudioError>;

    /// Move a playback stream onto `sink`.
    async fn move_stream(&self, stream: AudioStreamRef, sink: &str) -> Result<(), AudioError>;

    /// Create or reuse a named source (a `module-remap-source` of `master`).
    ///
    /// An existing source with the same name is reused. When it is missing and
    /// `spec.master` is `None`, creation is refused instead of inventing a
    /// source. Backends without source support return a typed error.
    async fn ensure_source(&self, spec: AudioSourceSpec) -> Result<AudioSourceState, AudioError> {
        spec.validate()?;
        Err(source_unsupported("ensure_source"))
    }

    /// Unload the `module-remap-source` that owns `name`.
    ///
    /// An unknown source is a successful no-op; a source that exists but is not
    /// an Ozma remap module is refused.
    async fn remove_source(&self, name: &str) -> Result<(), AudioError> {
        validate_token("source name", name)?;
        Err(source_unsupported("remove_source"))
    }

    /// Move a record stream onto `source`.
    async fn move_source_output(
        &self,
        stream: AudioSourceRef,
        source: &str,
    ) -> Result<(), AudioError> {
        validate_token("source name", source)?;
        let _ = stream;
        Err(source_unsupported("move_source_output"))
    }

    /// Apply source routing/volume/mute for a profile.
    async fn apply_source_profile(&self, profile: AudioSourceProfile) -> Result<(), AudioError> {
        profile.validate()?;
        Err(source_unsupported("apply_source_profile"))
    }

    /// Apply sink routing/volume/mute for a profile.
    async fn apply_profile(&self, profile: AudioProfile) -> Result<(), AudioError>;

    /// Restore the sink a profile was applied to and forget the profile.
    async fn remove_profile(&self, profile_id: &str) -> Result<(), AudioError>;

    /// Play a reference track through a sink.
    async fn play_reference(&self, spec: AudioPlaybackSpec) -> Result<(), AudioError>;

    /// Stop the current reference-track playback, if any.
    async fn stop_playback(&self) -> Result<(), AudioError>;
}

/// Configuration shared by both routers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RouterConfig {
    /// Directory containing reference tracks. The track name in
    /// [`AudioPlaybackSpec`] is a single file name joined onto this directory.
    /// When `None`, the current working directory is used.
    pub track_root: Option<PathBuf>,
}

impl RouterConfig {
    /// Set the directory that holds reference tracks.
    pub fn with_track_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.track_root = Some(root.into());
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PlaybackTool {
    PipeWire,
    PulseAudio,
}

impl PlaybackTool {
    fn argv(self, sink: &str, track: &Path) -> Vec<String> {
        let track = track.to_string_lossy().into_owned();
        match self {
            Self::PipeWire => vec![
                PW_PLAY.to_owned(),
                "--target".to_owned(),
                sink.to_owned(),
                track,
            ],
            Self::PulseAudio => vec![PAPLAY.to_owned(), format!("--device={sink}"), track],
        }
    }
}

struct AppliedProfile {
    sink: String,
    volume_applied: bool,
    mute_applied: bool,
}

struct AppliedSourceProfile {
    source: String,
    volume_applied: bool,
    mute_applied: bool,
}

struct RouterCore {
    runner: Arc<dyn CommandRunner>,
    config: RouterConfig,
    tool: PlaybackTool,
    profiles: Mutex<HashMap<String, AppliedProfile>>,
    source_profiles: Mutex<HashMap<String, AppliedSourceProfile>>,
    playback: tokio::sync::Mutex<Option<ChildId>>,
}

impl RouterCore {
    fn new(runner: Arc<dyn CommandRunner>, config: RouterConfig, tool: PlaybackTool) -> Self {
        Self {
            runner,
            config,
            tool,
            profiles: Mutex::new(HashMap::new()),
            source_profiles: Mutex::new(HashMap::new()),
            playback: tokio::sync::Mutex::new(None),
        }
    }

    async fn run(&self, argv: Vec<String>) -> Result<CommandOutput, AudioError> {
        let output = self.runner.run(&argv, None).await?;
        check_status(&argv, output)
    }

    async fn sink_exists(&self, name: &str) -> Result<bool, AudioError> {
        let argv = short_list_argv("sinks");
        let output = self.run(argv).await?;
        let exists = parse_sink_names(&output.stdout).any(|candidate| candidate == name);
        Ok(exists)
    }

    async fn find_null_sink_module(&self, name: &str) -> Result<Option<u32>, AudioError> {
        let argv = short_list_argv("modules");
        let output = self.run(argv).await?;
        parse_null_sink_module(&output.stdout, name)
    }

    async fn source_exists(&self, name: &str) -> Result<bool, AudioError> {
        let argv = short_list_argv("sources");
        let output = self.run(argv).await?;
        let exists = parse_source_names(&output.stdout).any(|candidate| candidate == name);
        Ok(exists)
    }

    async fn find_remap_source_module(&self, name: &str) -> Result<Option<u32>, AudioError> {
        let argv = short_list_argv("modules");
        let output = self.run(argv).await?;
        parse_remap_source_module(&output.stdout, name)
    }

    async fn ensure_sink(&self, spec: AudioSinkSpec) -> Result<AudioSinkState, AudioError> {
        spec.validate()?;

        // Always ask the server: the audio server may have restarted and
        // dropped a sink this process created earlier.
        if self.sink_exists(&spec.name).await? {
            return Ok(sink_state(&spec.name, false));
        }

        let argv = vec![
            PACTL.to_owned(),
            "load-module".to_owned(),
            NULL_SINK_MODULE.to_owned(),
            format!("sink_name={}", spec.name),
            format!("sink_properties=device.description={}", spec.description),
            format!("rate={}", spec.rate),
            format!("channels={}", spec.channels),
        ];
        let output = self.run(argv.clone()).await?;
        // Validate the returned id now; `remove_sink` re-discovers it from the
        // server so a stale in-process id can never unload an unrelated module.
        parse_module_id(&output.stdout, &argv)?;
        Ok(sink_state(&spec.name, true))
    }

    async fn remove_sink(&self, name: &str) -> Result<(), AudioError> {
        validate_token("sink name", name)?;

        let module_id = self.find_null_sink_module(name).await?;

        let Some(module_id) = module_id else {
            if self.sink_exists(name).await? {
                return Err(AudioError::Refused {
                    operation: "remove_sink".to_owned(),
                    reason: format!(
                        "sink `{name}` exists but is not a `{NULL_SINK_MODULE}` owned by Ozma"
                    ),
                    remediation: "Only Ozma-created null sinks can be removed. Disconnect or \
                                  reconfigure a hardware sink through the audio server instead."
                        .to_owned(),
                });
            }
            return Ok(());
        };

        let argv = vec![
            PACTL.to_owned(),
            "unload-module".to_owned(),
            module_id.to_string(),
        ];
        match self.run(argv).await {
            Ok(_) => Ok(()),
            Err(error) => {
                // A concurrent removal can make `unload-module` fail after the
                // sink is already gone. That is the documented unknown-sink
                // success case, not a failure.
                if !self.sink_exists(name).await.unwrap_or(true) {
                    Ok(())
                } else {
                    Err(error)
                }
            }
        }
    }

    async fn ensure_source(&self, spec: AudioSourceSpec) -> Result<AudioSourceState, AudioError> {
        spec.validate()?;

        if self.source_exists(&spec.name).await? {
            return Ok(source_state(&spec.name, spec.master, false));
        }

        let Some(master) = spec.master.as_deref() else {
            return Err(AudioError::Refused {
                operation: "ensure_source".to_owned(),
                reason: format!(
                    "source `{}` does not exist and no master source or monitor was supplied",
                    spec.name
                ),
                remediation: "Pass an existing master source such as `alsa_input.usb-mic` or a \
                              sink monitor `<sink>.monitor` so a `module-remap-source` can be \
                              created, or select an existing source name."
                    .to_owned(),
            });
        };

        let argv = vec![
            PACTL.to_owned(),
            "load-module".to_owned(),
            REMAP_SOURCE_MODULE.to_owned(),
            format!("master={master}"),
            format!("source_name={}", spec.name),
            format!("source_properties=device.description={}", spec.description),
            format!("channels={}", spec.channels),
        ];
        let output = self.run(argv.clone()).await?;
        parse_module_id(&output.stdout, &argv)?;
        Ok(source_state(&spec.name, spec.master, true))
    }

    async fn remove_source(&self, name: &str) -> Result<(), AudioError> {
        validate_token("source name", name)?;

        let module_id = self.find_remap_source_module(name).await?;

        let Some(module_id) = module_id else {
            if self.source_exists(name).await? {
                return Err(AudioError::Refused {
                    operation: "remove_source".to_owned(),
                    reason: format!(
                        "source `{name}` exists but is not a `{REMAP_SOURCE_MODULE}` owned by Ozma"
                    ),
                    remediation: "Only Ozma-created remap sources can be removed. Reconfigure a \
                                  hardware source through the audio server instead."
                        .to_owned(),
                });
            }
            return Ok(());
        };

        let argv = vec![
            PACTL.to_owned(),
            "unload-module".to_owned(),
            module_id.to_string(),
        ];
        match self.run(argv).await {
            Ok(_) => Ok(()),
            Err(error) => {
                // A concurrent removal can make `unload-module` fail after the
                // source is already gone. That is the documented unknown-source
                // success case, not a failure.
                if !self.source_exists(name).await.unwrap_or(true) {
                    Ok(())
                } else {
                    Err(error)
                }
            }
        }
    }

    async fn move_source_output(
        &self,
        stream: AudioSourceRef,
        source: &str,
    ) -> Result<(), AudioError> {
        validate_token("source name", source)?;
        let argv = vec![
            PACTL.to_owned(),
            "move-source-output".to_owned(),
            stream.index.to_string(),
            source.to_owned(),
        ];
        self.run(argv).await.map(|_| ())
    }

    async fn move_stream(&self, stream: AudioStreamRef, sink: &str) -> Result<(), AudioError> {
        validate_token("sink name", sink)?;
        let argv = vec![
            PACTL.to_owned(),
            "move-sink-input".to_owned(),
            stream.index.to_string(),
            sink.to_owned(),
        ];
        self.run(argv).await.map(|_| ())
    }

    async fn apply_profile(&self, profile: AudioProfile) -> Result<(), AudioError> {
        profile.validate()?;

        if !profile.sink.is_empty() {
            if let Some(volume) = profile.volume {
                let argv = vec![
                    PACTL.to_owned(),
                    "set-sink-volume".to_owned(),
                    profile.sink.clone(),
                    format!("{volume}%"),
                ];
                self.run(argv).await?;
            }
            if let Some(muted) = profile.muted {
                let argv = vec![
                    PACTL.to_owned(),
                    "set-sink-mute".to_owned(),
                    profile.sink.clone(),
                    if muted { "yes" } else { "no" }.to_owned(),
                ];
                self.run(argv).await?;
            }

            lock(&self.profiles).insert(
                profile.id.clone(),
                AppliedProfile {
                    sink: profile.sink.clone(),
                    volume_applied: profile.volume.is_some(),
                    mute_applied: profile.muted.is_some(),
                },
            );
        }

        if let Some(source) = profile.source {
            self.apply_source_profile(AudioSourceProfile {
                id: profile.id,
                source,
                volume: profile.volume,
                muted: profile.muted,
            })
            .await?;
        }
        Ok(())
    }

    async fn apply_source_profile(&self, profile: AudioSourceProfile) -> Result<(), AudioError> {
        profile.validate()?;

        if let Some(volume) = profile.volume {
            let argv = vec![
                PACTL.to_owned(),
                "set-source-volume".to_owned(),
                profile.source.clone(),
                format!("{volume}%"),
            ];
            self.run(argv).await?;
        }
        if let Some(muted) = profile.muted {
            let argv = vec![
                PACTL.to_owned(),
                "set-source-mute".to_owned(),
                profile.source.clone(),
                if muted { "yes" } else { "no" }.to_owned(),
            ];
            self.run(argv).await?;
        }

        lock(&self.source_profiles).insert(
            profile.id.clone(),
            AppliedSourceProfile {
                source: profile.source,
                volume_applied: profile.volume.is_some(),
                mute_applied: profile.muted.is_some(),
            },
        );
        Ok(())
    }

    async fn remove_profile(&self, profile_id: &str) -> Result<(), AudioError> {
        validate_token("profile id", profile_id)?;
        let applied = lock(&self.profiles).remove(profile_id);
        let applied_source = lock(&self.source_profiles).remove(profile_id);

        if let Some(applied) = applied {
            if applied.mute_applied {
                let unmute = vec![
                    PACTL.to_owned(),
                    "set-sink-mute".to_owned(),
                    applied.sink.clone(),
                    "no".to_owned(),
                ];
                self.run(unmute).await?;
            }
            if applied.volume_applied {
                let volume = vec![
                    PACTL.to_owned(),
                    "set-sink-volume".to_owned(),
                    applied.sink,
                    format!("{DEFAULT_VOLUME_PERCENT}%"),
                ];
                self.run(volume).await?;
            }
        }

        if let Some(applied) = applied_source {
            if applied.mute_applied {
                let unmute = vec![
                    PACTL.to_owned(),
                    "set-source-mute".to_owned(),
                    applied.source.clone(),
                    "no".to_owned(),
                ];
                self.run(unmute).await?;
            }
            if applied.volume_applied {
                let volume = vec![
                    PACTL.to_owned(),
                    "set-source-volume".to_owned(),
                    applied.source,
                    format!("{DEFAULT_VOLUME_PERCENT}%"),
                ];
                self.run(volume).await?;
            }
        }
        Ok(())
    }

    fn resolve_track(&self, track: &str) -> Result<PathBuf, AudioError> {
        let root = self
            .config
            .track_root
            .clone()
            .unwrap_or_else(|| PathBuf::from("."));
        Ok(root.join(track))
    }

    async fn play_reference(&self, spec: AudioPlaybackSpec) -> Result<(), AudioError> {
        spec.validate()?;
        let path = self.resolve_track(&spec.track)?;
        if !path.is_file() {
            return Err(AudioError::TrackNotFound {
                track: spec.track,
                path: path.display().to_string(),
                remediation: TRACK_REMEDIATION.to_owned(),
            });
        }

        // `pw-play --target` and `paplay --device` silently fall back to the
        // default sink when the named sink is missing, so spawning without this
        // check reported `ok` and played on the user's speakers instead.
        if !self.sink_exists(&spec.sink).await? {
            return Err(AudioError::Refused {
                operation: "play_reference".to_owned(),
                reason: format!("playback sink `{}` does not exist", spec.sink),
                remediation: "Create the sink first (`AudioRouter::ensure_sink` or \
                              `ozma-node --audio-sink <name>`), or pass an existing sink name. \
                              No player was started; pw-play/paplay would otherwise fall back \
                              to the default sink."
                    .to_owned(),
            });
        }

        self.stop_playback().await?;
        let argv = self.tool.argv(&spec.sink, &path);
        let child = self.runner.spawn(&argv).await?;
        *self.playback.lock().await = Some(child);
        Ok(())
    }

    async fn stop_playback(&self) -> Result<(), AudioError> {
        let child = self.playback.lock().await.take();
        match child {
            Some(child) => self.runner.kill(child).await,
            None => Ok(()),
        }
    }
}

/// PipeWire-side router: `pactl` for graph management, `pw-play` for tracks.
pub struct PipeWireAudioRouter {
    core: RouterCore,
}

impl PipeWireAudioRouter {
    /// Build a router with an injected runner and default configuration.
    pub fn new(runner: Arc<dyn CommandRunner>) -> Self {
        Self::with_config(runner, RouterConfig::default())
    }

    /// Build a router with an injected runner and explicit configuration.
    pub fn with_config(runner: Arc<dyn CommandRunner>, config: RouterConfig) -> Self {
        Self {
            core: RouterCore::new(runner, config, PlaybackTool::PipeWire),
        }
    }

    /// Build a router using the real process runner.
    pub fn production() -> Self {
        Self::new(Arc::new(TokioCommandRunner::new()))
    }

    /// The configuration this router was built with.
    pub fn config(&self) -> &RouterConfig {
        &self.core.config
    }
}

/// PulseAudio-side router: `pactl` for graph management, `paplay` for tracks.
pub struct PulseAudioAudioRouter {
    core: RouterCore,
}

impl PulseAudioAudioRouter {
    /// Build a router with an injected runner and default configuration.
    pub fn new(runner: Arc<dyn CommandRunner>) -> Self {
        Self::with_config(runner, RouterConfig::default())
    }

    /// Build a router with an injected runner and explicit configuration.
    pub fn with_config(runner: Arc<dyn CommandRunner>, config: RouterConfig) -> Self {
        Self {
            core: RouterCore::new(runner, config, PlaybackTool::PulseAudio),
        }
    }

    /// Build a router using the real process runner.
    pub fn production() -> Self {
        Self::new(Arc::new(TokioCommandRunner::new()))
    }

    /// The configuration this router was built with.
    pub fn config(&self) -> &RouterConfig {
        &self.core.config
    }
}

macro_rules! impl_audio_router {
    ($router:ty) => {
        #[async_trait]
        impl AudioRouter for $router {
            async fn ensure_sink(&self, spec: AudioSinkSpec) -> Result<AudioSinkState, AudioError> {
                self.core.ensure_sink(spec).await
            }

            async fn remove_sink(&self, name: &str) -> Result<(), AudioError> {
                self.core.remove_sink(name).await
            }

            async fn move_stream(
                &self,
                stream: AudioStreamRef,
                sink: &str,
            ) -> Result<(), AudioError> {
                self.core.move_stream(stream, sink).await
            }

            async fn ensure_source(
                &self,
                spec: AudioSourceSpec,
            ) -> Result<AudioSourceState, AudioError> {
                self.core.ensure_source(spec).await
            }

            async fn remove_source(&self, name: &str) -> Result<(), AudioError> {
                self.core.remove_source(name).await
            }

            async fn move_source_output(
                &self,
                stream: AudioSourceRef,
                source: &str,
            ) -> Result<(), AudioError> {
                self.core.move_source_output(stream, source).await
            }

            async fn apply_source_profile(
                &self,
                profile: AudioSourceProfile,
            ) -> Result<(), AudioError> {
                self.core.apply_source_profile(profile).await
            }

            async fn apply_profile(&self, profile: AudioProfile) -> Result<(), AudioError> {
                self.core.apply_profile(profile).await
            }

            async fn remove_profile(&self, profile_id: &str) -> Result<(), AudioError> {
                self.core.remove_profile(profile_id).await
            }

            async fn play_reference(&self, spec: AudioPlaybackSpec) -> Result<(), AudioError> {
                self.core.play_reference(spec).await
            }

            async fn stop_playback(&self) -> Result<(), AudioError> {
                self.core.stop_playback().await
            }
        }
    };
}

impl_audio_router!(PipeWireAudioRouter);
impl_audio_router!(PulseAudioAudioRouter);

fn sink_state(name: &str, created: bool) -> AudioSinkState {
    AudioSinkState {
        name: name.to_owned(),
        monitor_target: AudioTarget::monitor_name(name),
        created,
    }
}

fn source_state(name: &str, master: Option<String>, created: bool) -> AudioSourceState {
    AudioSourceState {
        name: name.to_owned(),
        master,
        created,
    }
}

fn source_unsupported(operation: &str) -> AudioError {
    AudioError::Unsupported {
        feature: format!("audio source routing (`{operation}`)"),
        remediation: "Use a router backed by a PulseAudio-compatible server (`pactl`), or \
                      select an existing source with `default`/`object:<name>`/`monitor:<sink>` \
                      for capture. No source graph change was attempted."
            .to_owned(),
    }
}

fn short_list_argv(kind: &str) -> Vec<String> {
    vec![
        PACTL.to_owned(),
        "list".to_owned(),
        "short".to_owned(),
        kind.to_owned(),
    ]
}

fn parse_short_names(stdout: &str) -> impl Iterator<Item = &str> {
    stdout.lines().filter_map(|line| {
        // `pactl list short sinks|sources` is tab-separated on current
        // releases; fall back to whitespace so older/mocked output still
        // parses.
        let mut fields = line.split('\t');
        match (fields.next(), fields.next()) {
            (Some(_), Some(name)) if !name.trim().is_empty() => Some(name.trim()),
            _ => line.split_whitespace().nth(1),
        }
    })
}

fn parse_sink_names(stdout: &str) -> impl Iterator<Item = &str> {
    parse_short_names(stdout)
}

fn parse_source_names(stdout: &str) -> impl Iterator<Item = &str> {
    parse_short_names(stdout)
}

fn parse_module_id(stdout: &str, argv: &[String]) -> Result<u32, AudioError> {
    let trimmed = stdout.trim();
    trimmed.parse::<u32>().map_err(|_| AudioError::OutputParse {
        command: render_command(argv),
        output: stdout.to_owned(),
        reason: "expected `pactl load-module` to print a numeric module id".to_owned(),
    })
}

fn parse_named_module(
    stdout: &str,
    module: &str,
    key: &str,
    value: &str,
) -> Result<Option<u32>, AudioError> {
    let command = render_command(&short_list_argv("modules"));
    let wanted = format!("{key}={value}");
    for line in stdout.lines() {
        let mut words = line.split_whitespace();
        let Some(id_raw) = words.next() else {
            continue;
        };
        let Some(found_module) = words.next() else {
            continue;
        };
        if found_module != module {
            continue;
        }
        if !words.any(|word| word == wanted) {
            continue;
        }
        let id = id_raw.parse::<u32>().map_err(|_| AudioError::OutputParse {
            command: command.clone(),
            output: line.to_owned(),
            reason: "module id is not a u32".to_owned(),
        })?;
        return Ok(Some(id));
    }
    Ok(None)
}

fn parse_null_sink_module(stdout: &str, sink: &str) -> Result<Option<u32>, AudioError> {
    parse_named_module(stdout, NULL_SINK_MODULE, "sink_name", sink)
}

fn parse_remap_source_module(stdout: &str, source: &str) -> Result<Option<u32>, AudioError> {
    parse_named_module(stdout, REMAP_SOURCE_MODULE, "source_name", source)
}

fn validate_token(field: &str, value: &str) -> Result<(), AudioError> {
    if value.is_empty() {
        return Err(invalid_spec(field, "must not be empty"));
    }
    if value.trim().is_empty() {
        return Err(invalid_spec(field, "must not be whitespace-only"));
    }
    if let Some(character) = value.chars().find(|character| character.is_control()) {
        return Err(invalid_spec(
            field,
            format!("must not contain control character {character:?}"),
        ));
    }
    if let Some(character) = value
        .chars()
        .find(|character| !is_token_character(*character))
    {
        return Err(invalid_spec(
            field,
            format!(
                "must be a single token made of letters, digits, `.`, `_`, or `-`; \
                 found {character:?}"
            ),
        ));
    }
    if value.starts_with('-') {
        return Err(invalid_spec(field, "must not start with `-`"));
    }
    Ok(())
}

fn is_token_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-')
}

fn invalid_spec(field: &str, reason: impl Into<String>) -> AudioError {
    AudioError::InvalidSpec {
        field: field.to_owned(),
        reason: reason.into(),
    }
}

fn check_status(argv: &[String], output: CommandOutput) -> Result<CommandOutput, AudioError> {
    if output.is_success() {
        Ok(output)
    } else {
        Err(AudioError::CommandFailed {
            command: render_command(argv),
            status: output.status,
            stderr: output.stderr.trim().to_owned(),
        })
    }
}

fn split_argv(argv: &[String]) -> Result<(&str, &[String]), AudioError> {
    let Some(program) = argv.first() else {
        return Err(invalid_spec("argv", "command argv must not be empty"));
    };
    if program.is_empty() {
        return Err(invalid_spec("argv", "program name must not be empty"));
    }
    Ok((program, &argv[1..]))
}

fn map_spawn_error(program: &str, error: io::Error) -> AudioError {
    if error.kind() == io::ErrorKind::NotFound {
        AudioError::MissingCommand {
            command: program.to_owned(),
            remediation: remediation_for(program).to_owned(),
        }
    } else {
        AudioError::Process {
            command: program.to_owned(),
            message: error.to_string(),
        }
    }
}

fn remediation_for(program: &str) -> &'static str {
    match program {
        PACTL => PACTL_REMEDIATION,
        PW_PLAY => PW_PLAY_REMEDIATION,
        PAPLAY => PAPLAY_REMEDIATION,
        _ => "Install the command and ensure it is on PATH.",
    }
}

/// Render an argv for diagnostics only. Never passed to a shell.
fn render_command(argv: &[String]) -> String {
    argv.iter()
        .map(|argument| {
            if argument.is_empty() || argument.chars().any(char::is_whitespace) {
                format!("{argument:?}")
            } else {
                argument.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicU64;

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct RecordedCall {
        argv: Vec<String>,
        stdin: Option<String>,
    }

    #[derive(Default)]
    struct FakeRunner {
        calls: Mutex<Vec<RecordedCall>>,
        responses: Mutex<VecDeque<Result<CommandOutput, AudioError>>>,
        spawns: Mutex<Vec<Vec<String>>>,
        killed: Mutex<Vec<ChildId>>,
        next_child: AtomicU64,
    }

    impl FakeRunner {
        fn with_responses(responses: Vec<Result<CommandOutput, AudioError>>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
                ..Self::default()
            }
        }

        fn calls(&self) -> Vec<RecordedCall> {
            lock(&self.calls).clone()
        }

        fn spawns(&self) -> Vec<Vec<String>> {
            lock(&self.spawns).clone()
        }

        fn killed(&self) -> Vec<ChildId> {
            lock(&self.killed).clone()
        }
    }

    #[async_trait]
    impl CommandRunner for FakeRunner {
        async fn run(
            &self,
            argv: &[String],
            stdin: Option<&str>,
        ) -> Result<CommandOutput, AudioError> {
            lock(&self.calls).push(RecordedCall {
                argv: argv.to_vec(),
                stdin: stdin.map(str::to_owned),
            });
            match lock(&self.responses).pop_front() {
                Some(response) => response,
                None => Ok(CommandOutput::success("")),
            }
        }

        async fn spawn(&self, argv: &[String]) -> Result<ChildId, AudioError> {
            lock(&self.spawns).push(argv.to_vec());
            let id = self.next_child.fetch_add(1, Ordering::Relaxed) + 1;
            Ok(ChildId(id))
        }

        async fn kill(&self, child: ChildId) -> Result<(), AudioError> {
            lock(&self.killed).push(child);
            Ok(())
        }
    }

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| (*part).to_owned()).collect()
    }

    fn pipewire(runner: Arc<FakeRunner>) -> PipeWireAudioRouter {
        PipeWireAudioRouter::new(runner)
    }

    fn sink_list(name: &str) -> CommandOutput {
        CommandOutput::success(format!("3\t{name}\tPipeWire\ts16le 2ch 48000Hz\tRUNNING\n"))
    }

    fn source_list(name: &str) -> CommandOutput {
        CommandOutput::success(format!("5\t{name}\tPipeWire\ts16le 2ch 48000Hz\tRUNNING\n"))
    }

    fn module_list(sink: &str, id: u32) -> CommandOutput {
        CommandOutput::success(format!(
            "{id}\t{NULL_SINK_MODULE}\tsink_name={sink} \
             sink_properties=device.description=Ozma-VM1 rate=48000 channels=2\n"
        ))
    }

    fn source_module_list(source: &str, id: u32) -> CommandOutput {
        CommandOutput::success(format!(
            "{id}\t{REMAP_SOURCE_MODULE}\tmaster=ozma-vm1.monitor \
             source_name={source} source_properties=device.description=Ozma-VM1-Mic channels=2\n"
        ))
    }

    #[tokio::test]
    async fn ensure_sink_creates_a_null_sink_and_parses_the_module_id() {
        let fake = Arc::new(FakeRunner::with_responses(vec![
            Ok(CommandOutput::success("")),
            Ok(CommandOutput::success("42\n")),
        ]));
        let router = pipewire(fake.clone());

        let state = router
            .ensure_sink(AudioSinkSpec::new("ozma-vm1", "Ozma-VM1").unwrap())
            .await
            .unwrap();

        assert_eq!(
            state,
            AudioSinkState {
                name: "ozma-vm1".to_owned(),
                monitor_target: "ozma-vm1.monitor".to_owned(),
                created: true,
            }
        );
        let calls = fake.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].argv, argv(&["pactl", "list", "short", "sinks"]));
        assert_eq!(
            calls[1].argv,
            argv(&[
                "pactl",
                "load-module",
                "module-null-sink",
                "sink_name=ozma-vm1",
                "sink_properties=device.description=Ozma-VM1",
                "rate=48000",
                "channels=2",
            ])
        );
        assert_eq!(calls[0].stdin, None);
        assert_eq!(calls[1].stdin, None);
    }

    #[tokio::test]
    async fn ensure_sink_reuses_an_existing_sink_by_name() {
        let fake = Arc::new(FakeRunner::with_responses(vec![
            Ok(sink_list("ozma-vm1")),
            Ok(sink_list("ozma-vm1")),
        ]));
        let router = pipewire(fake.clone());

        let first = router
            .ensure_sink(AudioSinkSpec::new("ozma-vm1", "Ozma-VM1").unwrap())
            .await
            .unwrap();
        assert!(!first.created);
        assert_eq!(first.monitor_target, "ozma-vm1.monitor");
        assert_eq!(fake.calls().len(), 1);
        assert!(
            fake.calls()
                .iter()
                .all(|call| call.argv[1] != "load-module"),
            "an existing sink must not be loaded twice"
        );

        // A second ensure verifies the server again and still does not reload.
        let second = router
            .ensure_sink(AudioSinkSpec::new("ozma-vm1", "Ozma-VM1").unwrap())
            .await
            .unwrap();
        assert!(!second.created);
        let calls = fake.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].argv, argv(&["pactl", "list", "short", "sinks"]));
        assert!(calls.iter().all(|call| call.argv[1] != "load-module"));
    }

    #[tokio::test]
    async fn ensure_sink_recreates_after_the_server_drops_it() {
        let fake = Arc::new(FakeRunner::with_responses(vec![
            Ok(CommandOutput::success("")),
            Ok(CommandOutput::success("1")),
            Ok(CommandOutput::success("")),
            Ok(CommandOutput::success("2")),
        ]));
        let router = pipewire(fake.clone());

        let first = router
            .ensure_sink(AudioSinkSpec::new("ozma-vm1", "Ozma-VM1").unwrap())
            .await
            .unwrap();
        assert!(first.created);

        // The server no longer lists the sink (restart), so the next ensure
        // must load it again rather than trusting process-local state.
        let second = router
            .ensure_sink(AudioSinkSpec::new("ozma-vm1", "Ozma-VM1").unwrap())
            .await
            .unwrap();
        assert!(second.created);

        let load_calls = fake
            .calls()
            .into_iter()
            .filter(|call| call.argv[1] == "load-module")
            .count();
        assert_eq!(load_calls, 2);
    }

    #[tokio::test]
    async fn remove_sink_finds_the_module_then_noops_when_already_gone() {
        let fake = Arc::new(FakeRunner::with_responses(vec![
            Ok(CommandOutput::success("")),
            Ok(CommandOutput::success("9")),
            Ok(module_list("ozma-vm1", 9)),
        ]));
        let router = pipewire(fake.clone());
        router
            .ensure_sink(AudioSinkSpec::new("ozma-vm1", "Ozma-VM1").unwrap())
            .await
            .unwrap();

        router.remove_sink("ozma-vm1").await.unwrap();
        let calls = fake.calls();
        assert_eq!(
            calls.last().unwrap().argv,
            argv(&["pactl", "unload-module", "9"])
        );

        // Unknown sink: query modules and sinks, then succeed without an
        // unload command.
        let before = fake.calls().len();
        router.remove_sink("ozma-vm1").await.unwrap();
        let calls = fake.calls();
        assert_eq!(
            calls[before].argv,
            argv(&["pactl", "list", "short", "modules"])
        );
        assert_eq!(
            calls[before + 1].argv,
            argv(&["pactl", "list", "short", "sinks"])
        );
        assert!(
            calls[before..]
                .iter()
                .all(|call| call.argv[1] != "unload-module"),
            "an unknown sink must be a typed no-op"
        );
    }

    #[tokio::test]
    async fn remove_sink_finds_the_module_of_a_preexisting_sink() {
        let fake = Arc::new(FakeRunner::with_responses(vec![
            Ok(module_list("ozma-vm1", 5)),
            Ok(CommandOutput::success("")),
        ]));
        let router = pipewire(fake.clone());

        router.remove_sink("ozma-vm1").await.unwrap();

        let calls = fake.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].argv, argv(&["pactl", "list", "short", "modules"]));
        assert_eq!(calls[1].argv, argv(&["pactl", "unload-module", "5"]));
    }

    #[tokio::test]
    async fn remove_sink_refuses_to_unload_a_non_ozma_sink() {
        let fake = Arc::new(FakeRunner::with_responses(vec![
            Ok(CommandOutput::success("")),
            Ok(sink_list("alsa_output.pci-0000_00_1f.3.analog-stereo")),
        ]));
        let router = pipewire(fake.clone());

        let error = router
            .remove_sink("alsa_output.pci-0000_00_1f.3.analog-stereo")
            .await
            .unwrap_err();

        assert!(matches!(error, AudioError::Refused { .. }));
        assert!(fake
            .calls()
            .iter()
            .all(|call| call.argv[1] != "unload-module"));
    }

    #[tokio::test]
    async fn move_stream_uses_the_sink_input_index() {
        let fake = Arc::new(FakeRunner::default());
        let router = pipewire(fake.clone());

        router
            .move_stream(AudioStreamRef::new(17), "ozma-vm1")
            .await
            .unwrap();

        assert_eq!(
            fake.calls()[0].argv,
            argv(&["pactl", "move-sink-input", "17", "ozma-vm1"])
        );
    }

    #[tokio::test]
    async fn move_source_output_uses_the_source_output_index() {
        let fake = Arc::new(FakeRunner::default());
        let router = pipewire(fake.clone());

        router
            .move_source_output(AudioSourceRef::new(23), "ozma-vm1-mic")
            .await
            .unwrap();

        assert_eq!(
            fake.calls()[0].argv,
            argv(&["pactl", "move-source-output", "23", "ozma-vm1-mic"])
        );
    }

    #[tokio::test]
    async fn ensure_source_creates_a_remap_source_from_a_monitor() {
        let fake = Arc::new(FakeRunner::with_responses(vec![
            Ok(CommandOutput::success("")),
            Ok(CommandOutput::success("44\n")),
        ]));
        let router = pipewire(fake.clone());

        let state = router
            .ensure_source(
                AudioSourceSpec::monitor("ozma-vm1-mic", "Ozma-VM1-Mic", "ozma-vm1").unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(
            state,
            AudioSourceState {
                name: "ozma-vm1-mic".to_owned(),
                master: Some("ozma-vm1.monitor".to_owned()),
                created: true,
            }
        );
        let calls = fake.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].argv, argv(&["pactl", "list", "short", "sources"]));
        assert_eq!(
            calls[1].argv,
            argv(&[
                "pactl",
                "load-module",
                "module-remap-source",
                "master=ozma-vm1.monitor",
                "source_name=ozma-vm1-mic",
                "source_properties=device.description=Ozma-VM1-Mic",
                "channels=2",
            ])
        );
    }

    #[tokio::test]
    async fn ensure_source_reuses_an_existing_source_by_name() {
        let fake = Arc::new(FakeRunner::with_responses(vec![Ok(source_list(
            "alsa_input.usb-mic",
        ))]));
        let router = pipewire(fake.clone());

        let state = router
            .ensure_source(AudioSourceSpec::new("alsa_input.usb-mic", "USB-Mic").unwrap())
            .await
            .unwrap();

        assert!(!state.created);
        assert_eq!(state.master, None);
        assert_eq!(fake.calls().len(), 1);
        assert_eq!(
            fake.calls()[0].argv,
            argv(&["pactl", "list", "short", "sources"])
        );
    }

    #[tokio::test]
    async fn ensure_source_without_a_master_refuses_to_invent_a_source() {
        let fake = Arc::new(FakeRunner::with_responses(vec![Ok(
            CommandOutput::success(""),
        )]));
        let router = pipewire(fake.clone());

        let error = router
            .ensure_source(AudioSourceSpec::new("ozma-vm9-mic", "Ozma-VM9-Mic").unwrap())
            .await
            .unwrap_err();

        assert!(matches!(error, AudioError::Refused { .. }));
        assert!(error.to_string().contains("master"));
        assert!(
            fake.calls()
                .iter()
                .all(|call| call.argv[1] != "load-module"),
            "no source may be loaded without a master"
        );
    }

    #[tokio::test]
    async fn remove_source_finds_the_remap_module_then_noops_when_already_gone() {
        let fake = Arc::new(FakeRunner::with_responses(vec![
            Ok(source_module_list("ozma-vm1-mic", 12)),
            Ok(CommandOutput::success("")),
            Ok(CommandOutput::success("")),
            Ok(CommandOutput::success("")),
        ]));
        let router = pipewire(fake.clone());

        router.remove_source("ozma-vm1-mic").await.unwrap();
        let calls = fake.calls();
        assert_eq!(calls[0].argv, argv(&["pactl", "list", "short", "modules"]));
        assert_eq!(calls[1].argv, argv(&["pactl", "unload-module", "12"]));

        // Unknown source: query modules and sources, then succeed without an
        // unload command.
        let before = fake.calls().len();
        router.remove_source("ozma-vm1-mic").await.unwrap();
        let calls = fake.calls();
        assert_eq!(
            calls[before].argv,
            argv(&["pactl", "list", "short", "modules"])
        );
        assert_eq!(
            calls[before + 1].argv,
            argv(&["pactl", "list", "short", "sources"])
        );
        assert!(
            calls[before..]
                .iter()
                .all(|call| call.argv[1] != "unload-module"),
            "an unknown source must be a typed no-op"
        );
    }

    #[tokio::test]
    async fn remove_source_refuses_to_unload_a_non_ozma_source() {
        let fake = Arc::new(FakeRunner::with_responses(vec![
            Ok(CommandOutput::success("")),
            Ok(source_list("alsa_input.usb-mic")),
        ]));
        let router = pipewire(fake.clone());

        let error = router
            .remove_source("alsa_input.usb-mic")
            .await
            .unwrap_err();

        assert!(matches!(error, AudioError::Refused { .. }));
        assert!(fake
            .calls()
            .iter()
            .all(|call| call.argv[1] != "unload-module"));
    }

    #[tokio::test]
    async fn apply_source_profile_sets_volume_and_mute_and_remove_restores_defaults() {
        let fake = Arc::new(FakeRunner::default());
        let router = pipewire(fake.clone());
        let profile = AudioSourceProfile::new("mic", "ozma-vm1-mic")
            .unwrap()
            .with_volume(35)
            .unwrap()
            .with_muted(true);

        router.apply_source_profile(profile).await.unwrap();
        let calls = fake.calls();
        assert_eq!(
            calls[0].argv,
            argv(&["pactl", "set-source-volume", "ozma-vm1-mic", "35%"])
        );
        assert_eq!(
            calls[1].argv,
            argv(&["pactl", "set-source-mute", "ozma-vm1-mic", "yes"])
        );

        router.remove_profile("mic").await.unwrap();
        let calls = fake.calls();
        assert_eq!(
            calls[2].argv,
            argv(&["pactl", "set-source-mute", "ozma-vm1-mic", "no"])
        );
        assert_eq!(
            calls[3].argv,
            argv(&["pactl", "set-source-volume", "ozma-vm1-mic", "100%"])
        );
    }

    #[tokio::test]
    async fn apply_profile_with_a_source_applies_both_sides() {
        let fake = Arc::new(FakeRunner::default());
        let router = pipewire(fake.clone());
        let profile = AudioProfile::new("duplex", "ozma-vm1")
            .unwrap()
            .with_source("ozma-vm1-mic")
            .unwrap()
            .with_volume(50)
            .unwrap();

        router.apply_profile(profile).await.unwrap();

        let calls: Vec<Vec<String>> = fake.calls().into_iter().map(|call| call.argv).collect();
        assert_eq!(
            calls,
            vec![
                argv(&["pactl", "set-sink-volume", "ozma-vm1", "50%"]),
                argv(&["pactl", "set-source-volume", "ozma-vm1-mic", "50%"]),
            ]
        );

        router.remove_profile("duplex").await.unwrap();
        let calls: Vec<Vec<String>> = fake.calls().into_iter().map(|call| call.argv).collect();
        assert_eq!(
            calls[2..],
            [
                argv(&["pactl", "set-sink-volume", "ozma-vm1", "100%"]),
                argv(&["pactl", "set-source-volume", "ozma-vm1-mic", "100%"]),
            ]
        );
    }

    #[tokio::test]
    async fn invalid_names_are_rejected_before_any_command_runs() {
        let fake = Arc::new(FakeRunner::default());
        let router = pipewire(fake.clone());

        let error = router
            .move_stream(AudioStreamRef::new(1), "bad sink")
            .await
            .unwrap_err();
        assert!(matches!(error, AudioError::InvalidSpec { .. }));
        assert!(fake.calls().is_empty());
    }

    #[tokio::test]
    async fn apply_profile_sets_volume_and_mute_and_remove_restores_defaults() {
        let fake = Arc::new(FakeRunner::default());
        let router = pipewire(fake.clone());
        let profile = AudioProfile::new("harman", "ozma-vm1")
            .unwrap()
            .with_volume(55)
            .unwrap()
            .with_muted(true);

        router.apply_profile(profile).await.unwrap();
        let calls = fake.calls();
        assert_eq!(
            calls[0].argv,
            argv(&["pactl", "set-sink-volume", "ozma-vm1", "55%"])
        );
        assert_eq!(
            calls[1].argv,
            argv(&["pactl", "set-sink-mute", "ozma-vm1", "yes"])
        );

        router.remove_profile("harman").await.unwrap();
        let calls = fake.calls();
        assert_eq!(
            calls[2].argv,
            argv(&["pactl", "set-sink-mute", "ozma-vm1", "no"])
        );
        assert_eq!(
            calls[3].argv,
            argv(&["pactl", "set-sink-volume", "ozma-vm1", "100%"])
        );

        // Unknown profile removal is a no-op.
        router.remove_profile("missing").await.unwrap();
        assert_eq!(fake.calls().len(), 4);
    }

    #[tokio::test]
    async fn removing_a_profile_restores_only_what_it_changed() {
        let fake = Arc::new(FakeRunner::default());
        let router = pipewire(fake.clone());

        router
            .apply_profile(
                AudioProfile::new("mute-only", "ozma-vm1")
                    .unwrap()
                    .with_muted(true),
            )
            .await
            .unwrap();
        router.remove_profile("mute-only").await.unwrap();

        let calls = fake.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(
            calls[0].argv,
            argv(&["pactl", "set-sink-mute", "ozma-vm1", "yes"])
        );
        assert_eq!(
            calls[1].argv,
            argv(&["pactl", "set-sink-mute", "ozma-vm1", "no"])
        );

        router
            .apply_profile(AudioProfile::new("sink-only", "ozma-vm1").unwrap())
            .await
            .unwrap();
        let before = fake.calls().len();
        router.remove_profile("sink-only").await.unwrap();
        assert_eq!(
            fake.calls().len(),
            before,
            "a sink-only profile must not reset unrelated volume or mute"
        );
    }

    #[tokio::test]
    async fn production_runner_maps_a_missing_binary_to_a_typed_error() {
        let runner = TokioCommandRunner::new();
        let error = runner
            .run(&argv(&["ozma-audio-command-that-does-not-exist"]), None)
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            AudioError::MissingCommand { ref command, .. }
                if command == "ozma-audio-command-that-does-not-exist"
        ));
        assert!(error.to_string().contains("was not found"));
    }

    #[tokio::test]
    async fn invalid_source_names_are_rejected_before_any_command_runs() {
        let fake = Arc::new(FakeRunner::default());
        let router = pipewire(fake.clone());
        let profile = AudioSourceProfile {
            id: "mic".to_owned(),
            source: "bad source".to_owned(),
            volume: Some(40),
            muted: None,
        };

        let error = router.apply_source_profile(profile).await.unwrap_err();

        assert!(matches!(error, AudioError::InvalidSpec { .. }));
        assert!(fake.calls().is_empty(), "no command may run");
    }

    #[tokio::test]
    async fn missing_command_is_a_typed_error_not_success() {
        let fake = Arc::new(FakeRunner::with_responses(vec![Err(
            AudioError::MissingCommand {
                command: "pactl".to_owned(),
                remediation: PACTL_REMEDIATION.to_owned(),
            },
        )]));
        let router = pipewire(fake);

        let error = router
            .ensure_sink(AudioSinkSpec::new("ozma-vm1", "Ozma-VM1").unwrap())
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            AudioError::MissingCommand { ref command, .. } if command == "pactl"
        ));
        assert_eq!(
            error.to_api_error().code,
            crate::error::audio_codes::AUDIO_MISSING_COMMAND
        );
    }

    #[tokio::test]
    async fn non_zero_exit_is_a_typed_error_not_success() {
        let fake = Arc::new(FakeRunner::with_responses(vec![
            Ok(CommandOutput::success("")),
            Ok(CommandOutput::new(1, "", "Failed to load module\n")),
        ]));
        let router = pipewire(fake);

        let error = router
            .ensure_sink(AudioSinkSpec::new("ozma-vm1", "Ozma-VM1").unwrap())
            .await
            .unwrap_err();

        match error {
            AudioError::CommandFailed {
                command,
                status,
                stderr,
            } => {
                assert!(command.contains("pactl load-module"));
                assert_eq!(status, 1);
                assert_eq!(stderr, "Failed to load module");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn unparseable_module_id_is_a_typed_error() {
        let fake = Arc::new(FakeRunner::with_responses(vec![
            Ok(CommandOutput::success("")),
            Ok(CommandOutput::success("not-a-number\n")),
        ]));
        let router = pipewire(fake);

        let error = router
            .ensure_sink(AudioSinkSpec::new("ozma-vm1", "Ozma-VM1").unwrap())
            .await
            .unwrap_err();

        assert!(matches!(error, AudioError::OutputParse { .. }));
    }

    #[test]
    fn module_parser_accepts_tab_and_space_separated_output() {
        let tab = "7\tmodule-null-sink\tsink_name=ozma-vm1 rate=48000\n";
        let space = "9 module-null-sink sink_name=ozma-vm2 channels=2\n";
        let other = "4\tmodule-null-sink\tsink_name=other\n";

        assert_eq!(parse_null_sink_module(tab, "ozma-vm1").unwrap(), Some(7));
        assert_eq!(parse_null_sink_module(space, "ozma-vm2").unwrap(), Some(9));
        assert_eq!(parse_null_sink_module(other, "ozma-vm1").unwrap(), None);
        assert_eq!(parse_null_sink_module("", "ozma-vm1").unwrap(), None);
    }

    #[test]
    fn sink_names_parse_from_tab_and_space_output() {
        let output = "0\talsa_output.foo\tPipeWire\ts16le\tRUNNING\n\
                      1 alsa_output.bar PipeWire s16le RUNNING\n";
        let names: Vec<&str> = parse_sink_names(output).collect();
        assert_eq!(names, vec!["alsa_output.foo", "alsa_output.bar"]);
    }

    struct TempTrack {
        dir: PathBuf,
        name: String,
    }

    impl TempTrack {
        fn new(name: &str) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let dir =
                std::env::temp_dir().join(format!("ozma-audio-router-{}-{id}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(name), b"reference").unwrap();
            Self {
                dir,
                name: name.to_owned(),
            }
        }
    }

    impl Drop for TempTrack {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[tokio::test]
    async fn pipewire_playback_spawns_pw_play_and_stop_kills_the_child() {
        let track = TempTrack::new("sweep.flac");
        let fake = Arc::new(FakeRunner::with_responses(vec![Ok(sink_list("ozma-vm1"))]));
        let router = PipeWireAudioRouter::with_config(
            fake.clone(),
            RouterConfig::default().with_track_root(&track.dir),
        );

        router
            .play_reference(AudioPlaybackSpec::new(&track.name, "ozma-vm1").unwrap())
            .await
            .unwrap();

        let expected_path = track.dir.join(&track.name);
        let spawns = fake.spawns();
        assert_eq!(spawns.len(), 1);
        assert_eq!(
            spawns[0],
            vec![
                "pw-play".to_owned(),
                "--target".to_owned(),
                "ozma-vm1".to_owned(),
                expected_path.to_string_lossy().into_owned(),
            ]
        );

        router.stop_playback().await.unwrap();
        assert_eq!(fake.killed(), vec![ChildId(1)]);
        router.stop_playback().await.unwrap();
        assert_eq!(fake.killed().len(), 1);
    }

    #[tokio::test]
    async fn pulseaudio_playback_uses_paplay_device_flag() {
        let track = TempTrack::new("sweep.wav");
        let fake = Arc::new(FakeRunner::with_responses(vec![Ok(sink_list("ozma-vm2"))]));
        let router = PulseAudioAudioRouter::with_config(
            fake.clone(),
            RouterConfig::default().with_track_root(&track.dir),
        );

        router
            .play_reference(AudioPlaybackSpec::new(&track.name, "ozma-vm2").unwrap())
            .await
            .unwrap();

        assert_eq!(
            fake.spawns()[0],
            vec![
                "paplay".to_owned(),
                "--device=ozma-vm2".to_owned(),
                track.dir.join(&track.name).to_string_lossy().into_owned(),
            ]
        );
    }

    #[tokio::test]
    async fn missing_track_is_a_typed_error_and_does_not_spawn() {
        let track = TempTrack::new("present.flac");
        let fake = Arc::new(FakeRunner::default());
        let router = PipeWireAudioRouter::with_config(
            fake.clone(),
            RouterConfig::default().with_track_root(&track.dir),
        );

        let error = router
            .play_reference(AudioPlaybackSpec::new("absent.flac", "ozma-vm1").unwrap())
            .await
            .unwrap_err();

        assert!(matches!(error, AudioError::TrackNotFound { .. }));
        assert!(fake.spawns().is_empty());
    }

    #[tokio::test]
    async fn missing_playback_sink_is_refused_and_does_not_spawn() {
        let track = TempTrack::new("present.flac");
        // The empty sink list means `play_reference` must refuse before spawning.
        let fake = Arc::new(FakeRunner::with_responses(vec![Ok(
            CommandOutput::success(""),
        )]));
        let router = PipeWireAudioRouter::with_config(
            fake.clone(),
            RouterConfig::default().with_track_root(&track.dir),
        );

        let error = router
            .play_reference(AudioPlaybackSpec::new(&track.name, "ozma-absent").unwrap())
            .await
            .unwrap_err();

        match error {
            AudioError::Refused {
                operation, reason, ..
            } => {
                assert_eq!(operation, "play_reference");
                assert!(reason.contains("ozma-absent"), "{reason}");
            }
            other => panic!("expected Refused, got {other:?}"),
        }
        assert!(
            fake.spawns().is_empty(),
            "a missing sink must not reach pw-play/paplay"
        );
    }

    #[test]
    fn invalid_specs_are_rejected() {
        assert!(AudioSinkSpec::new("", "Ozma").is_err());
        assert!(AudioSinkSpec::new("ozma-vm1", "").is_err());
        assert!(AudioSinkSpec::new("ozma vm1", "Ozma").is_err());
        assert!(AudioSinkSpec::new("ozma-vm1", "Ozma VM1").is_err());
        assert!(AudioSinkSpec::new("ozma-vm1", "Ozma\u{1b}").is_err());
        let spec = AudioSinkSpec::new("ozma-vm1", "Ozma-VM1").unwrap();
        assert!(spec.clone().with_channels(0).is_err());
        assert!(spec.clone().with_channels(MAX_SINK_CHANNELS + 1).is_err());
        assert!(spec.clone().with_rate(MIN_SINK_RATE - 1).is_err());
        assert!(spec.clone().with_rate(MAX_SINK_RATE + 1).is_err());
        assert!(spec.with_channels(1).unwrap().with_rate(44_100).is_ok());

        assert!(AudioProfile::new("", "ozma-vm1").is_err());
        assert!(AudioProfile::new("harman", "").is_err());
        assert!(AudioProfile::new("harman", "ozma-vm1")
            .unwrap()
            .with_volume(101)
            .is_err());
        assert!(AudioProfile::new("harman", "ozma-vm1")
            .unwrap()
            .with_source("bad source")
            .is_err());
        assert!(AudioProfile::for_source("mic", "").is_err());
        assert!(AudioProfile::for_source("mic", "bad source").is_err());
        assert!(AudioProfile::for_source("mic", "ozma-vm1-mic")
            .unwrap()
            .is_source_only());
        assert!(!AudioProfile::new("harman", "ozma-vm1")
            .unwrap()
            .is_source_only());

        assert!(AudioSourceSpec::new("", "Mic").is_err());
        assert!(AudioSourceSpec::new("ozma-vm1-mic", "").is_err());
        assert!(AudioSourceSpec::new("ozma vm1 mic", "Mic").is_err());
        assert!(AudioSourceSpec::from_master("ozma-vm1-mic", "Mic", "bad master").is_err());
        assert!(AudioSourceSpec::monitor("ozma-vm1-mic", "Mic", "ozma-vm1").is_ok());
        assert!(AudioSourceSpec::monitor("ozma-vm1-mic", "Mic", "").is_err());
        assert!(AudioSourceSpec::monitor("ozma-vm1-mic", "Mic", "bad sink").is_err());
        let source_spec = AudioSourceSpec::new("ozma-vm1-mic", "Mic").unwrap();
        assert!(source_spec.clone().with_channels(0).is_err());
        assert!(source_spec
            .clone()
            .with_channels(MAX_SOURCE_CHANNELS + 1)
            .is_err());
        assert!(source_spec.with_channels(1).is_ok());

        assert!(AudioSourceProfile::new("", "ozma-vm1-mic").is_err());
        assert!(AudioSourceProfile::new("mic", "").is_err());
        assert!(AudioSourceProfile::new("mic", "ozma-vm1-mic")
            .unwrap()
            .with_volume(101)
            .is_err());

        assert!(AudioPlaybackSpec::new("../sweep.flac", "ozma-vm1").is_err());
        assert!(AudioPlaybackSpec::new("dir/sweep.flac", "ozma-vm1").is_err());
        assert!(AudioPlaybackSpec::new("-sweep.flac", "ozma-vm1").is_err());
        assert!(AudioPlaybackSpec::new("", "ozma-vm1").is_err());
        assert!(AudioPlaybackSpec::new("sweep.flac", "").is_err());
    }

    #[test]
    fn router_is_object_safe() {
        let runner: Arc<dyn CommandRunner> = Arc::new(FakeRunner::default());
        let _pipewire: Box<dyn AudioRouter> = Box::new(PipeWireAudioRouter::new(runner.clone()));
        let _pulse: Box<dyn AudioRouter> = Box::new(PulseAudioAudioRouter::new(runner));
    }
}
