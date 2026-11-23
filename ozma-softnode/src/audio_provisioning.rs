//! Opt-in per-VM audio sink provisioning.
//!
//! The real adapter sits on top of [`ozma_audio::router::AudioRouter`]. It
//! creates one named null sink per VM (default `ozma-<vm>`) and keeps the
//! conventional `<sink>.monitor` target available for capture. The router
//! already talks to `pactl` without a shell; this module only decides which
//! sink belongs to which VM and whether this process owns it.
//!
//! Creation is **opt-in** and non-fatal. If the audio server or `pactl` is
//! unavailable, the supervisor logs the typed failure and starts the VM node
//! without `--audio-sink`. No sink name is reported as ready unless the router
//! returned one.
//!
//! Honest scope:
//!
//! * This controls an existing PipeWire/PulseAudio graph. It does not start or
//!   configure the audio server.
//! * The sink is a `module-null-sink`; the monitor target is always
//!   `<name>.monitor`.
//! * Only sinks this process created are removed on VM stop. A pre-existing
//!   sink with the same name is reused and left in place.
//! * Ownership is persisted to a small JSON file so a restarted supervisor can
//!   still clean up sinks it created earlier. An existing sink that is not
//!   recorded there is never claimed or removed.
//! * It does not verify audible output or route QEMU streams beyond passing
//!   the sink name to `ozma-node` and `PULSE_SINK` to the libvirt XML.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::Utc;
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{debug, warn};

use ozma_audio::{
    AudioError, AudioRouter, AudioSinkSpec, PipeWireAudioRouter, PulseAudioAudioRouter,
    RouterConfig, TokioCommandRunner, DEFAULT_SINK_CHANNELS, DEFAULT_SINK_RATE,
};

/// Default per-VM sink prefix, matching the Python soft node.
pub const DEFAULT_AUDIO_SINK_PREFIX: &str = "ozma-";

/// Audio backend selected by the operator.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum AudioBackend {
    /// No per-VM sink provisioning.
    #[default]
    #[value(name = "none")]
    None,
    /// Use `pactl` against `pipewire-pulse`.
    #[value(name = "pipewire")]
    PipeWire,
    /// Use `pactl` against PulseAudio.
    #[value(name = "pulseaudio")]
    PulseAudio,
}

impl AudioBackend {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::PipeWire => "pipewire",
            Self::PulseAudio => "pulseaudio",
        }
    }
}

impl std::fmt::Display for AudioBackend {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Operator-selected audio provisioning behaviour.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioProvisionConfig {
    /// Whether the supervisor should create per-VM sinks. Off by default.
    pub enabled: bool,
    /// PipeWire or PulseAudio. `None` is invalid when `enabled` is true.
    pub backend: AudioBackend,
    /// Sink name prefix. The full name is `<prefix><vm>`.
    pub sink_prefix: String,
    /// Directory used by the router for reference tracks.
    pub track_root: Option<PathBuf>,
    /// JSON file that records sink ownership across supervisor restarts.
    pub state_path: PathBuf,
}

impl Default for AudioProvisionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            backend: AudioBackend::None,
            sink_prefix: DEFAULT_AUDIO_SINK_PREFIX.to_owned(),
            track_root: None,
            state_path: default_state_path(),
        }
    }
}

impl AudioProvisionConfig {
    /// Validate flag combinations before any VM is touched.
    pub fn validate(&self) -> Result<(), AudioProvisionError> {
        if self.enabled && self.backend == AudioBackend::None {
            return Err(AudioProvisionError::InvalidConfig {
                detail: "--provision-audio requires --audio-backend pipewire|pulseaudio".to_owned(),
            });
        }
        if !self.enabled && self.backend != AudioBackend::None {
            return Err(AudioProvisionError::InvalidConfig {
                detail: "--audio-backend requires --provision-audio".to_owned(),
            });
        }
        if !self.enabled && self.track_root.is_some() {
            return Err(AudioProvisionError::InvalidConfig {
                detail: "--audio-track-root requires --provision-audio".to_owned(),
            });
        }
        validate_sink_prefix(&self.sink_prefix)?;
        if let Some(root) = &self.track_root {
            if root.as_os_str().is_empty() {
                return Err(AudioProvisionError::InvalidConfig {
                    detail: "audio track_root must not be empty".to_owned(),
                });
            }
            if let Some(text) = root.to_str() {
                if text.contains('\0') || text.contains('\n') || text.contains('\r') {
                    return Err(AudioProvisionError::InvalidConfig {
                        detail: "audio track_root contains a control character".to_owned(),
                    });
                }
            }
        }
        if self.state_path.as_os_str().is_empty() {
            return Err(AudioProvisionError::InvalidConfig {
                detail: "audio state_path must not be empty".to_owned(),
            });
        }
        if let Some(text) = self.state_path.to_str() {
            if text.contains('\0') || text.contains('\n') || text.contains('\r') {
                return Err(AudioProvisionError::InvalidConfig {
                    detail: "audio state_path contains a control character".to_owned(),
                });
            }
        }
        Ok(())
    }

