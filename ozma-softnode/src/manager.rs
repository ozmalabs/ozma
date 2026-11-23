//! `VirtualNodeManager` — VM discovery, per-VM `ozma-node` supervision, status,
//! and agent provisioning.
//!
//! The manager is the hypervisor-side supervisor. It discovers running VMs
//! (Proxmox, libvirt, or a QMP socket scan), tracks their status, optionally
//! provisions the in-guest agent, and — when auto-management is enabled —
//! launches one real `ozma-node --profile vm` subprocess per discovered VM
//! that has a QMP socket.
//!
//! Child lifecycle is handled here: new VMs start, removed VMs stop, unchanged
//! VMs are left alone, and failed children are restarted with bounded
//! exponential backoff. Shutdown stops every child and waits for it.
//!
//! This is the subprocess transition plus the safe, opt-in libvirt provisioning
//! core. `--provision` can inject QMP/display chardevs, D-Bus p2p display,
//! evdev `input-linux` objects, and PulseAudio environment into libvirt XML;
//! `--provision-restart` separately allows the destroy/start cycle.
//! `--provision-evdev` can create Linux uinput devices and `--provision-audio`
//! can create per-VM null sinks. Failure of either device/sink creation is
//! non-fatal and falls back to no injected path. libvirt power, USB/media, and
//! Looking Glass remain separate work.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::sync::{watch, Mutex};
use tracing::{debug, info, warn};

use crate::agent::AgentProvisioner;
use crate::audio_provisioning::{
    provisioner_from_config as audio_provisioner_from_config, AudioProvisionConfig,
    AudioSinkProvisioner, ProvisionedSink,
};
use crate::discovery;
use crate::evdev_provisioning::{EvdevPaths, EvdevProvisioner, UinputEvdevProvisioner};
use crate::provisioning::{
    LibvirtProvisioner, ProvisionConfig, ProvisionHints, TokioCommandRunner, VmProvisioner,
};
use crate::supervisor::{
    build_node_spec, restart_delay, NodeHandle, NodeLauncher, NodeSpec, PortAllocator,
    ProcessNodeLauncher, DEFAULT_BASE_PORT, DEFAULT_NODE_BIN, DEFAULT_NODE_HTTP_BASE,
    DEFAULT_RESTART_BACKOFF_MAX_MS, DEFAULT_RESTART_BACKOFF_MS, HEALTHY_UPTIME,
};
use crate::vm_info::VMInfo;

/// How often discovery re-scans for VM additions/removals.
pub const DEFAULT_DISCOVERY_INTERVAL: Duration = Duration::from_secs(10);
/// How often children are polled and due restarts are launched.
pub const DEFAULT_SUPERVISION_TICK: Duration = Duration::from_secs(1);

/// Construction-time supervisor settings, mostly mirroring CLI flags.
#[derive(Clone, Debug)]
pub struct ManagerOptions {
    pub controller_url: String,
    pub qmp_dir: String,
    pub auto_manage: bool,
    pub auto_agent: bool,
    pub exclude_patterns: Vec<String>,
    /// Child binary. Defaults to `ozma-node`; this crate never spawns itself.
    pub node_bin: PathBuf,
    /// First HID UDP port.
    pub base_port: u16,
    /// First HTTP port.
    pub node_http_base: u16,
    /// First restart delay after a failure.
    pub restart_backoff: Duration,
    /// Restart delay cap.
    pub restart_backoff_max: Duration,
    /// Discovery re-scan interval.
    pub discovery_interval: Duration,
    /// Child poll / restart tick.
    pub supervision_tick: Duration,
    /// Opt-in libvirt provisioning for VMs without a QMP socket.
    pub provision: bool,
    /// Provisioning paths, environment, and restart behaviour.
    pub provision_config: ProvisionConfig,
    /// Opt-in per-VM audio sink provisioning.
    pub audio: AudioProvisionConfig,
}

impl Default for ManagerOptions {
    fn default() -> Self {
        Self {
            controller_url: String::new(),
            qmp_dir: String::new(),
            auto_manage: true,
            auto_agent: true,
            exclude_patterns: Vec::new(),
            node_bin: PathBuf::from(DEFAULT_NODE_BIN),
            base_port: DEFAULT_BASE_PORT,
            node_http_base: DEFAULT_NODE_HTTP_BASE,
            restart_backoff: Duration::from_millis(DEFAULT_RESTART_BACKOFF_MS),
            restart_backoff_max: Duration::from_millis(DEFAULT_RESTART_BACKOFF_MAX_MS),
            discovery_interval: DEFAULT_DISCOVERY_INTERVAL,
            supervision_tick: DEFAULT_SUPERVISION_TICK,
            provision: false,
            provision_config: ProvisionConfig::default(),
            audio: AudioProvisionConfig::default(),
        }
    }
}

// ── Managed state ──────────────────────────────────────────────────────────

struct ManagedVm {
    vm: VMInfo,
    agent_provisioned: bool,
    agent_check_failures: u32,
    /// `None` when auto-management is off, the VM has no QMP socket, or no
    /// ports could be allocated.
    node: Option<NodeState>,
    /// Suppress repeated "defined; restart required" warnings.
    provision_restart_warned: bool,
    /// Suppress repeated provisioning-failure warnings.
    provision_failure_warned: bool,
}

struct NodeState {
    spec: NodeSpec,
    handle: Option<NodeHandle>,
    /// Consecutive failures, used for the exponential restart delay.
    attempts: u32,
    next_restart: Option<Instant>,
    started_at: Option<Instant>,
    /// Guards against a second spawn while an async `launcher.spawn()` is in
    /// flight.
    spawning: bool,
}

impl NodeState {
    fn new(spec: NodeSpec) -> Self {
        Self {
            spec,
            handle: None,
            attempts: 0,
            next_restart: Some(Instant::now()),
            started_at: None,
            spawning: false,
        }
    }
}

struct SupervisorState {
    vms: HashMap<String, ManagedVm>,
    ports: PortAllocator,
}

// ── VirtualNodeManager ─────────────────────────────────────────────────────

pub struct VirtualNodeManager {
    options: Arc<ManagerOptions>,
    inner: Arc<ManagerInner>,
    stop_tx: watch::Sender<bool>,
    stop_rx: watch::Receiver<bool>,
}

struct ManagerInner {
    options: Arc<ManagerOptions>,
    launcher: Arc<dyn NodeLauncher>,
    provisioner: Arc<AgentProvisioner>,
    vm_provisioner: Arc<dyn VmProvisioner>,
    evdev_provisioner: Option<Arc<dyn EvdevProvisioner>>,
    audio_provisioner: Option<Arc<dyn AudioSinkProvisioner>>,
    state: Mutex<SupervisorState>,
}

impl VirtualNodeManager {
    /// Production constructor: spawns real `ozma-node` children.
    pub fn new(options: ManagerOptions) -> Self {
        Self::with_launcher(options, Arc::new(ProcessNodeLauncher))
    }

    /// Constructor with an injected launcher, used by tests.
    pub fn with_launcher(options: ManagerOptions, launcher: Arc<dyn NodeLauncher>) -> Self {
        let vm_provisioner: Arc<dyn VmProvisioner> = Arc::new(LibvirtProvisioner::new(
            options.provision_config.clone(),
            Arc::new(TokioCommandRunner),
        ));
        Self::with_launcher_and_provisioner(options, launcher, vm_provisioner)
    }

    /// Constructor with both seams injected, used by provisioning tests.
    ///
    /// Production evdev/audio provisioners are built from the options when
    /// those opt-ins are on. Defaults keep both disabled.
    pub fn with_launcher_and_provisioner(
        options: ManagerOptions,
        launcher: Arc<dyn NodeLauncher>,
        vm_provisioner: Arc<dyn VmProvisioner>,
    ) -> Self {
        let evdev = if options.provision_config.evdev.enabled {
            Some(Arc::new(UinputEvdevProvisioner::new(
                options.provision_config.evdev.clone(),
            )) as Arc<dyn EvdevProvisioner>)
        } else {
            None
        };
        let audio = audio_provisioner_from_config(&options.audio).ok().flatten();
        Self::with_all_provisioners(options, launcher, vm_provisioner, evdev, audio)
    }

