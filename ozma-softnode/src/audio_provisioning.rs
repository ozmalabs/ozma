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
//! * It does not verify audible output or route QEMU streams beyond passing
//!   the sink name to `ozma-node` and `PULSE_SINK` to the libvirt XML.

use std::collections::HashMap;
use std::path::PathBuf;
#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use clap::ValueEnum;
use thiserror::Error;

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
}

impl Default for AudioProvisionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            backend: AudioBackend::None,
            sink_prefix: DEFAULT_AUDIO_SINK_PREFIX.to_owned(),
            track_root: None,
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

/// Real adapter over the crate's `AudioRouter`.
pub struct RouterAudioProvisioner {
    router: Arc<dyn AudioRouter>,
    config: AudioProvisionConfig,
    /// VM -> whether this process loaded the module. It survives repeated
    /// `ensure` calls so a later reuse still belongs to us.
    owned: Mutex<HashMap<String, bool>>,
}

impl RouterAudioProvisioner {
    pub fn new(router: Arc<dyn AudioRouter>, config: AudioProvisionConfig) -> Self {
        Self {
            router,
            config,
            owned: Mutex::new(HashMap::new()),
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

        let mut owned = self.owned.lock().unwrap();
        let owned_by_us = owned.get(vm_name).copied().unwrap_or(false) || state.created;
        owned.insert(vm_name.to_owned(), owned_by_us);

        Ok(ProvisionedSink {
            name: state.name,
            monitor_target: state.monitor_target,
            created: state.created,
        })
    }

    async fn release(&self, vm_name: &str) -> Result<(), AudioProvisionError> {
        let owned = self
            .owned
            .lock()
            .unwrap()
            .get(vm_name)
            .copied()
            .unwrap_or(false);
        if !owned {
            return Ok(());
        }

        let name = self.config.sink_name(vm_name);
        self.router
            .remove_sink(&name)
            .await
            .map_err(|error| AudioProvisionError::router(vm_name, error))?;
        self.owned.lock().unwrap().remove(vm_name);
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
    use ozma_audio::{AudioPlaybackSpec, AudioProfile, AudioSinkState, AudioStreamRef};

    #[derive(Default)]
    struct FakeRouter {
        sinks: Mutex<Vec<AudioSinkSpec>>,
        removed: Mutex<Vec<String>>,
        existing: AtomicBool,
        fail: AtomicBool,
    }

    impl FakeRouter {
        fn sinks(&self) -> Vec<AudioSinkSpec> {
            self.sinks.lock().unwrap().clone()
        }

        fn removed(&self) -> Vec<String> {
            self.removed.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl AudioRouter for FakeRouter {
        async fn ensure_sink(&self, spec: AudioSinkSpec) -> Result<AudioSinkState, AudioError> {
            self.sinks.lock().unwrap().push(spec.clone());
            if self.fail.load(Ordering::SeqCst) {
                return Err(AudioError::Unsupported {
                    feature: "fake".to_owned(),
                    remediation: "do not fail".to_owned(),
                });
            }
            Ok(AudioSinkState {
                monitor_target: format!("{}.monitor", spec.name),
                name: spec.name,
                created: !self.existing.load(Ordering::SeqCst),
            })
        }

        async fn remove_sink(&self, name: &str) -> Result<(), AudioError> {
            self.removed.lock().unwrap().push(name.to_owned());
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

    #[test]
    fn defaults_are_off() {
        let config = AudioProvisionConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.backend, AudioBackend::None);
        assert_eq!(config.sink_prefix, DEFAULT_AUDIO_SINK_PREFIX);
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

    #[tokio::test]
    async fn router_provisioner_creates_named_sink_and_monitor_target() {
        let router = Arc::new(FakeRouter::default());
        let provisioner = RouterAudioProvisioner::new(router.clone(), config());

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
    }

    #[tokio::test]
    async fn router_provisioner_reuses_an_existing_sink_without_removing_it() {
        let router = Arc::new(FakeRouter::default());
        router.existing.store(true, Ordering::SeqCst);
        let provisioner = RouterAudioProvisioner::new(router.clone(), config());

        let sink = provisioner.ensure("vm-a").await.unwrap();
        assert!(!sink.created);
        provisioner.release("vm-a").await.unwrap();
        assert!(router.removed().is_empty(), "externally-owned sink is kept");
    }

    #[tokio::test]
    async fn router_provisioner_removes_a_sink_it_created() {
        let router = Arc::new(FakeRouter::default());
        let provisioner = RouterAudioProvisioner::new(router.clone(), config());

        provisioner.ensure("vm-a").await.unwrap();
        // A second ensure reuses it (the server already has the sink);
        // ownership must survive.
        router.existing.store(true, Ordering::SeqCst);
        let reused = provisioner.ensure("vm-a").await.unwrap();
        assert!(!reused.created);
        provisioner.release("vm-a").await.unwrap();

        assert_eq!(router.removed(), vec!["ozma-vm-a".to_owned()]);
    }

    #[tokio::test]
    async fn router_provisioner_maps_failure_without_returning_a_sink() {
        let router = Arc::new(FakeRouter::default());
        router.fail.store(true, Ordering::SeqCst);
        let provisioner = RouterAudioProvisioner::new(router, config());

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