    /// Full sink name for a VM.
    pub fn sink_name(&self, vm_name: &str) -> String {
        format!("{}{}", self.sink_prefix, vm_name)
    }
}

fn validate_sink_prefix(prefix: &str) -> Result<(), AudioProvisionError> {
    if prefix.is_empty() {
        return Err(AudioProvisionError::InvalidConfig {
            detail: "audio sink prefix must not be empty".to_owned(),
        });
    }
    if prefix.starts_with('-') {
        return Err(AudioProvisionError::InvalidConfig {
            detail: "audio sink prefix must not start with `-`".to_owned(),
        });
    }
    if let Some(character) = prefix.chars().find(|character| {
        !(character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-'))
    }) {
        return Err(AudioProvisionError::InvalidConfig {
            detail: format!(
                "audio sink prefix must contain only letters, digits, `.`, `_`, or `-`; \
                 found {character:?}"
            ),
        });
    }
    Ok(())
}

/// Typed per-VM audio provisioning failure.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum AudioProvisionError {
    #[error("per-VM audio provisioning is disabled")]
    Disabled,
    #[error("per-VM audio provisioning has no backend; configure pipewire or pulseaudio")]
    NoBackend,
    #[error("invalid audio provisioning configuration: {detail}")]
    InvalidConfig { detail: String },
    #[error("audio sink provisioning failed for VM {vm:?}: {detail}")]
    Router { vm: String, detail: String },
}

impl AudioProvisionError {
    fn router(vm: &str, error: AudioError) -> Self {
        Self::Router {
            vm: vm.to_owned(),
            detail: error.to_string(),
        }
    }
}

/// Result of ensuring a per-VM sink.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProvisionedSink {
    /// Sink name, exactly as requested.
    pub name: String,
    /// Conventional monitor target, `<name>.monitor`.
    pub monitor_target: String,
    /// True when this call loaded the null-sink module.
    pub created: bool,
}

/// Provisioning seam used by the supervisor.
#[async_trait]
pub trait AudioSinkProvisioner: Send + Sync {
    /// Create or reuse the VM's sink.
    async fn ensure(&self, vm_name: &str) -> Result<ProvisionedSink, AudioProvisionError>;

    /// Remove the sink only if this provisioner created it. An unknown or
    /// externally-owned sink is a no-op.
    async fn release(&self, vm_name: &str) -> Result<(), AudioProvisionError>;
}

/// Version of the on-disk audio provisioning state schema.
pub const AUDIO_PROVISION_STATE_VERSION: u32 = 1;
/// State file name used under the runtime directory.
pub const AUDIO_PROVISION_STATE_FILE: &str = "audio-provision.json";

/// Default state file: `$XDG_RUNTIME_DIR/ozma/audio-provision.json`, falling
/// back to `/run/ozma/audio-provision.json`.
pub fn default_state_path() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir)
            .join("ozma")
            .join(AUDIO_PROVISION_STATE_FILE),
        _ => PathBuf::from("/run/ozma").join(AUDIO_PROVISION_STATE_FILE),
    }
}

/// One sink this tooling created, persisted across supervisor restarts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnedAudioSink {
    /// VM name the sink belongs to.
    pub vm: String,
    /// Exact sink name reported by the router at creation time.
    pub sink: String,
    /// Always true for records this module writes. Kept in the schema so a
    /// hand-edited or future record can be ignored without guessing.
    pub created: bool,
    /// RFC 3339 creation timestamp.
    pub created_at: String,
}

/// On-disk state schema. Unknown fields are ignored; unsupported versions are
/// discarded rather than guessed at.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioProvisionState {
    pub version: u32,
    pub sinks: Vec<OwnedAudioSink>,
}