    /// Constructor with every provisioning seam injected. Tests use fakes;
    /// production callers normally use [`Self::with_launcher`].
    pub fn with_all_provisioners(
        options: ManagerOptions,
        launcher: Arc<dyn NodeLauncher>,
        vm_provisioner: Arc<dyn VmProvisioner>,
        evdev_provisioner: Option<Arc<dyn EvdevProvisioner>>,
        audio_provisioner: Option<Arc<dyn AudioSinkProvisioner>>,
    ) -> Self {
        let options = Arc::new(options);
        let (stop_tx, stop_rx) = watch::channel(false);
        let inner = Arc::new(ManagerInner {
            options: Arc::clone(&options),
            launcher,
            provisioner: Arc::new(AgentProvisioner::new(options.controller_url.clone())),
            vm_provisioner,
            evdev_provisioner,
            audio_provisioner,
            state: Mutex::new(SupervisorState {
                vms: HashMap::new(),
                ports: PortAllocator::new(options.base_port, options.node_http_base),
            }),
        });
        Self {
            options,
            inner,
            stop_tx,
            stop_rx,
        }
    }

    /// Reject configurations this supervisor cannot serve.
    ///
    /// The per-VM runtime is now real: validation checks the child binary,
    /// port bases, and restart schedule instead of refusing to manage VMs.
    pub fn validate(&self) -> Result<()> {
        if self.options.node_bin.as_os_str().is_empty() {
            anyhow::bail!("--node-bin must not be empty");
        }
        if self.options.base_port == 0 {
            anyhow::bail!("--base-port must be greater than zero");
        }
        if self.options.node_http_base == 0 {
            anyhow::bail!("--node-http-base must be greater than zero");
        }
        if self.options.restart_backoff.is_zero() {
            anyhow::bail!("--restart-backoff-ms must be greater than zero");
        }
        if self.options.restart_backoff_max < self.options.restart_backoff {
            anyhow::bail!(
                "--restart-backoff-max-ms must be greater than or equal to \
                 --restart-backoff-ms"
            );
        }
        if self.options.provision_config.restart_vm && !self.options.provision {
            anyhow::bail!("--provision-restart requires --provision");
        }
        if self.options.provision {
            self.options
                .provision_config
                .validate()
                .map_err(|error| anyhow::anyhow!("{error}"))?;
            let keyboard = self.options.provision_config.evdev_keyboard.is_some();
            let mouse = self.options.provision_config.evdev_mouse.is_some();
            if keyboard != mouse {
                anyhow::bail!(
                    "--evdev-kbd and --evdev-mouse must be provided together \
                     (input-linux is only injected when both are set)"
                );
            }
            if self.options.provision_config.evdev.enabled && (keyboard || mouse) {
                anyhow::bail!(
                    "--provision-evdev conflicts with --evdev-kbd/--evdev-mouse; \
                     choose device creation or operator-supplied paths"
                );
            }
        }
        if self.options.provision_config.evdev.enabled && !self.options.provision {
            anyhow::bail!("--provision-evdev requires --provision");
        }
        self.options
            .audio
            .validate()
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        Ok(())
    }

    /// Run discovery, supervision, and optional agent provisioning until
    /// signalled to stop. Shutdown stops every child and waits for it.
    pub async fn run(&self) -> Result<()> {
        self.validate()?;
        self.log_startup();

        self.inner.discover_and_sync().await;
        self.inner.supervise().await;

        let agent_task = if self.options.auto_agent {
            let inner = Arc::clone(&self.inner);
            let mut stop_rx = self.stop_rx.clone();
            Some(tokio::spawn(async move {
                inner.agent_loop(&mut stop_rx).await;
            }))
        } else {
            None
        };

        let tick = self.options.supervision_tick.max(Duration::from_millis(1));
        let mut ticker = tokio::time::interval(tick);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_discovery = Instant::now();
        let mut stop_rx = self.stop_rx.clone();

        loop {
            tokio::select! {
                _ = stop_rx.wait_for(|value| *value) => break,
                _ = ticker.tick() => {
                    if last_discovery.elapsed() >= self.options.discovery_interval {
                        self.inner.discover_and_sync().await;
                        last_discovery = Instant::now();
                    }
                    self.inner.supervise().await;
                }
            }
        }

        if let Some(task) = agent_task {
            task.abort();
            let _ = task.await;
        }

        info!("Shutting down: stopping every per-VM ozma-node child...");
        self.inner.shutdown().await;
        Ok(())
    }

    /// Signal the manager to stop. `run` then shuts down all children.
    pub async fn stop(&self) {
        let _ = self.stop_tx.send(true);
    }

    fn log_startup(&self) {
        info!(
            "Virtual Node Manager starting ({} mode)...",
            if self.options.auto_manage {
                "supervisor"
            } else {
                "discovery-only"
            }
        );
        info!(
            "  Controller: {}",
            if self.options.controller_url.is_empty() {
                "(auto-discover)"
            } else {
                &self.options.controller_url
            }
        );
        if self.options.auto_manage {
            info!("  Node binary: {}", self.options.node_bin.display());
            info!(
                "  Ports:       HID UDP from {}, HTTP from {}",
                self.options.base_port, self.options.node_http_base
            );
            info!(
                "  Restart:     {:?} initial, {:?} max",
                self.options.restart_backoff, self.options.restart_backoff_max
            );
        } else {
            info!("  Auto-manage: off (discovery-only; no ozma-node children)");
        }
        info!(
            "  Auto-agent:  {}",
            if self.options.auto_agent { "on" } else { "off" }
        );
        info!(
            "  Provision:   {}{}",
            if self.options.provision { "on" } else { "off" },
            if self.options.provision_config.restart_vm {
                " (restart VM after define)"
            } else {
                " (define only; restart required)"
            }
        );
        if self.options.provision {
            info!(
                "  QMP dir:     {}",
                self.options.provision_config.qmp_dir.display()
            );
            if let Some(value) = &self.options.provision_config.pulse_server {
                info!("  PulseAudio:  {}", value);
            }
            if let Some(value) = &self.options.provision_config.xdg_runtime_dir {
                info!("  XDG runtime: {}", value);
            }
            if let (Some(keyboard), Some(mouse)) = (
                &self.options.provision_config.evdev_keyboard,
                &self.options.provision_config.evdev_mouse,
            ) {
                info!(
                    "  evdev:       {} (keyboard), {} (mouse)",
                    keyboard.display(),
                    mouse.display()
                );
            }
            if self.options.provision_config.evdev.enabled {
                info!(
                    "  evdev create: on (markers {}, devices {})",
                    self.options.provision_config.evdev.marker_dir.display(),
                    self.options.provision_config.evdev.device_dir.display()
                );
            }
        }
        if self.options.audio.enabled {
            info!(
                "  Audio sink:  {} backend, prefix {:?}, track root {}",
                self.options.audio.backend,
                self.options.audio.sink_prefix,
                self.options
                    .audio
                    .track_root
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "(none)".to_owned())
            );
            info!(
                "  Audio type:  ozma-node has no --audio-type flag; a non-empty \
                 --audio-sink is advertised as pipewire"
            );
        } else {
            info!("  Audio sink:  off (no per-VM sink provisioning)");
        }
        if !self.options.exclude_patterns.is_empty() {
            info!(
                "  Exclude:     {}",
                self.options.exclude_patterns.join(", ")
            );
        }
    }
}

// ── ManagerInner — shared supervision state ────────────────────────────────

/// One VM's provisioning decision made outside the supervisor state lock.
enum ProvisionOutcome {
    Ready {
        qmp_socket: String,
        qmp_input_socket: Option<String>,
    },
    /// XML was defined but the VM has not been restarted, so no socket exists.
    NeedsRestart {
        qmp_socket: PathBuf,
        display_socket: PathBuf,
    },
    /// Discovery reported a configured socket path that does not exist yet.
    AwaitingSocket {
        qmp_socket: PathBuf,
    },
    /// Auto-management or provisioning is off and discovery found no socket.
    NoQmp,
    Failed(crate::provisioning::ProvisionError),
    DiscoveryOnly,
}

impl ManagerInner {
    fn is_excluded(&self, name: &str) -> bool {
        self.options
            .exclude_patterns
            .iter()
            .any(|pattern| glob_match(pattern, name))
    }

    /// Refresh the discovered-VM set, then start/stop children as needed.
    async fn discover_and_sync(&self) {
        match discovery::discover_vms(&self.options.qmp_dir).await {
            Ok(vms) => self.reconcile(vms).await,
            Err(error) => warn!("VM discovery error: {}", error),
        }
    }

    /// Create or reuse evdev devices. Returns `None` on any failure so XML
    /// injection falls back to no `input-linux` block.
    fn ensure_evdev(&self, vm_name: &str) -> Option<EvdevPaths> {
        let provisioner = self.evdev_provisioner.as_ref()?;
        match provisioner.ensure(vm_name) {
            Ok(paths) => {
                info!(
                    "VM {}: evdev devices ready (keyboard {}, mouse {})",
                    vm_name,
                    paths.keyboard.display(),
                    paths.mouse.display()
                );
                Some(paths)
            }
            Err(error) => {
                warn!(
                    "VM {}: evdev device provisioning failed: {}; continuing \
                     without input-linux devices",
                    vm_name, error
                );
                None
            }
        }
    }