/// Persistent ownership record for per-VM sinks.
///
/// Loading is best-effort: a missing file starts empty with a debug diagnostic;
/// an unreadable, corrupt, or unsupported file starts empty and logs a warning.
/// Saving writes a temporary file in the same directory and renames it into
/// place, so a crash cannot leave a partially written JSON document.
pub struct AudioProvisionStateStore {
    path: PathBuf,
    state: Mutex<AudioProvisionState>,
}

impl AudioProvisionStateStore {
    /// Load the state at `path` (or start empty when it cannot be used).
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let state = load_state(&path);
        Self {
            path,
            state: Mutex::new(state),
        }
    }

    /// The state file this store reads and writes.
    pub fn state_path(&self) -> &Path {
        &self.path
    }

    /// Whether `sink` is recorded as created by this tooling for `vm`.
    pub fn is_owned(&self, vm: &str, sink: &str) -> bool {
        self.state
            .lock()
            .unwrap()
            .sinks
            .iter()
            .any(|record| record.created && record.vm == vm && record.sink == sink)
    }

    /// Sinks recorded as owned for a VM, in state-file order.
    pub fn owned_sinks(&self, vm: &str) -> Vec<String> {
        self.state
            .lock()
            .unwrap()
            .sinks
            .iter()
            .filter(|record| record.created && record.vm == vm)
            .map(|record| record.sink.clone())
            .collect()
    }

    /// Record a sink that the router reported creating. Repeated calls for the
    /// same VM/sink are idempotent; a different sink for the same VM is kept
    /// alongside the old record so both can be cleaned up.
    pub fn record_created(&self, vm: &str, sink: &str) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        if state
            .sinks
            .iter()
            .any(|record| record.created && record.vm == vm && record.sink == sink)
        {
            return Ok(());
        }
        state
            .sinks
            .retain(|record| !(record.vm == vm && record.sink == sink));
        state.sinks.push(OwnedAudioSink {
            vm: vm.to_owned(),
            sink: sink.to_owned(),
            created: true,
            created_at: Utc::now().to_rfc3339(),
        });
        self.persist(&state)
    }

    /// Remove one ownership record. An unknown record is a successful no-op.
    pub fn forget(&self, vm: &str, sink: &str) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        let before = state.sinks.len();
        state
            .sinks
            .retain(|record| !(record.vm == vm && record.sink == sink));
        if state.sinks.len() == before {
            return Ok(());
        }
        self.persist(&state)
    }

    /// Persist the current state (also used after external reconciliation).
    #[allow(dead_code)]
    pub fn save(&self) -> io::Result<()> {
        let state = self.state.lock().unwrap();
        self.persist(&state)
    }

    fn persist(&self, state: &AudioProvisionState) -> io::Result<()> {
        let mut snapshot = state.clone();
        snapshot.version = AUDIO_PROVISION_STATE_VERSION;
        snapshot
            .sinks
            .sort_by(|a, b| a.vm.cmp(&b.vm).then_with(|| a.sink.cmp(&b.sink)));

        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }

        let json = serde_json::to_vec_pretty(&snapshot)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let temp = self.temp_path();
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)?;
            file.write_all(&json)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temp, &self.path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    }

    fn temp_path(&self) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
        let name = self
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(AUDIO_PROVISION_STATE_FILE);
        let temp_name = format!(".{name}.tmp-{}-{sequence}", std::process::id());
        match self.path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.join(temp_name),
            _ => PathBuf::from(temp_name),
        }
    }
}

fn load_state(path: &Path) -> AudioProvisionState {
    match fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<AudioProvisionState>(&text) {
            Ok(mut state) if state.version == AUDIO_PROVISION_STATE_VERSION => {
                state.sinks.retain(|record| {
                    record.created && !record.vm.is_empty() && !record.sink.is_empty()
                });
                state
            }
            Ok(state) => {
                warn!(
                    "Audio provisioning state {} has unsupported version {}; \
                     starting with no owned sinks",
                    path.display(),
                    state.version
                );
                AudioProvisionState::default()
            }
            Err(error) => {
                warn!(
                    "Could not parse audio provisioning state {}: {}; \
                     starting with no owned sinks",
                    path.display(),
                    error
                );
                AudioProvisionState::default()
            }
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            debug!(
                "Audio provisioning state {} does not exist yet",
                path.display()
            );
            AudioProvisionState::default()
        }
        Err(error) => {
            warn!(
                "Could not read audio provisioning state {}: {}; \
                 starting with no owned sinks",
                path.display(),
                error
            );
            AudioProvisionState::default()
        }
    }
}