    /// Create or reuse the per-VM audio sink. Returns `None` on failure so the
    /// node starts without `--audio-sink` rather than reporting a false sink.
    async fn ensure_audio_sink(&self, vm_name: &str) -> Option<ProvisionedSink> {
        let provisioner = self.audio_provisioner.as_ref()?;
        match provisioner.ensure(vm_name).await {
            Ok(sink) => {
                info!(
                    "VM {}: audio sink {} ({}) {}",
                    vm_name,
                    sink.name,
                    sink.monitor_target,
                    if sink.created { "created" } else { "reused" }
                );
                Some(sink)
            }
            Err(error) => {
                warn!(
                    "VM {}: per-VM audio sink provisioning failed: {}; \
                     continuing without audio",
                    vm_name, error
                );
                None
            }
        }
    }

    /// Release evdev devices and the sink (if this process created it) for a
    /// stopped or removed VM.
    async fn release_vm_resources(&self, vm_name: &str) {
        if let Some(provisioner) = &self.evdev_provisioner {
            provisioner.release(vm_name);
        }
        if let Some(provisioner) = &self.audio_provisioner {
            if let Err(error) = provisioner.release(vm_name).await {
                warn!(
                    "VM {}: could not remove per-VM audio sink: {}",
                    vm_name, error
                );
            }
        }
        self.vm_provisioner.release(vm_name).await;
    }

    /// Ask the provisioner for a QMP socket when the VM has none. This is the
    /// only place libvirt XML may be touched, and only when `--provision` is on.
    async fn provision_for_node(&self, vm: &VMInfo) -> ProvisionOutcome {
        if !self.options.auto_manage {
            return ProvisionOutcome::DiscoveryOnly;
        }

        let discovered = vm.qmp_path.trim();
        if !discovered.is_empty() {
            // When provisioning is enabled, do not feed a configured-but-not-yet
            // created socket into a child. A pre-restart libvirt XML can name a
            // path that only appears after the VM starts.
            if self.options.provision && !Path::new(discovered).exists() {
                return ProvisionOutcome::AwaitingSocket {
                    qmp_socket: PathBuf::from(discovered),
                };
            }
            let qmp_input_socket = vm
                .qmp_input_path
                .as_deref()
                .map(str::trim)
                .filter(|path| !path.is_empty())
                .filter(|path| !self.options.provision || Path::new(path).exists())
                .map(str::to_owned);
            return ProvisionOutcome::Ready {
                qmp_socket: discovered.to_owned(),
                qmp_input_socket,
            };
        }

        if !self.options.provision {
            return ProvisionOutcome::NoQmp;
        }

        let hints = ProvisionHints {
            evdev: self.ensure_evdev(&vm.name),
            pulse_sink: self.ensure_audio_sink(&vm.name).await.map(|sink| sink.name),
        };

        match self.vm_provisioner.ensure_with_hints(vm, &hints).await {
            Ok(provisioned) if provisioned.needs_restart => ProvisionOutcome::NeedsRestart {
                qmp_socket: provisioned.qmp_socket,
                display_socket: provisioned.display_socket,
            },
            Ok(provisioned) => ProvisionOutcome::Ready {
                qmp_socket: provisioned.qmp_socket.display().to_string(),
                qmp_input_socket: Some(provisioned.qmp_input_socket.display().to_string()),
            },
            Err(error) => ProvisionOutcome::Failed(error),
        }
    }

    /// Apply one discovery snapshot. New VMs are inserted, removed VMs are
    /// stopped, unchanged children are left alone, and VMs that gained a QMP
    /// socket since the last snapshot (for example after an operator restart)
    /// are promoted to supervised children.
    async fn reconcile(&self, mut vms: Vec<VMInfo>) {
        vms.sort_by(|a, b| a.name.cmp(&b.name));
        vms.dedup_by(|a, b| a.name == b.name);

        let current: HashSet<String> = vms.iter().map(|vm| vm.name.clone()).collect();
        let mut to_stop: Vec<NodeHandle> = Vec::new();
        let mut removed_names: Vec<String> = Vec::new();
        let mut candidates: Vec<(VMInfo, bool)> = Vec::new();

        {
            let mut state = self.state.lock().await;

            let removed: Vec<String> = state
                .vms
                .keys()
                .filter(|name| !current.contains(*name))
                .cloned()
                .collect();
            for name in removed {
                if let Some(mut managed) = state.vms.remove(&name) {
                    if let Some(node) = managed.node.take() {
                        if let Some(handle) = node.handle {
                            to_stop.push(handle);
                        }
                    }
                    state.ports.release(&name);
                    removed_names.push(name.clone());
                    info!("VM no longer running: {}", name);
                }
            }

            for vm in vms {
                if self.is_excluded(&vm.name) {
                    debug!("Excluded VM: {}", vm.name);
                    continue;
                }

                if let Some(managed) = state.vms.get_mut(&vm.name) {
                    // Keep the child and its spec; refresh discovery metadata
                    // so a newly visible QMP socket can promote a node later.
                    managed.vm = vm.clone();
                    let should_try = managed.node.is_none()
                        && self.options.auto_manage
                        && (self.options.provision || !vm.qmp_path.trim().is_empty());
                    if should_try {
                        candidates.push((vm, false));
                    }
                    continue;
                }

                info!(
                    "Discovered VM: {} (os={}, gpu_passthrough={}, qmp={})",
                    vm.name,
                    if vm.guest_os.is_empty() {
                        "unknown"
                    } else {
                        &vm.guest_os
                    },
                    vm.has_gpu_passthrough,
                    if vm.qmp_path.is_empty() {
                        "(none)"
                    } else {
                        &vm.qmp_path
                    },
                );
                candidates.push((vm, true));
            }

            debug!(
                "Discovery status: {} VM(s) tracked, {} child(ren) running",
                state.vms.len(),
                state
                    .vms
                    .values()
                    .filter(|managed| managed
                        .node
                        .as_ref()
                        .and_then(|node| node.handle.as_ref())
                        .is_some())
                    .count()
            );
        }

        for handle in to_stop {
            let name = handle.vm_name().to_owned();
            info!("Stopping ozma-node for removed VM {}", name);
            if let Err(error) = self.launcher.stop(handle).await {
                warn!("Failed to stop ozma-node for VM {}: {}", name, error);
            }
        }

        for name in removed_names {
            self.release_vm_resources(&name).await;
        }

        // Provisioning can take seconds (a requested restart waits for the
        // socket), so never hold the state lock across it.
        for (vm, is_new) in candidates {
            let outcome = self.provision_for_node(&vm).await;
            let mut state = self.state.lock().await;
            let existing = state.vms.get(&vm.name).map(|managed| {
                (
                    managed.node.is_some(),
                    managed.provision_restart_warned,
                    managed.provision_failure_warned,
                )
            });
            let node_present = existing.map(|(present, _, _)| present).unwrap_or(false);

            match outcome {
                ProvisionOutcome::Ready {
                    qmp_socket,
                    qmp_input_socket,
                } => {
                    let mut ready = vm;
                    ready.qmp_path = qmp_socket;
                    ready.qmp_input_path = qmp_input_socket;

                    let need_node = self.options.auto_manage && !node_present;
                    // The state lock is held by this match arm; drop it before
                    // awaiting the router so a slow `pactl` cannot stall
                    // discovery/status readers.
                    drop(state);
                    let audio_sink = if need_node {
                        self.ensure_audio_sink(&ready.name).await
                    } else {
                        None
                    };
                    let mut state = self.state.lock().await;

                    let node = if need_node {
                        match state.ports.allocate(&ready.name) {
                            Ok(ports) => Some(NodeState::new(build_node_spec(
                                &ready,
                                ports,
                                &self.options.node_bin,
                                &self.options.controller_url,
                                audio_sink.as_ref().map(|sink| sink.name.as_str()),
                            ))),
                            Err(error) => {
                                warn!("VM {}: cannot allocate per-VM ports: {}", ready.name, error);
                                None
                            }
                        }
                    } else {
                        None
                    };

                    if let Some(managed) = state.vms.get_mut(&ready.name) {
                        managed.vm = ready;
                        if managed.node.is_none() {
                            managed.node = node;
                        }
                        managed.provision_restart_warned = false;
                        managed.provision_failure_warned = false;
                    } else {
                        state.vms.insert(
                            ready.name.clone(),
                            ManagedVm {
                                vm: ready,
                                agent_provisioned: false,
                                agent_check_failures: 0,
                                node,
                                provision_restart_warned: false,
                                provision_failure_warned: false,
                            },
                        );
                    }
                }
                ProvisionOutcome::NeedsRestart {
                    qmp_socket,
                    display_socket,
                } => {
                    let first = !existing.map(|(_, warned, _)| warned).unwrap_or(false);
                    if first {
                        warn!(
                            "VM {}: ozma QMP/display defined ({} / {}); restart \
                             required before ozma-node can start. Not spawning a \
                             node. Restart the VM, or pass --provision-restart to \
                             allow the supervisor to destroy/start it.",
                            vm.name,
                            qmp_socket.display(),
                            display_socket.display()
                        );
                    } else {
                        debug!(
                            "VM {}: still waiting for a restart before QMP {} appears",
                            vm.name,
                            qmp_socket.display()
                        );
                    }

                    if let Some(managed) = state.vms.get_mut(&vm.name) {
                        managed.provision_restart_warned = true;
                    } else {
                        state.vms.insert(
                            vm.name.clone(),
                            ManagedVm {
                                vm,
                                agent_provisioned: false,
                                agent_check_failures: 0,
                                node: None,
                                provision_restart_warned: true,
                                provision_failure_warned: false,
                            },
                        );
                    }
                }
                ProvisionOutcome::AwaitingSocket { qmp_socket } => {
                    let first = !existing.map(|(_, warned, _)| warned).unwrap_or(false);
                    if first {
                        warn!(
                            "VM {}: QMP socket {} is configured but does not \
                             exist yet; restart the VM before ozma-node can \
                             start. Not spawning a node and not fabricating a \
                             replacement path.",
                            vm.name,
                            qmp_socket.display()
                        );
                    } else {
                        debug!(
                            "VM {}: still waiting for configured QMP socket {}",
                            vm.name,
                            qmp_socket.display()
                        );
                    }

                    if let Some(managed) = state.vms.get_mut(&vm.name) {
                        managed.provision_restart_warned = true;
                    } else {
                        state.vms.insert(
                            vm.name.clone(),
                            ManagedVm {
                                vm,
                                agent_provisioned: false,
                                agent_check_failures: 0,
                                node: None,
                                provision_restart_warned: true,
                                provision_failure_warned: false,
                            },
                        );
                    }
                }
                ProvisionOutcome::NoQmp => {
                    if is_new {
                        warn!(
                            "VM {} has no QMP socket; not starting ozma-node. \
                             Enable --provision to define one on a libvirt \
                             domain, or provide a real discovered socket. No \
                             socket path will be fabricated.",
                            vm.name
                        );
                    }
                    if !state.vms.contains_key(&vm.name) {
                        state.vms.insert(
                            vm.name.clone(),
                            ManagedVm {
                                vm,
                                agent_provisioned: false,
                                agent_check_failures: 0,
                                node: None,
                                provision_restart_warned: false,
                                provision_failure_warned: false,
                            },
                        );
                    }
                }
                ProvisionOutcome::Failed(error) => {
                    let first = !existing.map(|(_, _, warned)| warned).unwrap_or(false);
                    if first {
                        warn!(
                            "VM {}: provisioning failed: {}; not starting \
                             ozma-node",
                            vm.name, error
                        );
                    } else {
                        debug!("VM {}: provisioning still failing: {}", vm.name, error);
                    }

                    if let Some(managed) = state.vms.get_mut(&vm.name) {
                        managed.provision_failure_warned = true;
                    } else {
                        state.vms.insert(
                            vm.name.clone(),
                            ManagedVm {
                                vm,
                                agent_provisioned: false,
                                agent_check_failures: 0,
                                node: None,
                                provision_restart_warned: false,
                                provision_failure_warned: true,
                            },
                        );
                    }
                }
                ProvisionOutcome::DiscoveryOnly => {
                    if !state.vms.contains_key(&vm.name) {
                        state.vms.insert(
                            vm.name.clone(),
                            ManagedVm {
                                vm,
                                agent_provisioned: false,
                                agent_check_failures: 0,
                                node: None,
                                provision_restart_warned: false,
                                provision_failure_warned: false,
                            },
                        );
                    }
                }
            }
        }
    }