/// Real adapter over the crate's `AudioRouter`.
pub struct RouterAudioProvisioner {
    router: Arc<dyn AudioRouter>,
    config: AudioProvisionConfig,
    /// Ownership loaded from disk at construction and updated on every
    /// successful create/release.
    state: AudioProvisionStateStore,
}

impl RouterAudioProvisioner {
    pub fn new(router: Arc<dyn AudioRouter>, config: AudioProvisionConfig) -> Self {
        let state = AudioProvisionStateStore::new(config.state_path.clone());
        Self {
            router,
            config,
            state,
        }
    }
}

#[async_trait]
impl AudioSinkProvisioner for RouterAudioProvisioner {
    async fn ensure(&self, vm_name: &str) -> Result<ProvisionedSink, AudioProvisionError> {
        if !self.config.enabled {
            return Err(AudioProvisionError::Disabled);
        }
        if self.config.backend == AudioBackend::None {
            return Err(AudioProvisionError::NoBackend);
        }

        let name = self.config.sink_name(vm_name);
        let spec = AudioSinkSpec::new(name.clone(), format!("Ozma-{vm_name}"))
            .and_then(|spec| spec.with_channels(DEFAULT_SINK_CHANNELS))
            .and_then(|spec| spec.with_rate(DEFAULT_SINK_RATE))
            .map(|spec| spec.with_monitor(true))
            .map_err(|error| AudioProvisionError::router(vm_name, error))?;

        let state = self
            .router
            .ensure_sink(spec)
            .await
            .map_err(|error| AudioProvisionError::router(vm_name, error))?;

        if state.created {
            // Only the router's created flag makes a sink ours. Persist it so a
            // restarted supervisor can still clean it up.
            if let Err(error) = self.state.record_created(vm_name, &state.name) {
                warn!(
                    "VM {}: created audio sink {} but could not persist ownership \
                     to {}: {}; continuing",
                    vm_name,
                    state.name,
                    self.state.state_path().display(),
                    error
                );
            }
        } else if !self.state.is_owned(vm_name, &state.name) {
            // A pre-existing sink with the same name is reused but never
            // claimed, even if this supervisor restarted after another process
            // created it.
            debug!(
                "VM {}: reusing audio sink {} without claiming ownership",
                vm_name, state.name
            );
        }

        Ok(ProvisionedSink {
            name: state.name,
            monitor_target: state.monitor_target,
            created: state.created,
        })
    }

    async fn release(&self, vm_name: &str) -> Result<(), AudioProvisionError> {
        let owned = self.state.owned_sinks(vm_name);
        for sink in owned {
            self.router
                .remove_sink(&sink)
                .await
                .map_err(|error| AudioProvisionError::router(vm_name, error))?;
            if let Err(error) = self.state.forget(vm_name, &sink) {
                warn!(
                    "VM {}: removed audio sink {} but could not persist its \
                     release to {}: {}; continuing",
                    vm_name,
                    sink,
                    self.state.state_path().display(),
                    error
                );
            }
        }
        Ok(())
    }
}

/// Build the production provisioner for a configuration, if enabled.
pub fn provisioner_from_config(
    config: &AudioProvisionConfig,
) -> Result<Option<Arc<dyn AudioSinkProvisioner>>, AudioProvisionError> {
    if !config.enabled {
        return Ok(None);
    }
    config.validate()?;

    let router_config = match &config.track_root {
        Some(root) => RouterConfig::default().with_track_root(root.clone()),
        None => RouterConfig::default(),
    };

    let router: Arc<dyn AudioRouter> = match config.backend {
        AudioBackend::None => return Err(AudioProvisionError::NoBackend),
        AudioBackend::PipeWire => Arc::new(PipeWireAudioRouter::with_config(
            Arc::new(TokioCommandRunner::new()),
            router_config,
        )),
        AudioBackend::PulseAudio => Arc::new(PulseAudioAudioRouter::with_config(
            Arc::new(TokioCommandRunner::new()),
            router_config,
        )),
    };

    Ok(Some(Arc::new(RouterAudioProvisioner::new(
        router,
        config.clone(),
    ))))
}