    /// Poll children, schedule exited/failed children for restart, and launch
    /// due restarts.
    async fn supervise(&self) {
        let mut exited = Vec::new();

        {
            let mut state = self.state.lock().await;
            let mut names: Vec<String> = state.vms.keys().cloned().collect();
            names.sort();

            for name in names {
                let Some(managed) = state.vms.get_mut(&name) else {
                    continue;
                };
                let Some(node) = managed.node.as_mut() else {
                    continue;
                };
                let Some(handle) = node.handle.as_mut() else {
                    continue;
                };

                match handle.try_wait() {
                    Ok(Some(status)) => {
                        let handle = node.handle.take().expect("handle exists");
                        let uptime = node.started_at.map(|at| at.elapsed()).unwrap_or_default();
                        exited.push((name, handle, Some(status), uptime));
                    }
                    Ok(None) => {}
                    Err(error) => {
                        let handle = node.handle.take().expect("handle exists");
                        let uptime = node.started_at.map(|at| at.elapsed()).unwrap_or_default();
                        warn!(
                            "Failed to poll ozma-node for VM {}: {}; treating it as exited",
                            name, error
                        );
                        exited.push((name, handle, None, uptime));
                    }
                }
            }
        }

        for (name, handle, status, uptime) in exited {
            if let Err(error) = self.launcher.stop(handle).await {
                debug!("Reaping exited ozma-node for VM {}: {}", name, error);
            }

            let mut state = self.state.lock().await;
            let Some(managed) = state.vms.get_mut(&name) else {
                continue;
            };
            let Some(node) = managed.node.as_mut() else {
                continue;
            };

            // A child that ran long enough before failing starts over at the
            // initial delay; a crash loop keeps doubling.
            if uptime >= HEALTHY_UPTIME {
                node.attempts = 0;
            }
            let delay = restart_delay(
                self.options.restart_backoff,
                self.options.restart_backoff_max,
                node.attempts,
            );
            node.attempts = node.attempts.saturating_add(1);
            node.next_restart = Some(Instant::now() + delay);
            node.started_at = None;

            match status {
                Some(status) => warn!(
                    "ozma-node for VM {} exited ({}); restarting in {} ms",
                    name,
                    status,
                    delay.as_millis()
                ),
                None => warn!(
                    "ozma-node for VM {} is no longer pollable; restarting in {} ms",
                    name,
                    delay.as_millis()
                ),
            }
        }

        let due = {
            let mut state = self.state.lock().await;
            let now = Instant::now();
            let mut names: Vec<String> = state.vms.keys().cloned().collect();
            names.sort();

            let mut due = Vec::new();
            for name in names {
                let Some(managed) = state.vms.get_mut(&name) else {
                    continue;
                };
                let Some(node) = managed.node.as_mut() else {
                    continue;
                };
                if node.handle.is_some() || node.spawning {
                    continue;
                }
                if node.next_restart.is_some_and(|at| at <= now) {
                    node.spawning = true;
                    node.next_restart = None;
                    due.push((name, node.spec.clone()));
                }
            }
            due
        };

        for (name, spec) in due {
            let argv = spec.argv();
            match self.launcher.spawn(&spec).await {
                Ok(handle) => {
                    let pid = handle.pid();
                    let mut state = self.state.lock().await;
                    if let Some(managed) = state.vms.get_mut(&name) {
                        if let Some(node) = managed.node.as_mut() {
                            node.handle = Some(handle);
                            node.started_at = Some(Instant::now());
                            node.spawning = false;
                            info!(
                                "Started ozma-node for VM {} (pid={}, hid_udp={}, http={})",
                                name,
                                pid.map(|pid| pid.to_string())
                                    .unwrap_or_else(|| "unknown".to_owned()),
                                spec.hid_udp_port,
                                spec.http_port
                            );
                            debug!("  argv: {}", argv.join(" "));
                            continue;
                        }
                    }
                    drop(state);
                    if let Err(error) = self.launcher.stop(handle).await {
                        debug!(
                            "Stopping ozma-node for VM {} after it disappeared: {}",
                            name, error
                        );
                    }
                }
                Err(error) => {
                    let mut state = self.state.lock().await;
                    if let Some(managed) = state.vms.get_mut(&name) {
                        if let Some(node) = managed.node.as_mut() {
                            node.spawning = false;
                            let delay = restart_delay(
                                self.options.restart_backoff,
                                self.options.restart_backoff_max,
                                node.attempts,
                            );
                            node.attempts = node.attempts.saturating_add(1);
                            node.next_restart = Some(Instant::now() + delay);
                            warn!(
                                "Failed to start ozma-node for VM {}: {}; retrying in {} ms",
                                name,
                                error,
                                delay.as_millis()
                            );
                            debug!("  argv: {}", argv.join(" "));
                        }
                    }
                }
            }
        }
    }

    /// Stop every tracked child and wait for it. Used on shutdown.
    async fn shutdown(&self) {
        let (names, handles) = {
            let mut state = self.state.lock().await;
            let mut names: Vec<String> = state.vms.keys().cloned().collect();
            names.sort();

            let mut handles = Vec::new();
            for name in &names {
                if let Some(mut managed) = state.vms.remove(name) {
                    state.ports.release(name);
                    if let Some(node) = managed.node.take() {
                        if let Some(handle) = node.handle {
                            handles.push(handle);
                        }
                    }
                }
            }
            state.ports.clear();
            (names, handles)
        };

        for handle in handles {
            let name = handle.vm_name().to_owned();
            info!("Stopping ozma-node for VM {}", name);
            if let Err(error) = self.launcher.stop(handle).await {
                warn!("Failed to stop ozma-node for VM {}: {}", name, error);
            }
        }
        for name in names {
            self.release_vm_resources(&name).await;
        }
        debug!("All per-VM ozma-node children stopped");
    }

    async fn agent_loop(&self, stop_rx: &mut watch::Receiver<bool>) {
        // Let discovery settle before the first attempt.
        tokio::time::sleep(Duration::from_secs(15)).await;
        if *stop_rx.borrow() {
            return;
        }

        let mut interval = tokio::time::interval(Duration::from_secs(30));
        loop {
            interval.tick().await;
            if *stop_rx.borrow() {
                break;
            }

            let snapshot: Vec<(String, VMInfo, bool, u32)> = {
                let state = self.state.lock().await;
                state
                    .vms
                    .iter()
                    .map(|(name, managed)| {
                        (
                            name.clone(),
                            managed.vm.clone(),
                            managed.agent_provisioned,
                            managed.agent_check_failures,
                        )
                    })
                    .collect()
            };

            for (name, vm, already_provisioned, failures) in snapshot {
                if already_provisioned {
                    continue;
                }

                // GPU passthrough VMs: the agent is the only display path, so
                // never give up. Other VMs stop after five failures.
                let max_failures: Option<u32> = if vm.has_gpu_passthrough {
                    None
                } else {
                    Some(5)
                };
                if max_failures.is_some_and(|max| failures > max) {
                    continue;
                }

                if self.provisioner.provision(&vm).await {
                    if let Some(managed) = self.state.lock().await.vms.get_mut(&name) {
                        managed.agent_provisioned = true;
                    }
                    info!("Agent provisioned in {}", name);
                } else if let Some(managed) = self.state.lock().await.vms.get_mut(&name) {
                    managed.agent_check_failures += 1;
                    if vm.has_gpu_passthrough {
                        debug!(
                            "GPU passthrough VM {}: agent not yet installed \
                             (attempt {}) — will keep retrying",
                            name, managed.agent_check_failures
                        );
                    }
                }
            }
        }
    }
}

// ── Glob matching ──────────────────────────────────────────────────────────

/// Minimal glob matcher supporting `*` (any sequence) and `?` (one char).
fn glob_match(pattern: &str, text: &str) -> bool {
    let pat: Vec<char> = pattern.chars().collect();
    let txt: Vec<char> = text.chars().collect();
    glob_match_inner(&pat, &txt)
}