/// Deterministic in-memory provisioner for tests and embedders. It never runs
/// `pactl`.
#[cfg(test)]
pub struct FakeAudioSinkProvisioner {
    prefix: String,
    fail: AtomicBool,
    created: AtomicBool,
    ensured: Mutex<Vec<String>>,
    released: Mutex<Vec<String>>,
}

#[cfg(test)]
impl FakeAudioSinkProvisioner {
    pub fn new(prefix: impl Into<String>) -> Self {
        Self {
            prefix: prefix.into(),
            fail: AtomicBool::new(false),
            created: AtomicBool::new(true),
            ensured: Mutex::new(Vec::new()),
            released: Mutex::new(Vec::new()),
        }
    }

    /// Make every `ensure` fail with a typed router error.
    pub fn set_fail(&self, fail: bool) {
        self.fail.store(fail, Ordering::SeqCst);
    }

    /// Control whether `ensure` reports a newly created sink.
    pub fn set_created(&self, created: bool) {
        self.created.store(created, Ordering::SeqCst);
    }

    pub fn ensured(&self) -> Vec<String> {
        self.ensured.lock().unwrap().clone()
    }

    pub fn released(&self) -> Vec<String> {
        self.released.lock().unwrap().clone()
    }
}

#[cfg(test)]
#[async_trait]
impl AudioSinkProvisioner for FakeAudioSinkProvisioner {
    async fn ensure(&self, vm_name: &str) -> Result<ProvisionedSink, AudioProvisionError> {
        self.ensured.lock().unwrap().push(vm_name.to_owned());
        if self.fail.load(Ordering::SeqCst) {
            return Err(AudioProvisionError::Router {
                vm: vm_name.to_owned(),
                detail: "fake audio failure".to_owned(),
            });
        }

        let name = format!("{}{}", self.prefix, vm_name);
        Ok(ProvisionedSink {
            monitor_target: format!("{name}.monitor"),
            name,
            created: self.created.load(Ordering::SeqCst),
        })
    }

    async fn release(&self, vm_name: &str) -> Result<(), AudioProvisionError> {
        self.released.lock().unwrap().push(vm_name.to_owned());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::fs;
    use std::path::Path;
    use tempfile::{tempdir, TempDir};

    use ozma_audio::{AudioPlaybackSpec, AudioProfile, AudioSinkState, AudioStreamRef};

    #[derive(Default)]
    struct FakeRouter {
        sinks: Mutex<Vec<AudioSinkSpec>>,
        existing: Mutex<HashSet<String>>,
        removed: Mutex<Vec<String>>,
        fail: AtomicBool,
    }

    impl FakeRouter {
        fn sinks(&self) -> Vec<AudioSinkSpec> {
            self.sinks.lock().unwrap().clone()
        }

        fn removed(&self) -> Vec<String> {
            self.removed.lock().unwrap().clone()
        }

        /// Make a sink exist without recording it as created by this process.
        fn precreate(&self, name: &str) {
            self.existing.lock().unwrap().insert(name.to_owned());
        }

        /// Make a sink disappear without going through `remove_sink`.
        fn forget_silently(&self, name: &str) {
            self.existing.lock().unwrap().remove(name);
        }
    }

    #[async_trait]
    impl AudioRouter for FakeRouter {
        async fn ensure_sink(&self, spec: AudioSinkSpec) -> Result<AudioSinkState, AudioError> {
            if self.fail.load(Ordering::SeqCst) {
                return Err(AudioError::Unsupported {
                    feature: "fake".to_owned(),
                    remediation: "do not fail".to_owned(),
                });
            }
            let created = self.existing.lock().unwrap().insert(spec.name.clone());
            if created {
                self.sinks.lock().unwrap().push(spec.clone());
            }
            Ok(AudioSinkState {
                monitor_target: format!("{}.monitor", spec.name),
                name: spec.name,
                created,
            })
        }

        async fn remove_sink(&self, name: &str) -> Result<(), AudioError> {
            self.removed.lock().unwrap().push(name.to_owned());
            self.existing.lock().unwrap().remove(name);
            Ok(())
        }

        async fn move_stream(
            &self,
            _stream: AudioStreamRef,
            _sink: &str,
        ) -> Result<(), AudioError> {
            Ok(())
        }

        async fn apply_profile(&self, _profile: AudioProfile) -> Result<(), AudioError> {
            Ok(())
        }

        async fn remove_profile(&self, _profile_id: &str) -> Result<(), AudioError> {
            Ok(())
        }

        async fn play_reference(&self, _spec: AudioPlaybackSpec) -> Result<(), AudioError> {
            Ok(())
        }

        async fn stop_playback(&self) -> Result<(), AudioError> {
            Ok(())
        }
    }

    fn config() -> AudioProvisionConfig {
        AudioProvisionConfig {
            enabled: true,
            backend: AudioBackend::PipeWire,
            ..AudioProvisionConfig::default()
        }
    }

    fn config_at(state_path: &Path) -> AudioProvisionConfig {
        AudioProvisionConfig {
            state_path: state_path.to_path_buf(),
            ..config()
        }
    }

    fn state_path(dir: &TempDir) -> PathBuf {
        dir.path().join(AUDIO_PROVISION_STATE_FILE)
    }

    fn read_state(path: &Path) -> AudioProvisionState {
        let text = fs::read_to_string(path).expect("state file exists");
        serde_json::from_str(&text).expect("state file is valid JSON")
    }

    #[test]
    fn defaults_are_off() {
        let config = AudioProvisionConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.backend, AudioBackend::None);
        assert_eq!(config.sink_prefix, DEFAULT_AUDIO_SINK_PREFIX);
        assert!(config.state_path.ends_with(AUDIO_PROVISION_STATE_FILE));
    }

    #[test]
    fn validate_rejects_enabled_without_backend() {
        let config = AudioProvisionConfig {
            enabled: true,
            ..AudioProvisionConfig::default()
        };
        assert!(matches!(
            config.validate(),
            Err(AudioProvisionError::InvalidConfig { .. })
        ));
    }

    #[test]
    fn validate_rejects_backend_without_enable() {
        let config = AudioProvisionConfig {
            backend: AudioBackend::PulseAudio,
            ..AudioProvisionConfig::default()
        };
        assert!(matches!(
            config.validate(),
            Err(AudioProvisionError::InvalidConfig { .. })
        ));

        let track_root = AudioProvisionConfig {
            track_root: Some(PathBuf::from("/srv/tracks")),
            ..AudioProvisionConfig::default()
        };
        assert!(matches!(
            track_root.validate(),
            Err(AudioProvisionError::InvalidConfig { .. })
        ));
    }

    #[test]
    fn validate_rejects_unsafe_sink_prefix() {
        let config = AudioProvisionConfig {
            sink_prefix: "bad prefix".to_owned(),
            ..config()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn validate_rejects_empty_state_path() {
        let config = AudioProvisionConfig {
            state_path: PathBuf::new(),
            ..config()
        };
        assert!(matches!(
            config.validate(),
            Err(AudioProvisionError::InvalidConfig { .. })
        ));
    }

    #[tokio::test]
    async fn router_provisioner_creates_named_sink_and_persists_ownership() {
        let dir = tempdir().unwrap();
        let path = state_path(&dir);
        let router = Arc::new(FakeRouter::default());
        let provisioner = RouterAudioProvisioner::new(router.clone(), config_at(&path));

        let sink = provisioner.ensure("vm-a").await.unwrap();
        assert_eq!(sink.name, "ozma-vm-a");
        assert_eq!(sink.monitor_target, "ozma-vm-a.monitor");
        assert!(sink.created);

        let specs = router.sinks();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "ozma-vm-a");
        assert_eq!(specs[0].description, "Ozma-vm-a");
        assert_eq!(specs[0].channels, 2);
        assert_eq!(specs[0].rate, 48_000);
        assert!(specs[0].monitor);

        let state = read_state(&path);
        assert_eq!(state.version, AUDIO_PROVISION_STATE_VERSION);
        assert_eq!(state.sinks.len(), 1);
        assert_eq!(state.sinks[0].vm, "vm-a");
        assert_eq!(state.sinks[0].sink, "ozma-vm-a");
        assert!(state.sinks[0].created);
        assert!(!state.sinks[0].created_at.is_empty());
    }

    #[tokio::test]
    async fn router_provisioner_reuses_an_unrecorded_existing_sink_without_removing_it() {
        let dir = tempdir().unwrap();
        let path = state_path(&dir);
        let router = Arc::new(FakeRouter::default());
        router.precreate("ozma-vm-a");
        let provisioner = RouterAudioProvisioner::new(router.clone(), config_at(&path));

        let sink = provisioner.ensure("vm-a").await.unwrap();
        assert!(!sink.created);
        assert!(!AudioProvisionStateStore::new(&path).is_owned("vm-a", "ozma-vm-a"));

        provisioner.release("vm-a").await.unwrap();
        assert!(router.removed().is_empty(), "externally-owned sink is kept");
        assert!(AudioProvisionStateStore::new(&path)
            .owned_sinks("vm-a")
            .is_empty());
    }

    #[tokio::test]
    async fn router_provisioner_removes_a_sink_it_created() {
        let dir = tempdir().unwrap();
        let path = state_path(&dir);
        let router = Arc::new(FakeRouter::default());
        let provisioner = RouterAudioProvisioner::new(router.clone(), config_at(&path));

        provisioner.ensure("vm-a").await.unwrap();
        // A second ensure reuses it (the server already has the sink);
        // ownership must survive.
        let reused = provisioner.ensure("vm-a").await.unwrap();
        assert!(!reused.created);
        provisioner.release("vm-a").await.unwrap();

        assert_eq!(router.removed(), vec!["ozma-vm-a".to_owned()]);
        assert!(read_state(&path).sinks.is_empty());
    }

    #[tokio::test]
    async fn release_removes_only_owned_sinks() {
        let dir = tempdir().unwrap();
        let path = state_path(&dir);
        let router = Arc::new(FakeRouter::default());
        router.precreate("ozma-vm-b");
        let provisioner = RouterAudioProvisioner::new(router.clone(), config_at(&path));

        assert!(provisioner.ensure("vm-a").await.unwrap().created);
        assert!(!provisioner.ensure("vm-b").await.unwrap().created);

        provisioner.release("vm-a").await.unwrap();
        provisioner.release("vm-b").await.unwrap();

        assert_eq!(router.removed(), vec!["ozma-vm-a".to_owned()]);
        assert!(read_state(&path).sinks.is_empty());
    }

    #[tokio::test]
    async fn restarted_provisioner_reuses_recorded_sink_and_keeps_ownership() {
        let dir = tempdir().unwrap();
        let path = state_path(&dir);
        let router = Arc::new(FakeRouter::default());

        let first = RouterAudioProvisioner::new(router.clone(), config_at(&path));
        assert!(first.ensure("vm-a").await.unwrap().created);
        drop(first);

        let restarted = RouterAudioProvisioner::new(router.clone(), config_at(&path));
        let sink = restarted.ensure("vm-a").await.unwrap();
        assert!(!sink.created, "restart must reuse the existing sink");
        assert!(AudioProvisionStateStore::new(&path).is_owned("vm-a", "ozma-vm-a"));

        restarted.release("vm-a").await.unwrap();
        assert_eq!(router.removed(), vec!["ozma-vm-a".to_owned()]);
        assert!(read_state(&path).sinks.is_empty());
    }

    #[tokio::test]
    async fn restarted_provisioner_does_not_claim_an_unrecorded_existing_sink() {
        let dir = tempdir().unwrap();
        let path = state_path(&dir);
        let router = Arc::new(FakeRouter::default());
        router.precreate("ozma-vm-a");

        let restarted = RouterAudioProvisioner::new(router.clone(), config_at(&path));
        assert!(!restarted.ensure("vm-a").await.unwrap().created);
        assert!(!AudioProvisionStateStore::new(&path).is_owned("vm-a", "ozma-vm-a"));

        restarted.release("vm-a").await.unwrap();
        assert!(
            router.removed().is_empty(),
            "an existing sink not recorded as owned must never be unloaded"
        );
    }

    #[tokio::test]
    async fn missing_and_corrupt_state_start_empty() {
        let dir = tempdir().unwrap();
        let path = state_path(&dir);

        let missing = AudioProvisionStateStore::new(&path);
        assert!(missing.owned_sinks("vm-a").is_empty());

        fs::write(&path, "{ this is not json").unwrap();
        let corrupt = AudioProvisionStateStore::new(&path);
        assert!(corrupt.owned_sinks("vm-a").is_empty());

        // A corrupt file must not turn an existing sink into an owned one.
        let router = Arc::new(FakeRouter::default());
        router.precreate("ozma-vm-a");
        let provisioner = RouterAudioProvisioner::new(router.clone(), config_at(&path));
        assert!(!provisioner.ensure("vm-a").await.unwrap().created);
        provisioner.release("vm-a").await.unwrap();
        assert!(router.removed().is_empty());
    }

    #[tokio::test]
    async fn atomic_write_creates_parents_and_leaves_no_temp_file() {
        let dir = tempdir().unwrap();
        let nested = dir.path().join("runtime").join("ozma");
        let path = nested.join(AUDIO_PROVISION_STATE_FILE);
        let router = Arc::new(FakeRouter::default());
        let provisioner = RouterAudioProvisioner::new(router, config_at(&path));

        provisioner.ensure("vm-a").await.unwrap();

        assert!(path.exists());
        let names: Vec<String> = fs::read_dir(&nested)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, vec![AUDIO_PROVISION_STATE_FILE.to_owned()]);
        assert!(!names.iter().any(|name| name.contains(".tmp")));
        assert_eq!(read_state(&path).sinks[0].sink, "ozma-vm-a");
    }