fn glob_match_inner(pat: &[char], txt: &[char]) -> bool {
    match (pat.first(), txt.first()) {
        (None, None) => true,
        (Some(&'*'), _) => {
            glob_match_inner(&pat[1..], txt)
                || (!txt.is_empty() && glob_match_inner(pat, &txt[1..]))
        }
        (Some(&'?'), Some(_)) => glob_match_inner(&pat[1..], &txt[1..]),
        (Some(p), Some(t)) if p == t => glob_match_inner(&pat[1..], &txt[1..]),
        _ => false,
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio_provisioning::{AudioBackend, FakeAudioSinkProvisioner};
    use crate::evdev_provisioning::{EvdevProvisionConfig, FakeEvdevProvisioner};
    use crate::provisioning::{ProvisionError, ProvisionedVm, VmProvisioner};
    use crate::supervisor::testing::FakeNodeLauncher;
    use crate::supervisor::NodePorts;

    fn options(auto_manage: bool) -> ManagerOptions {
        ManagerOptions {
            auto_manage,
            node_bin: PathBuf::from("/usr/bin/ozma-node"),
            base_port: 7400,
            node_http_base: 7500,
            restart_backoff: Duration::from_millis(2),
            restart_backoff_max: Duration::from_millis(8),
            ..ManagerOptions::default()
        }
    }

    fn vm(name: &str, qmp: &str) -> VMInfo {
        let mut vm = VMInfo::new(name);
        vm.qmp_path = qmp.to_owned();
        vm
    }

    fn manager(options: ManagerOptions, launcher: Arc<FakeNodeLauncher>) -> VirtualNodeManager {
        VirtualNodeManager::with_launcher(options, launcher)
    }

    fn manager_with_provisioner(
        options: ManagerOptions,
        launcher: Arc<FakeNodeLauncher>,
        provisioner: Arc<FakeVmProvisioner>,
    ) -> VirtualNodeManager {
        VirtualNodeManager::with_launcher_and_provisioner(options, launcher, provisioner)
    }

    fn manager_with_device_provisioners(
        options: ManagerOptions,
        launcher: Arc<FakeNodeLauncher>,
        vm_provisioner: Arc<FakeVmProvisioner>,
        evdev: Option<Arc<FakeEvdevProvisioner>>,
        audio: Option<Arc<FakeAudioSinkProvisioner>>,
    ) -> VirtualNodeManager {
        VirtualNodeManager::with_all_provisioners(
            options,
            launcher,
            vm_provisioner,
            evdev.map(|provisioner| provisioner as Arc<dyn EvdevProvisioner>),
            audio.map(|provisioner| provisioner as Arc<dyn AudioSinkProvisioner>),
        )
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum FakeOutcome {
        Ready,
        NeedsRestart,
        Failed,
    }

    struct FakeVmProvisioner {
        calls: std::sync::Mutex<Vec<String>>,
        hints: std::sync::Mutex<Vec<ProvisionHints>>,
        outcome: std::sync::Mutex<FakeOutcome>,
    }

    impl FakeVmProvisioner {
        fn new(outcome: FakeOutcome) -> Self {
            Self {
                calls: std::sync::Mutex::new(Vec::new()),
                hints: std::sync::Mutex::new(Vec::new()),
                outcome: std::sync::Mutex::new(outcome),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        fn hints(&self) -> Vec<ProvisionHints> {
            self.hints.lock().unwrap().clone()
        }

        fn set_outcome(&self, outcome: FakeOutcome) {
            *self.outcome.lock().unwrap() = outcome;
        }

        async fn provision(&self, vm: &VMInfo) -> Result<ProvisionedVm, ProvisionError> {
            self.calls.lock().unwrap().push(vm.name.clone());
            let outcome = *self.outcome.lock().unwrap();
            if outcome == FakeOutcome::Failed {
                return Err(ProvisionError::DumpXml {
                    domain: vm.name.clone(),
                    detail: "fake dumpxml failure".to_owned(),
                });
            }
            let qmp = PathBuf::from(format!("/run/ozma/qmp/{}.sock", vm.name));
            Ok(ProvisionedVm {
                qmp_socket: qmp,
                qmp_input_socket: PathBuf::from(format!("/run/ozma/qmp/{}-input.sock", vm.name)),
                display_socket: PathBuf::from(format!("/run/ozma/qmp/{}-display.sock", vm.name)),
                changed: true,
                needs_restart: outcome == FakeOutcome::NeedsRestart,
            })
        }
    }

    #[async_trait::async_trait]
    impl VmProvisioner for FakeVmProvisioner {
        async fn ensure(&self, vm: &VMInfo) -> Result<ProvisionedVm, ProvisionError> {
            self.provision(vm).await
        }

        async fn ensure_with_hints(
            &self,
            vm: &VMInfo,
            hints: &ProvisionHints,
        ) -> Result<ProvisionedVm, ProvisionError> {
            self.hints.lock().unwrap().push(hints.clone());
            self.provision(vm).await
        }
    }

    async fn tracked(mgr: &VirtualNodeManager, name: &str) -> bool {
        mgr.inner.state.lock().await.vms.contains_key(name)
    }

    #[tokio::test]
    async fn starts_one_vm_with_exact_argv_and_ports() {
        let launcher = Arc::new(FakeNodeLauncher::new());
        let mgr = manager(options(true), Arc::clone(&launcher));

        mgr.inner
            .reconcile(vec![vm("vm-a", "/run/ozma/a.qmp")])
            .await;
        mgr.inner.supervise().await;

        let spawns = launcher.spawns();
        assert_eq!(spawns.len(), 1);
        assert_eq!(
            spawns[0].argv(),
            vec![
                "/usr/bin/ozma-node",
                "--profile",
                "vm",
                "--name",
                "vm-a",
                "--qmp-socket",
                "/run/ozma/a.qmp",
                "--hid-udp-port",
                "7400",
                "--http-port",
                "7500",
                "--capture-provider",
                "qmp",
                "--input-provider",
                "qmp",
                "--power-provider",
                "qmp",
            ]
        );
        assert_eq!((spawns[0].hid_udp_port, spawns[0].http_port), (7400, 7500));
        assert!(launcher.active("vm-a"));
    }

    #[tokio::test]
    async fn two_vms_get_distinct_ports_and_sorted_argv() {
        let launcher = Arc::new(FakeNodeLauncher::new());
        let mgr = manager(options(true), Arc::clone(&launcher));

        mgr.inner
            .reconcile(vec![
                vm("vm-b", "/run/ozma/b.qmp"),
                vm("vm-a", "/run/ozma/a.qmp"),
            ])
            .await;
        mgr.inner.supervise().await;

        let spawns = launcher.spawns();
        assert_eq!(spawns.len(), 2);
        assert_eq!(spawns[0].vm_name, "vm-a");
        assert_eq!(spawns[1].vm_name, "vm-b");
        assert_eq!((spawns[0].hid_udp_port, spawns[0].http_port), (7400, 7500));
        assert_eq!((spawns[1].hid_udp_port, spawns[1].http_port), (7401, 7501));
        assert_ne!(spawns[0].hid_udp_port, spawns[1].hid_udp_port);
    }

    #[tokio::test]
    async fn passes_register_url_when_controller_is_configured() {
        let launcher = Arc::new(FakeNodeLauncher::new());
        let mgr = manager(
            ManagerOptions {
                controller_url: "http://10.0.0.1:7380".to_owned(),
                ..options(true)
            },
            Arc::clone(&launcher),
        );

        mgr.inner
            .reconcile(vec![vm("vm-a", "/run/ozma/a.qmp")])
            .await;
        mgr.inner.supervise().await;

        let argv = launcher.spawns()[0].argv();
        let register = argv
            .iter()
            .position(|arg| arg == "--register-url")
            .expect("register url flag");
        assert_eq!(argv[register + 1], "http://10.0.0.1:7380");
    }

    #[tokio::test]
    async fn passes_qmp_input_socket_only_when_exposed() {
        let launcher = Arc::new(FakeNodeLauncher::new());
        let mgr = manager(options(true), Arc::clone(&launcher));

        let mut vm_a = vm("vm-a", "/run/ozma/a.qmp");
        vm_a.qmp_input_path = Some("/run/ozma/a-input.qmp".to_owned());
        mgr.inner.reconcile(vec![vm_a]).await;
        mgr.inner.supervise().await;

        let argv = launcher.spawns()[0].argv();
        let input_socket = argv
            .iter()
            .position(|arg| arg == "--qmp-input-socket")
            .expect("input socket flag");
        assert_eq!(argv[input_socket + 1], "/run/ozma/a-input.qmp");
        assert!(argv.iter().any(|arg| arg == "--input-provider"));
    }

    #[tokio::test]
    async fn vm_without_qmp_is_tracked_but_not_spawned() {
        let launcher = Arc::new(FakeNodeLauncher::new());
        let mgr = manager(options(true), Arc::clone(&launcher));

        mgr.inner.reconcile(vec![vm("no-qmp", "")]).await;
        mgr.inner.supervise().await;

        assert!(launcher.spawns().is_empty());
        assert!(tracked(&mgr, "no-qmp").await);
    }

    #[tokio::test]
    async fn provision_on_spawns_with_the_provisioned_qmp_path() {
        let launcher = Arc::new(FakeNodeLauncher::new());
        let provisioner = Arc::new(FakeVmProvisioner::new(FakeOutcome::Ready));
        let mgr = manager_with_provisioner(
            ManagerOptions {
                provision: true,
                ..options(true)
            },
            Arc::clone(&launcher),
            Arc::clone(&provisioner),
        );

        mgr.inner.reconcile(vec![vm("vm-a", "")]).await;
        mgr.inner.supervise().await;

        assert_eq!(provisioner.calls(), vec!["vm-a".to_owned()]);
        let spawns = launcher.spawns();
        assert_eq!(spawns.len(), 1);
        assert_eq!(spawns[0].qmp_socket, "/run/ozma/qmp/vm-a.sock");
        let argv = spawns[0].argv();
        let qmp = argv
            .iter()
            .position(|arg| arg == "--qmp-socket")
            .expect("qmp socket flag");
        assert_eq!(argv[qmp + 1], "/run/ozma/qmp/vm-a.sock");
        let input = argv
            .iter()
            .position(|arg| arg == "--qmp-input-socket")
            .expect("qmp input socket flag");
        assert_eq!(argv[input + 1], "/run/ozma/qmp/vm-a-input.sock");
        assert!(launcher.active("vm-a"));
    }

    #[tokio::test]
    async fn provision_off_skips_without_calling_the_provisioner() {
        let launcher = Arc::new(FakeNodeLauncher::new());
        let provisioner = Arc::new(FakeVmProvisioner::new(FakeOutcome::Ready));
        let mgr = manager_with_provisioner(
            options(true),
            Arc::clone(&launcher),
            Arc::clone(&provisioner),
        );

        mgr.inner.reconcile(vec![vm("vm-a", "")]).await;
        mgr.inner.supervise().await;

        assert!(provisioner.calls().is_empty());
        assert!(launcher.spawns().is_empty());
        assert!(tracked(&mgr, "vm-a").await);
    }

    #[tokio::test]
    async fn needs_restart_skips_until_discovery_reports_the_socket() {
        let launcher = Arc::new(FakeNodeLauncher::new());
        let provisioner = Arc::new(FakeVmProvisioner::new(FakeOutcome::NeedsRestart));
        let mgr = manager_with_provisioner(
            ManagerOptions {
                provision: true,
                ..options(true)
            },
            Arc::clone(&launcher),
            Arc::clone(&provisioner),
        );

        mgr.inner.reconcile(vec![vm("vm-a", "")]).await;
        mgr.inner.supervise().await;

        assert_eq!(provisioner.calls(), vec!["vm-a".to_owned()]);
        assert!(launcher.spawns().is_empty());
        assert!(tracked(&mgr, "vm-a").await);
        assert!(mgr.inner.state.lock().await.vms["vm-a"].provision_restart_warned);

        // The operator restarted the VM; discovery now sees a real socket.
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("vm-a.sock");
        std::fs::write(&socket, b"").unwrap();
        mgr.inner
            .reconcile(vec![vm("vm-a", &socket.to_string_lossy())])
            .await;
        mgr.inner.supervise().await;

        assert_eq!(
            provisioner.calls(),
            vec!["vm-a".to_owned()],
            "a discovered socket must not trigger another ensure()"
        );
        assert_eq!(launcher.spawns().len(), 1);
        assert_eq!(launcher.spawns()[0].qmp_socket, socket.to_string_lossy());
    }

    #[tokio::test]
    async fn configured_but_absent_socket_does_not_spawn_when_provisioning() {
        let launcher = Arc::new(FakeNodeLauncher::new());
        let provisioner = Arc::new(FakeVmProvisioner::new(FakeOutcome::Ready));
        let mgr = manager_with_provisioner(
            ManagerOptions {
                provision: true,
                ..options(true)
            },
            Arc::clone(&launcher),
            Arc::clone(&provisioner),
        );

        mgr.inner
            .reconcile(vec![vm("vm-a", "/run/ozma/qmp/vm-a.sock")])
            .await;
        mgr.inner.supervise().await;

        assert!(launcher.spawns().is_empty());
        assert!(provisioner.calls().is_empty());
        assert!(tracked(&mgr, "vm-a").await);
    }

    #[tokio::test]
    async fn failed_provisioning_does_not_spawn_and_retries() {
        let launcher = Arc::new(FakeNodeLauncher::new());
        let provisioner = Arc::new(FakeVmProvisioner::new(FakeOutcome::Failed));
        let mgr = manager_with_provisioner(
            ManagerOptions {
                provision: true,
                ..options(true)
            },
            Arc::clone(&launcher),
            Arc::clone(&provisioner),
        );

        mgr.inner.reconcile(vec![vm("vm-a", "")]).await;
        mgr.inner.supervise().await;

        assert!(launcher.spawns().is_empty());
        assert!(tracked(&mgr, "vm-a").await);
        assert!(mgr.inner.state.lock().await.vms["vm-a"].provision_failure_warned);

        mgr.inner.reconcile(vec![vm("vm-a", "")]).await;
        mgr.inner.supervise().await;
        assert_eq!(provisioner.calls().len(), 2, "tracked VM is retried");
        assert!(launcher.spawns().is_empty());
    }

    #[tokio::test]
    async fn evdev_hints_flow_into_provisioning_and_are_released_on_removal() {
        let launcher = Arc::new(FakeNodeLauncher::new());
        let provisioner = Arc::new(FakeVmProvisioner::new(FakeOutcome::Ready));
        let evdev = Arc::new(FakeEvdevProvisioner::new("/run/ozma/fake-evdev"));
        let mgr = manager_with_device_provisioners(
            ManagerOptions {
                provision: true,
                ..options(true)
            },
            Arc::clone(&launcher),
            Arc::clone(&provisioner),
            Some(Arc::clone(&evdev)),
            None,
        );

        mgr.inner.reconcile(vec![vm("vm-a", "")]).await;
        mgr.inner.supervise().await;

        assert_eq!(evdev.ensured(), vec!["vm-a".to_owned()]);
        let hints = provisioner.hints();
        assert_eq!(hints.len(), 1);
        let paths = hints[0].evdev.as_ref().expect("evdev hint");
        assert_eq!(
            paths.keyboard,
            PathBuf::from("/run/ozma/fake-evdev/vm-a-kbd")
        );
        assert_eq!(
            paths.mouse,
            PathBuf::from("/run/ozma/fake-evdev/vm-a-mouse")
        );
        assert_eq!(launcher.spawns().len(), 1);

        mgr.inner.reconcile(Vec::new()).await;
        assert_eq!(evdev.released(), vec!["vm-a".to_owned()]);
    }

    #[tokio::test]
    async fn evdev_failure_falls_back_and_still_starts_the_node() {
        let launcher = Arc::new(FakeNodeLauncher::new());
        let provisioner = Arc::new(FakeVmProvisioner::new(FakeOutcome::Ready));
        let evdev = Arc::new(FakeEvdevProvisioner::new("/run/ozma/fake-evdev"));
        evdev.set_fail(true);
        let mgr = manager_with_device_provisioners(
            ManagerOptions {
                provision: true,
                ..options(true)
            },
            Arc::clone(&launcher),
            Arc::clone(&provisioner),
            Some(evdev),
            None,
        );

        mgr.inner.reconcile(vec![vm("vm-a", "")]).await;
        mgr.inner.supervise().await;

        assert!(provisioner.hints()[0].evdev.is_none());
        assert_eq!(launcher.spawns().len(), 1);
    }

    #[tokio::test]
    async fn audio_sink_is_created_before_spawn_and_removed_on_removal() {
        let launcher = Arc::new(FakeNodeLauncher::new());
        let provisioner = Arc::new(FakeVmProvisioner::new(FakeOutcome::Ready));
        let audio = Arc::new(FakeAudioSinkProvisioner::new("ozma-"));
        let mgr = manager_with_device_provisioners(
            ManagerOptions {
                provision: true,
                audio: AudioProvisionConfig {
                    enabled: true,
                    backend: AudioBackend::PipeWire,
                    ..AudioProvisionConfig::default()
                },
                ..options(true)
            },
            Arc::clone(&launcher),
            Arc::clone(&provisioner),
            None,
            Some(Arc::clone(&audio)),
        );

        mgr.inner.reconcile(vec![vm("vm-a", "")]).await;
        assert!(
            audio.ensured().contains(&"vm-a".to_owned()),
            "sink must be created during reconcile"
        );
        assert!(
            launcher.spawns().is_empty(),
            "sink creation precedes the supervised spawn"
        );
        mgr.inner.supervise().await;

        assert!(audio.ensured().contains(&"vm-a".to_owned()));
        let spawns = launcher.spawns();
        assert_eq!(spawns.len(), 1);
        assert_eq!(spawns[0].audio_sink.as_deref(), Some("ozma-vm-a"));
        let argv = spawns[0].argv();
        let sink = argv
            .iter()
            .position(|arg| arg == "--audio-sink")
            .expect("audio sink flag");
        assert_eq!(argv[sink + 1], "ozma-vm-a");
        assert_eq!(
            provisioner.hints()[0].pulse_sink.as_deref(),
            Some("ozma-vm-a")
        );

        mgr.inner.reconcile(Vec::new()).await;
        assert_eq!(audio.released(), vec!["vm-a".to_owned()]);
    }

    #[tokio::test]
    async fn audio_failure_falls_back_without_the_sink_flag() {
        let launcher = Arc::new(FakeNodeLauncher::new());
        let provisioner = Arc::new(FakeVmProvisioner::new(FakeOutcome::Ready));
        let audio = Arc::new(FakeAudioSinkProvisioner::new("ozma-"));
        audio.set_fail(true);
        let mgr = manager_with_device_provisioners(
            ManagerOptions {
                provision: true,
                audio: AudioProvisionConfig {
                    enabled: true,
                    backend: AudioBackend::PulseAudio,
                    ..AudioProvisionConfig::default()
                },
                ..options(true)
            },
            Arc::clone(&launcher),
            Arc::clone(&provisioner),
            None,
            Some(audio),
        );

        mgr.inner.reconcile(vec![vm("vm-a", "")]).await;
        mgr.inner.supervise().await;

        let spawns = launcher.spawns();
        assert_eq!(spawns.len(), 1);
        assert!(spawns[0].audio_sink.is_none());
        assert!(!spawns[0].argv().iter().any(|arg| arg == "--audio-sink"));
        assert!(provisioner.hints()[0].pulse_sink.is_none());
    }

    #[tokio::test]
    async fn shutdown_releases_evdev_and_audio_resources() {
        let launcher = Arc::new(FakeNodeLauncher::new());
        let provisioner = Arc::new(FakeVmProvisioner::new(FakeOutcome::Ready));
        let evdev = Arc::new(FakeEvdevProvisioner::new("/run/ozma/fake-evdev"));
        let audio = Arc::new(FakeAudioSinkProvisioner::new("ozma-"));
        let mgr = manager_with_device_provisioners(
            ManagerOptions {
                provision: true,
                audio: AudioProvisionConfig {
                    enabled: true,
                    backend: AudioBackend::PipeWire,
                    ..AudioProvisionConfig::default()
                },
                ..options(true)
            },
            Arc::clone(&launcher),
            Arc::clone(&provisioner),
            Some(Arc::clone(&evdev)),
            Some(Arc::clone(&audio)),
        );

        mgr.inner.reconcile(vec![vm("vm-a", "")]).await;
        mgr.inner.supervise().await;
        mgr.inner.shutdown().await;

        assert_eq!(evdev.released(), vec!["vm-a".to_owned()]);
        assert_eq!(audio.released(), vec!["vm-a".to_owned()]);
    }

    #[tokio::test]
    async fn restart_success_promotes_a_tracked_vm_to_a_child() {
        let launcher = Arc::new(FakeNodeLauncher::new());
        let provisioner = Arc::new(FakeVmProvisioner::new(FakeOutcome::NeedsRestart));
        let mgr = manager_with_provisioner(
            ManagerOptions {
                provision: true,
                ..options(true)
            },
            Arc::clone(&launcher),
            Arc::clone(&provisioner),
        );

        mgr.inner.reconcile(vec![vm("vm-a", "")]).await;
        mgr.inner.supervise().await;
        assert!(launcher.spawns().is_empty());

        // The provisioner restarted the VM and now reports a usable socket.
        provisioner.set_outcome(FakeOutcome::Ready);
        mgr.inner.reconcile(vec![vm("vm-a", "")]).await;
        mgr.inner.supervise().await;

        assert_eq!(provisioner.calls().len(), 2);
        assert_eq!(launcher.spawns().len(), 1);
        assert!(launcher.active("vm-a"));
    }

    #[tokio::test]
    async fn no_auto_manage_never_provisions() {
        let launcher = Arc::new(FakeNodeLauncher::new());
        let provisioner = Arc::new(FakeVmProvisioner::new(FakeOutcome::Ready));
        let mgr = manager_with_provisioner(
            ManagerOptions {
                provision: true,
                ..options(false)
            },
            Arc::clone(&launcher),
            Arc::clone(&provisioner),
        );

        mgr.inner.reconcile(vec![vm("vm-a", "")]).await;
        mgr.inner.supervise().await;

        assert!(provisioner.calls().is_empty());
        assert!(launcher.spawns().is_empty());
        assert!(tracked(&mgr, "vm-a").await);
    }

    #[tokio::test]
    async fn unchanged_vm_is_not_restarted() {
        let launcher = Arc::new(FakeNodeLauncher::new());
        let mgr = manager(options(true), Arc::clone(&launcher));

        mgr.inner
            .reconcile(vec![
                vm("vm-a", "/run/ozma/a.qmp"),
                vm("vm-b", "/run/ozma/b.qmp"),
            ])
            .await;
        mgr.inner.supervise().await;
        assert_eq!(launcher.spawns().len(), 2);

        mgr.inner
            .reconcile(vec![
                vm("vm-a", "/run/ozma/a.qmp"),
                vm("vm-b", "/run/ozma/b.qmp"),
            ])
            .await;
        mgr.inner.supervise().await;

        assert_eq!(launcher.spawns().len(), 2);
        assert!(launcher.stopped().is_empty());
    }

    #[tokio::test]
    async fn removed_vm_is_stopped_and_forgotten() {
        let launcher = Arc::new(FakeNodeLauncher::new());
        let mgr = manager(options(true), Arc::clone(&launcher));

        mgr.inner
            .reconcile(vec![
                vm("vm-a", "/run/ozma/a.qmp"),
                vm("vm-b", "/run/ozma/b.qmp"),
            ])
            .await;
        mgr.inner.supervise().await;

        mgr.inner
            .reconcile(vec![vm("vm-a", "/run/ozma/a.qmp")])
            .await;

        assert_eq!(launcher.stopped(), vec!["vm-b".to_owned()]);
        assert!(tracked(&mgr, "vm-a").await);
        assert!(!tracked(&mgr, "vm-b").await);
        assert!(!launcher.active("vm-b"));
    }

    #[tokio::test]
    async fn failed_child_restarts_after_backoff() {
        let launcher = Arc::new(FakeNodeLauncher::new());
        let mgr = manager(options(true), Arc::clone(&launcher));

        mgr.inner
            .reconcile(vec![vm("vm-a", "/run/ozma/a.qmp")])
            .await;
        mgr.inner.supervise().await;
        assert_eq!(launcher.spawns().len(), 1);

        launcher.exit("vm-a");
        mgr.inner.supervise().await;
        assert_eq!(launcher.spawns().len(), 1, "restart waits for backoff");

        tokio::time::sleep(Duration::from_millis(10)).await;
        mgr.inner.supervise().await;

        assert_eq!(launcher.spawns().len(), 2);
        assert!(launcher.active("vm-a"));
    }

    #[tokio::test]
    async fn spawn_failure_is_retried_with_backoff() {
        let launcher = Arc::new(FakeNodeLauncher::new());
        launcher.fail_spawn("vm-a");
        let mgr = manager(options(true), Arc::clone(&launcher));

        mgr.inner
            .reconcile(vec![vm("vm-a", "/run/ozma/a.qmp")])
            .await;
        mgr.inner.supervise().await;

        assert!(launcher.spawns().is_empty());
        let state = mgr.inner.state.lock().await;
        let node = state.vms["vm-a"].node.as_ref().unwrap();
        assert_eq!(node.attempts, 1);
        assert!(node.next_restart.is_some());
    }

    #[tokio::test]
    async fn shutdown_stops_every_child() {
        let launcher = Arc::new(FakeNodeLauncher::new());
        let mgr = manager(options(true), Arc::clone(&launcher));

        mgr.inner
            .reconcile(vec![
                vm("vm-a", "/run/ozma/a.qmp"),
                vm("vm-b", "/run/ozma/b.qmp"),
            ])
            .await;
        mgr.inner.supervise().await;
        assert_eq!(launcher.spawns().len(), 2);

        mgr.inner.shutdown().await;

        assert_eq!(
            launcher.stopped(),
            vec!["vm-a".to_owned(), "vm-b".to_owned()]
        );
        assert!(!launcher.active("vm-a"));
        assert!(!launcher.active("vm-b"));
        assert!(mgr.inner.state.lock().await.vms.is_empty());
    }

    #[tokio::test]
    async fn no_auto_manage_validates_and_never_spawns() {
        let launcher = Arc::new(FakeNodeLauncher::new());
        let mgr = manager(options(false), Arc::clone(&launcher));

        assert!(mgr.validate().is_ok());
        mgr.inner
            .reconcile(vec![vm("vm-a", "/run/ozma/a.qmp")])
            .await;
        mgr.inner.supervise().await;

        assert!(launcher.spawns().is_empty());
        assert!(tracked(&mgr, "vm-a").await);
        let state = mgr.inner.state.lock().await;
        assert!(state.vms["vm-a"].node.is_none());
    }

    #[test]
    fn validate_rejects_invalid_supervisor_settings() {
        let launcher = Arc::new(FakeNodeLauncher::new());

        let empty_bin = manager(
            ManagerOptions {
                node_bin: PathBuf::new(),
                ..options(true)
            },
            Arc::clone(&launcher),
        );
        assert!(empty_bin.validate().is_err());

        let zero_port = manager(
            ManagerOptions {
                base_port: 0,
                ..options(true)
            },
            Arc::clone(&launcher),
        );
        assert!(zero_port.validate().is_err());

        let reversed_backoff = manager(
            ManagerOptions {
                restart_backoff: Duration::from_millis(20),
                restart_backoff_max: Duration::from_millis(10),
                ..options(true)
            },
            launcher,
        );
        assert!(reversed_backoff.validate().is_err());
    }

    #[test]
    fn validate_rejects_restart_without_provision_and_relative_qmp_dir() {
        let launcher = Arc::new(FakeNodeLauncher::new());

        let restart_without_provision = manager(
            ManagerOptions {
                provision: false,
                provision_config: ProvisionConfig {
                    restart_vm: true,
                    ..ProvisionConfig::default()
                },
                ..options(true)
            },
            Arc::clone(&launcher),
        );
        assert!(restart_without_provision.validate().is_err());

        let relative_qmp = manager(
            ManagerOptions {
                provision: true,
                provision_config: ProvisionConfig {
                    qmp_dir: PathBuf::from("relative/qmp"),
                    ..ProvisionConfig::default()
                },
                ..options(true)
            },
            Arc::clone(&launcher),
        );
        assert!(relative_qmp.validate().is_err());

        let one_evdev = manager(
            ManagerOptions {
                provision: true,
                provision_config: ProvisionConfig {
                    evdev_keyboard: Some(PathBuf::from("/dev/input/event5")),
                    ..ProvisionConfig::default()
                },
                ..options(true)
            },
            launcher,
        );
        assert!(one_evdev.validate().is_err());
    }

    #[test]
    fn validate_rejects_new_device_and_audio_combinations() {
        let launcher = Arc::new(FakeNodeLauncher::new());

        let evdev_without_provision = manager(
            ManagerOptions {
                provision: false,
                provision_config: ProvisionConfig {
                    evdev: EvdevProvisionConfig {
                        enabled: true,
                        ..EvdevProvisionConfig::default()
                    },
                    ..ProvisionConfig::default()
                },
                ..options(true)
            },
            Arc::clone(&launcher),
        );
        assert!(evdev_without_provision.validate().is_err());

        let conflicting_evdev = manager(
            ManagerOptions {
                provision: true,
                provision_config: ProvisionConfig {
                    evdev_keyboard: Some(PathBuf::from("/dev/input/event5")),
                    evdev_mouse: Some(PathBuf::from("/dev/input/event6")),
                    evdev: EvdevProvisionConfig {
                        enabled: true,
                        ..EvdevProvisionConfig::default()
                    },
                    ..ProvisionConfig::default()
                },
                ..options(true)
            },
            Arc::clone(&launcher),
        );
        assert!(conflicting_evdev.validate().is_err());

        let audio_without_backend = manager(
            ManagerOptions {
                audio: AudioProvisionConfig {
                    enabled: true,
                    ..AudioProvisionConfig::default()
                },
                ..options(true)
            },
            Arc::clone(&launcher),
        );
        assert!(audio_without_backend.validate().is_err());

        let backend_without_enable = manager(
            ManagerOptions {
                audio: AudioProvisionConfig {
                    backend: AudioBackend::PipeWire,
                    ..AudioProvisionConfig::default()
                },
                ..options(true)
            },
            launcher,
        );
        assert!(backend_without_enable.validate().is_err());
    }

    #[test]
    fn exclusion_patterns_still_work() {
        assert!(glob_match("template-*", "template-win11"));
        assert!(glob_match("infra-?", "infra-1"));
        assert!(!glob_match("template-*", "desktop-1"));
    }

    #[test]
    fn port_allocator_pairs_match_spec_expectations() {
        let mut allocator = PortAllocator::new(7400, 7500);
        assert_eq!(
            allocator.allocate("vm-a").unwrap(),
            NodePorts {
                hid_udp: 7400,
                http: 7500
            }
        );
    }
}