    #[tokio::test]
    async fn state_path_override_is_used_instead_of_the_default() {
        let dir = tempdir().unwrap();
        let custom = dir.path().join("custom").join("owned-sinks.json");
        let router = Arc::new(FakeRouter::default());
        let provisioner = RouterAudioProvisioner::new(router, config_at(&custom));

        provisioner.ensure("vm-a").await.unwrap();

        assert!(custom.exists());
        let state = read_state(&custom);
        assert_eq!(state.sinks.len(), 1);
        assert_eq!(state.sinks[0].sink, "ozma-vm-a");
        assert_eq!(
            AudioProvisionStateStore::new(&custom).state_path(),
            custom.as_path()
        );
    }

    #[tokio::test]
    async fn release_after_a_recorded_sink_is_gone_clears_the_record() {
        let dir = tempdir().unwrap();
        let path = state_path(&dir);
        let router = Arc::new(FakeRouter::default());

        let first = RouterAudioProvisioner::new(router.clone(), config_at(&path));
        first.ensure("vm-a").await.unwrap();
        router.forget_silently("ozma-vm-a");
        drop(first);

        let restarted = RouterAudioProvisioner::new(router.clone(), config_at(&path));
        restarted.release("vm-a").await.unwrap();

        assert_eq!(router.removed(), vec!["ozma-vm-a".to_owned()]);
        let state = read_state(&path);
        assert!(
            state.sinks.is_empty(),
            "a recorded sink that is gone must not stay owned"
        );
    }

    #[tokio::test]
    async fn state_write_failure_does_not_fail_ensure() {
        let dir = tempdir().unwrap();
        let blocker = dir.path().join("not-a-directory");
        fs::write(&blocker, b"x").unwrap();
        let path = blocker.join(AUDIO_PROVISION_STATE_FILE);
        let router = Arc::new(FakeRouter::default());
        let provisioner = RouterAudioProvisioner::new(router, config_at(&path));

        let sink = provisioner.ensure("vm-a").await.unwrap();
        assert!(sink.created);
    }

    #[tokio::test]
    async fn router_provisioner_maps_failure_without_returning_a_sink() {
        let dir = tempdir().unwrap();
        let path = state_path(&dir);
        let router = Arc::new(FakeRouter::default());
        router.fail.store(true, Ordering::SeqCst);
        let provisioner = RouterAudioProvisioner::new(router, config_at(&path));

        assert!(matches!(
            provisioner.ensure("vm-a").await,
            Err(AudioProvisionError::Router { .. })
        ));
    }

    #[tokio::test]
    async fn fake_provisioner_records_calls_without_pactl() {
        let provisioner = FakeAudioSinkProvisioner::new("ozma-");
        let sink = provisioner.ensure("vm-a").await.unwrap();
        assert_eq!(sink.name, "ozma-vm-a");
        assert_eq!(sink.monitor_target, "ozma-vm-a.monitor");
        provisioner.set_created(false);
        let reused = provisioner.ensure("vm-a").await.unwrap();
        assert!(!reused.created);
        provisioner.release("vm-a").await.unwrap();
        assert_eq!(provisioner.ensured(), vec!["vm-a", "vm-a"]);
        assert_eq!(provisioner.released(), vec!["vm-a"]);
    }

    #[tokio::test]
    async fn fake_provisioner_failure_is_non_panicking() {
        let provisioner = FakeAudioSinkProvisioner::new("ozma-");
        provisioner.set_fail(true);
        assert!(matches!(
            provisioner.ensure("vm-a").await,
            Err(AudioProvisionError::Router { .. })
        ));
    }
}
