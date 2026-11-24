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
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;
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
use crate::status::{ChildInputStatus, ChildPowerStatus, ChildStatus, SupervisorStatus, VmStatus};
use crate::supervisor::{
    build_node_spec, restart_delay, NodeExitStatus, NodeHandle, NodeLauncher, NodeSpec,
    PortAllocator, ProcessNodeLauncher, DEFAULT_BASE_PORT, DEFAULT_NODE_BIN,
    DEFAULT_NODE_HTTP_BASE, DEFAULT_RESTART_BACKOFF_MAX_MS, DEFAULT_RESTART_BACKOFF_MS,
    HEALTHY_UPTIME,
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
    /// How often each running child's `GET /api/v1/connection` is polled.
    /// `Duration::ZERO` disables child aggregation.
    pub child_poll_interval: Duration,
    /// Per-child HTTP timeout for status polling.
    pub child_poll_timeout: Duration,
    /// Optional bearer token sent to child APIs when polling. Never serialized
    /// into status or metrics.
    pub child_api_token: String,
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
            child_poll_interval: Duration::ZERO,
            child_poll_timeout: Duration::from_millis(2_000),
            child_api_token: String::new(),
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
    /// Why there is no child yet. `None` while a node is tracked.
    skip: Option<VmSkipState>,
    /// Most recent provisioning/socket error, reported by `/status`.
    last_error: Option<String>,
    /// evdev devices this supervisor created for the VM, when provisioning
    /// ran. `None` leaves `--evdev-*` out of the child argv.
    evdev: Option<EvdevPaths>,
    /// Per-VM audio sink passed to the child. Kept so status still reports it
    /// after the node spec has been built.
    audio_sink: Option<String>,
    /// Suppress repeated "defined; restart required" warnings.
    provision_restart_warned: bool,
    /// Suppress repeated provisioning-failure warnings.
    provision_failure_warned: bool,
}

impl ManagedVm {
    fn new(vm: VMInfo, node: Option<NodeState>) -> Self {
        Self {
            vm,
            agent_provisioned: false,
            agent_check_failures: 0,
            node,
            skip: None,
            last_error: None,
            evdev: None,
            audio_sink: None,
            provision_restart_warned: false,
            provision_failure_warned: false,
        }
    }
}

/// Why a tracked VM has no supervised child.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VmSkipState {
    DiscoveryOnly,
    NoQmp,
    NeedsRestart,
    AwaitingSocket,
    ProvisionFailed,
}

impl VmSkipState {
    fn as_str(self) -> &'static str {
        match self {
            Self::DiscoveryOnly => "discovery-only",
            Self::NoQmp => "skipped",
            Self::NeedsRestart => "needs-restart",
            Self::AwaitingSocket => "awaiting-socket",
            Self::ProvisionFailed => "failed",
        }
    }
}

struct NodeState {
    spec: NodeSpec,
    handle: Option<NodeHandle>,
    /// Consecutive failures, used for the exponential restart delay.
    attempts: u32,
    /// Successful (re)starts after the initial spawn, for status/metrics.
    restarts: u32,
    next_restart: Option<Instant>,
    started_at: Option<Instant>,
    /// Last observed child exit, or `None` before the first exit.
    last_exit: Option<NodeExitStatus>,
    /// Most recent spawn/exit error.
    last_error: Option<String>,
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
            restarts: 0,
            next_restart: Some(Instant::now()),
            started_at: None,
            last_exit: None,
            last_error: None,
            spawning: false,
        }
    }
}

struct SupervisorState {
    vms: HashMap<String, ManagedVm>,
    ports: PortAllocator,
    /// Every VM name in the last discovery snapshot, before exclusion.
    discovered: HashSet<String>,
    /// VM names filtered out by `--exclude` in the last snapshot.
    excluded: HashSet<String>,
}

/// One child's cached connection summary plus poll freshness metadata.
#[derive(Clone, Debug, Default)]
struct ChildPollRecord {
    reachable: bool,
    last_attempt: Option<Instant>,
    last_success: Option<Instant>,
    last_seen_unix_ms: Option<u64>,
    summary: Option<ChildConnectionSummary>,
    last_error: Option<String>,
}

/// The subset of a child `/api/v1/connection` document kept in memory.
#[derive(Clone, Debug, Default)]
struct ChildConnectionSummary {
    node_id: Option<String>,
    profile: Option<String>,
    api_port: Option<u16>,
    hls: Option<String>,
    webrtc: Option<String>,
    audio: Option<String>,
    power: Option<ChildPowerStatus>,
    input: Option<ChildInputStatus>,
}

/// Typed, bounded view of a child connection document. Unknown fields are
/// ignored so a newer node cannot bloat the supervisor's cache.
#[derive(Debug, Deserialize)]
struct ChildConnectionDoc {
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    node_id: Option<String>,
    #[serde(default)]
    profile: Option<String>,
    #[serde(default)]
    api_port: Option<u16>,
    #[serde(default)]
    hls: Option<String>,
    #[serde(default)]
    webrtc: Option<String>,
    #[serde(default)]
    audio: Option<String>,
    #[serde(default)]
    power: Option<Value>,
    #[serde(default)]
    input: Option<ChildInputDoc>,
}

#[derive(Debug, Deserialize)]
struct ChildInputDoc {
    #[serde(default)]
    sink_kind: Option<String>,
    #[serde(default)]
    available: bool,
    #[serde(default)]
    injecting: bool,
}

/// Hard cap on a child connection response; the document is normally tiny.
const CHILD_CONNECTION_MAX_BYTES: usize = 256 * 1024;
/// Cap copied free-form strings so one malformed child cannot inflate `/status`.
const CHILD_FIELD_MAX_CHARS: usize = 256;

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
    /// Child connection summaries. Kept separate from `state` so a slow child
    /// poll never blocks a status read, and a status read never waits on HTTP.
    child_polls: Mutex<HashMap<String, ChildPollRecord>>,
    child_client: reqwest::Client,
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
                discovered: HashSet::new(),
                excluded: HashSet::new(),
            }),
            child_polls: Mutex::new(HashMap::new()),
            child_client: reqwest::Client::builder()
                .no_proxy()
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
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
        if !self.options.child_poll_interval.is_zero() && self.options.child_poll_timeout.is_zero()
        {
            anyhow::bail!(
                "--child-poll-timeout-ms must be greater than zero when \
                 --child-poll-interval-ms enables child polling"
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

        // Child API polling runs independently so a slow or hung child never
        // delays discovery, restart handling, or status reads.
        let child_poll_task = if self.options.child_poll_interval.is_zero() {
            None
        } else {
            let inner = Arc::clone(&self.inner);
            let mut stop_rx = self.stop_rx.clone();
            Some(tokio::spawn(async move {
                inner.child_poll_loop(&mut stop_rx).await;
            }))
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
        if let Some(task) = child_poll_task {
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

    /// A receiver that becomes `true` when shutdown starts. The status server
    /// uses it to stop accepting requests alongside the children.
    pub fn subscribe_shutdown(&self) -> watch::Receiver<bool> {
        self.stop_rx.clone()
    }

    /// Snapshot the aggregated multi-VM status.
    ///
    /// This reads in-memory supervisor state and the child poll cache: it never
    /// performs an HTTP request, opens a QMP socket, or runs a provisioning
    /// command while answering. A child poll in flight cannot block a status
    /// read because the poll cache lock is only held to copy completed results.
    pub async fn status(&self) -> SupervisorStatus {
        let (mut vms, discovered, next_ports) = {
            let state = self.inner.state.lock().await;
            let now = Instant::now();
            let mut vms = Vec::with_capacity(state.vms.len() + state.excluded.len());

            for managed in state.vms.values() {
                vms.push(vm_status(
                    managed,
                    self.options.auto_agent,
                    self.options.provision_config.evdev.enabled,
                    now,
                ));
            }
            for name in &state.excluded {
                vms.push(VmStatus::excluded(name.clone()));
            }
            vms.sort_by(|a, b| a.name.cmp(&b.name));

            (vms, state.discovered.len(), state.ports.next_ports())
        };

        let children = self.inner.child_status_map(Instant::now()).await;
        if !children.is_empty() {
            for vm in &mut vms {
                if let Some(child) = children.get(&vm.name) {
                    let mut child = child.clone();
                    // A child that is no longer spawned cannot be reachable now,
                    // even if the last poll succeeded before it exited.
                    if !vm.spawned {
                        child.reachable = false;
                    }
                    vm.child = Some(child);
                }
            }
        }

        let mut status = SupervisorStatus::new(discovered, next_ports, vms);
        status.child_poll_interval_ms = duration_millis(self.options.child_poll_interval);
        status.child_poll_timeout_ms = duration_millis(self.options.child_poll_timeout);
        status
    }

    /// Test-only entry point: apply one discovery snapshot and poll children.
    #[cfg(test)]
    pub(crate) async fn sync_for_test(&self, vms: Vec<VMInfo>) {
        self.inner.reconcile(vms).await;
        self.inner.supervise().await;
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
        if self.options.child_poll_interval.is_zero() {
            info!("  Child poll:  off (no per-VM API aggregation)");
        } else {
            info!(
                "  Child poll:  every {:?}, timeout {:?} (bearer token: {})",
                self.options.child_poll_interval,
                self.options.child_poll_timeout,
                if self.options.child_api_token.is_empty() {
                    "none"
                } else {
                    "configured"
                }
            );
        }
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
            info!(
                "  Audio state: {} (ownership across supervisor restarts)",
                self.options.audio.state_path.display()
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
        /// evdev devices created/reused for this VM, when provisioning ran.
        evdev: Option<EvdevPaths>,
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

    /// Poll every running child's `/api/v1/connection` on the configured
    /// interval until shutdown. Polling runs outside the supervision loop and
    /// outside the status state lock.
    async fn child_poll_loop(&self, stop: &mut watch::Receiver<bool>) {
        let interval = self
            .options
            .child_poll_interval
            .max(Duration::from_millis(1));
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            ticker.tick().await;
            if *stop.borrow() {
                break;
            }
            self.poll_children().await;
        }
    }

    /// Fetch one connection document per running child, concurrently, with a
    /// per-request timeout. Results are applied under the cache lock without
    /// holding it across any HTTP await.
    async fn poll_children(&self) {
        let (targets, tracked) = {
            let state = self.state.lock().await;
            let tracked: HashSet<String> = state.vms.keys().cloned().collect();
            let targets: Vec<(String, u16)> = state
                .vms
                .values()
                .filter_map(|managed| {
                    managed
                        .node
                        .as_ref()
                        .filter(|node| node.handle.is_some())
                        .map(|node| (managed.vm.name.clone(), node.spec.http_port))
                })
                .collect();
            (targets, tracked)
        };

        // Bound the cache to VMs the supervisor still tracks. A stopped child's
        // last summary is kept (and reported stale) until the VM disappears.
        self.child_polls
            .lock()
            .await
            .retain(|name, _| tracked.contains(name));

        if targets.is_empty() {
            return;
        }

        let mut tasks = tokio::task::JoinSet::new();
        for (name, port) in targets {
            let client = self.child_client.clone();
            let token = self.options.child_api_token.clone();
            let timeout = self.options.child_poll_timeout;
            tasks.spawn(async move {
                let result = fetch_child_connection(&client, &token, port, timeout).await;
                (name, result)
            });
        }

        while let Some(joined) = tasks.join_next().await {
            let Ok((name, result)) = joined else {
                continue;
            };
            let now = Instant::now();
            let mut polls = self.child_polls.lock().await;
            let record = polls.entry(name).or_default();
            record.last_attempt = Some(now);
            match result {
                Ok(summary) => {
                    record.reachable = true;
                    record.last_success = Some(now);
                    record.last_seen_unix_ms = Some(unix_millis());
                    record.summary = Some(summary);
                    record.last_error = None;
                }
                Err(error) => {
                    record.reachable = false;
                    record.last_error = Some(error);
                }
            }
        }
    }

    /// Snapshot the child poll cache with ages and staleness derived at read
    /// time. The lock is held only for this in-memory copy.
    async fn child_status_map(&self, now: Instant) -> HashMap<String, ChildStatus> {
        let polls = self.child_polls.lock().await;
        let stale_after = self
            .options
            .child_poll_interval
            .saturating_mul(2)
            .saturating_add(self.options.child_poll_timeout);

        polls
            .iter()
            .filter_map(|(name, record)| {
                // A record exists only after at least one attempt; skip empty
                // placeholders rather than reporting a fabricated summary.
                record.last_attempt?;
                let summary = record.summary.as_ref();
                let stale = match record.last_success {
                    None => true,
                    Some(at) => now.saturating_duration_since(at) > stale_after,
                };
                Some((
                    name.clone(),
                    ChildStatus {
                        reachable: record.reachable,
                        node_id: summary.and_then(|s| s.node_id.clone()),
                        profile: summary.and_then(|s| s.profile.clone()),
                        api_port: summary.and_then(|s| s.api_port),
                        hls: summary.and_then(|s| s.hls.clone()),
                        webrtc: summary.and_then(|s| s.webrtc.clone()),
                        audio: summary.and_then(|s| s.audio.clone()),
                        power: summary.and_then(|s| s.power.clone()),
                        input: summary.and_then(|s| s.input.clone()),
                        last_poll_age_ms: record
                            .last_attempt
                            .map(|at| duration_millis(now.saturating_duration_since(at))),
                        last_seen_age_ms: record
                            .last_success
                            .map(|at| duration_millis(now.saturating_duration_since(at))),
                        last_seen_unix_ms: record.last_seen_unix_ms,
                        stale,
                        last_error: record.last_error.clone(),
                    },
                ))
            })
            .collect()
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
                evdev: None,
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
                evdev: hints.evdev.clone(),
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

            // Record the full discovered set, including excluded VMs, so
            // totals and exclusion are visible without tracking child state.
            state.discovered = current.clone();
            state.excluded = vms
                .iter()
                .filter(|vm| self.is_excluded(&vm.name))
                .map(|vm| vm.name.clone())
                .collect();

            let removed: Vec<String> = state
                .vms
                .keys()
                .filter(|name| !current.contains(*name) || self.is_excluded(name))
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
                    info!("VM no longer running or is excluded: {}", name);
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
                    evdev,
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
                    let audio_sink_name = audio_sink.as_ref().map(|sink| sink.name.clone());
                    let mut state = self.state.lock().await;

                    let node = if need_node {
                        match state.ports.allocate(&ready.name) {
                            Ok(ports) => Some(NodeState::new(build_node_spec(
                                &ready,
                                ports,
                                &self.options.node_bin,
                                &self.options.controller_url,
                                audio_sink_name.as_deref(),
                            ))),
                            Err(error) => {
                                warn!("VM {}: cannot allocate per-VM ports: {}", ready.name, error);
                                None
                            }
                        }
                    } else {
                        None
                    };

                    let name = ready.name.clone();
                    if let Some(managed) = state.vms.get_mut(&name) {
                        managed.vm = ready;
                        if managed.node.is_none() {
                            managed.node = node;
                        }
                        if need_node {
                            managed.evdev = evdev;
                            managed.audio_sink = audio_sink_name;
                        }
                        managed.skip = None;
                        managed.last_error = None;
                        managed.provision_restart_warned = false;
                        managed.provision_failure_warned = false;
                    } else {
                        let mut managed = ManagedVm::new(ready, node);
                        managed.evdev = evdev;
                        managed.audio_sink = audio_sink_name;
                        state.vms.insert(name, managed);
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

                    let reason = format!(
                        "ozma QMP/display defined ({} / {}); VM restart required",
                        qmp_socket.display(),
                        display_socket.display()
                    );
                    if let Some(managed) = state.vms.get_mut(&vm.name) {
                        managed.skip = Some(VmSkipState::NeedsRestart);
                        managed.last_error = Some(reason);
                        managed.provision_restart_warned = true;
                    } else {
                        let mut managed = ManagedVm::new(vm, None);
                        managed.skip = Some(VmSkipState::NeedsRestart);
                        managed.last_error = Some(reason);
                        managed.provision_restart_warned = true;
                        state.vms.insert(managed.vm.name.clone(), managed);
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

                    let reason = format!(
                        "QMP socket {} is configured but does not exist yet",
                        qmp_socket.display()
                    );
                    if let Some(managed) = state.vms.get_mut(&vm.name) {
                        managed.skip = Some(VmSkipState::AwaitingSocket);
                        managed.last_error = Some(reason);
                        managed.provision_restart_warned = true;
                    } else {
                        let mut managed = ManagedVm::new(vm, None);
                        managed.skip = Some(VmSkipState::AwaitingSocket);
                        managed.last_error = Some(reason);
                        managed.provision_restart_warned = true;
                        state.vms.insert(managed.vm.name.clone(), managed);
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
                    if let Some(managed) = state.vms.get_mut(&vm.name) {
                        managed.skip = Some(VmSkipState::NoQmp);
                    } else if !state.vms.contains_key(&vm.name) {
                        let mut managed = ManagedVm::new(vm, None);
                        managed.skip = Some(VmSkipState::NoQmp);
                        state.vms.insert(managed.vm.name.clone(), managed);
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
                        managed.skip = Some(VmSkipState::ProvisionFailed);
                        managed.last_error = Some(error.to_string());
                        managed.provision_failure_warned = true;
                    } else {
                        let mut managed = ManagedVm::new(vm, None);
                        managed.skip = Some(VmSkipState::ProvisionFailed);
                        managed.last_error = Some(error.to_string());
                        managed.provision_failure_warned = true;
                        state.vms.insert(managed.vm.name.clone(), managed);
                    }
                }
                ProvisionOutcome::DiscoveryOnly => {
                    if !state.vms.contains_key(&vm.name) {
                        let mut managed = ManagedVm::new(vm, None);
                        managed.skip = Some(VmSkipState::DiscoveryOnly);
                        state.vms.insert(managed.vm.name.clone(), managed);
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
            node.last_exit = status;
            node.last_error = Some(match status {
                Some(status) => format!("child exited ({status})"),
                None => "child is no longer pollable".to_owned(),
            });

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
                            if node.attempts > 0 {
                                node.restarts = node.restarts.saturating_add(1);
                            }
                            node.handle = Some(handle);
                            node.started_at = Some(Instant::now());
                            node.spawning = false;
                            node.last_error = None;
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
                            node.last_error = Some(error.to_string());
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

// ── Status assembly ────────────────────────────────────────────────────────

/// Fetch and validate one child's connection document.
///
/// The request targets loopback and the allocated child HTTP port. The optional
/// bearer token is only sent in the `Authorization` header; it is never copied
/// into the result, status document, or an error string.
async fn fetch_child_connection(
    client: &reqwest::Client,
    token: &str,
    port: u16,
    timeout: Duration,
) -> Result<ChildConnectionSummary, String> {
    let url = format!("http://127.0.0.1:{port}/api/v1/connection");
    let mut request = client.get(&url).timeout(timeout);
    if !token.is_empty() {
        request = request.bearer_auth(token);
    }

    let response = request.send().await.map_err(|error| {
        if error.is_timeout() {
            format!(
                "child /api/v1/connection timed out after {} ms",
                timeout.as_millis()
            )
        } else {
            format!("child /api/v1/connection failed: {error}")
        }
    })?;

    let status = response.status();
    if !status.is_success() {
        return Err(format!("child /api/v1/connection returned HTTP {status}"));
    }

    let bytes = response
        .bytes()
        .await
        .map_err(|error| format!("child /api/v1/connection body failed: {error}"))?;
    if bytes.len() > CHILD_CONNECTION_MAX_BYTES {
        return Err(format!(
            "child /api/v1/connection body exceeded {} bytes",
            CHILD_CONNECTION_MAX_BYTES
        ));
    }

    let doc: ChildConnectionDoc = serde_json::from_slice(&bytes)
        .map_err(|error| format!("child /api/v1/connection returned invalid JSON: {error}"))?;
    if !doc.ok {
        return Err("child /api/v1/connection reported ok=false".to_owned());
    }

    Ok(ChildConnectionSummary {
        node_id: bounded(doc.node_id),
        profile: bounded(doc.profile),
        api_port: Some(doc.api_port.unwrap_or(port)),
        hls: bounded(doc.hls),
        webrtc: bounded(doc.webrtc),
        audio: bounded(doc.audio),
        power: doc.power.as_ref().and_then(power_status),
        input: doc.input.map(|input| ChildInputStatus {
            kind: bounded(input.sink_kind).unwrap_or_else(|| "unknown".to_owned()),
            available: input.available,
            injecting: input.injecting,
        }),
    })
}

fn power_status(value: &Value) -> Option<ChildPowerStatus> {
    match value {
        Value::String(state) => Some(ChildPowerStatus {
            state: bounded(Some(state.clone())).unwrap_or_default(),
            available: false,
        }),
        Value::Object(object) => Some(ChildPowerStatus {
            state: object
                .get("status")
                .and_then(Value::as_str)
                .and_then(|state| bounded(Some(state.to_owned())))
                .unwrap_or_else(|| "unknown".to_owned()),
            available: object
                .get("available")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }),
        _ => None,
    }
}

/// Truncate a free-form child string to keep `/status` bounded.
fn bounded(value: Option<String>) -> Option<String> {
    value.map(|value| value.chars().take(CHILD_FIELD_MAX_CHARS).collect())
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(duration_millis)
        .unwrap_or(0)
}

fn duration_millis(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

/// Build one VM's status entry from live supervisor state.
fn vm_status(managed: &ManagedVm, auto_agent: bool, evdev_enabled: bool, now: Instant) -> VmStatus {
    let node = managed.node.as_ref();

    let state = match node {
        Some(node) if node.spawning => "starting",
        Some(node) if node.handle.is_some() => "running",
        Some(node) if node.next_restart.is_some() => "backoff",
        Some(_) => "stopped",
        None => managed.skip.unwrap_or(VmSkipState::NoQmp).as_str(),
    };
    let spawned = node.and_then(|node| node.handle.as_ref()).is_some();
    let restart_in_ms = node
        .and_then(|node| node.next_restart)
        .map(|at| at.saturating_duration_since(now).as_millis() as u64);
    let last_error = node
        .and_then(|node| node.last_error.clone())
        .or_else(|| managed.last_error.clone());
    let last_exit = node
        .and_then(|node| node.last_exit)
        .map(|status| status.to_string());

    let (hid_udp_port, http_port, qmp_socket, audio_sink) = match node {
        Some(node) => (
            Some(node.spec.hid_udp_port),
            Some(node.spec.http_port),
            node.spec.qmp_socket.clone(),
            managed
                .audio_sink
                .clone()
                .or_else(|| node.spec.audio_sink.clone()),
        ),
        None => (
            None,
            None,
            managed.vm.qmp_path.clone(),
            managed.audio_sink.clone(),
        ),
    };

    let agent_status = if !auto_agent {
        "disabled"
    } else if managed.agent_provisioned {
        "provisioned"
    } else if managed.vm.has_gpu_passthrough {
        // GPU passthrough VMs keep retrying forever: the agent is the only
        // display path.
        "retrying"
    } else if managed.agent_check_failures > 5 {
        "failed"
    } else if managed.agent_check_failures == 0 {
        "pending"
    } else {
        "retrying"
    };

    let (evdev_status, evdev_keyboard, evdev_mouse) = match &managed.evdev {
        Some(paths) => (
            "ready".to_owned(),
            Some(paths.keyboard.display().to_string()),
            Some(paths.mouse.display().to_string()),
        ),
        None if evdev_enabled => ("unavailable".to_owned(), None, None),
        None => ("not-configured".to_owned(), None, None),
    };

    VmStatus {
        name: managed.vm.name.clone(),
        state: state.to_owned(),
        qmp_socket,
        hid_udp_port,
        http_port,
        child_pid: node
            .and_then(|node| node.handle.as_ref())
            .and_then(NodeHandle::pid),
        spawned,
        skipped: !spawned,
        needs_restart: state == VmSkipState::NeedsRestart.as_str(),
        excluded: false,
        restart_count: node.map(|node| node.restarts).unwrap_or(0),
        restart_in_ms,
        last_exit,
        last_error,
        evdev_status,
        evdev_keyboard,
        evdev_mouse,
        audio_sink,
        agent_status: agent_status.to_owned(),
        agent_checks: managed.agent_check_failures,
        child: None,
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

    #[derive(Clone)]
    struct FakeChildState {
        body: serde_json::Value,
        delay: Duration,
        authorization: Arc<std::sync::Mutex<Option<String>>>,
    }

    async fn fake_child_handler(
        axum::extract::State(state): axum::extract::State<FakeChildState>,
        headers: axum::http::HeaderMap,
    ) -> axum::Json<serde_json::Value> {
        if let Some(value) = headers.get(axum::http::header::AUTHORIZATION) {
            if let Ok(value) = value.to_str() {
                *state.authorization.lock().unwrap() = Some(value.to_owned());
            }
        }
        if !state.delay.is_zero() {
            tokio::time::sleep(state.delay).await;
        }
        axum::Json(state.body.clone())
    }

    /// Start a fake child node API on an ephemeral loopback port.
    async fn spawn_fake_child(
        body: serde_json::Value,
        delay: Duration,
    ) -> (
        u16,
        Arc<std::sync::Mutex<Option<String>>>,
        tokio::task::JoinHandle<()>,
    ) {
        use axum::routing::get;

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind fake child");
        let port = listener.local_addr().expect("fake child addr").port();
        let authorization = Arc::new(std::sync::Mutex::new(None));
        let state = FakeChildState {
            body,
            delay,
            authorization: Arc::clone(&authorization),
        };
        let app = axum::Router::new()
            .route("/api/v1/connection", get(fake_child_handler))
            .with_state(state);
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (port, authorization, task)
    }

    fn child_doc() -> serde_json::Value {
        serde_json::json!({
            "ok": true,
            "node_id": "vm-a",
            "profile": "vm",
            "api_port": 7700,
            "quic_port": 0,
            "hid_udp_port": 7600,
            "sink_kind": "qmp",
            "hls": "running",
            "webrtc": "not-configured",
            "audio": "running",
            "power": {"status": "running", "available": true},
            "input": {"sink_kind": "qmp", "available": true, "injecting": true},
            "api_token": "child-secret-that-must-not-leak"
        })
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

    #[test]
    fn port_allocator_reports_the_next_pair_and_bounds() {
        let mut allocator = PortAllocator::new(7400, 7500);
        assert_eq!(
            allocator.next_ports(),
            Some(NodePorts {
                hid_udp: 7400,
                http: 7500,
            })
        );
        allocator.allocate("vm-a").unwrap();
        assert_eq!(
            allocator.next_ports(),
            Some(NodePorts {
                hid_udp: 7401,
                http: 7501,
            })
        );

        let mut exhausted = PortAllocator::new(u16::MAX, u16::MAX);
        // Overlapping bases with no room for a distinct pair: reject instead
        // of wrapping around.
        assert!(exhausted.allocate("vm-a").is_err());
        assert_eq!(exhausted.next_ports(), None);
    }

    #[tokio::test]
    async fn status_aggregates_started_skipped_and_excluded_vms() {
        let launcher = Arc::new(FakeNodeLauncher::new());
        let mgr = manager(
            ManagerOptions {
                exclude_patterns: vec!["template-*".to_owned()],
                ..options(true)
            },
            Arc::clone(&launcher),
        );

        mgr.inner
            .reconcile(vec![
                vm("vm-a", "/run/ozma/a.qmp"),
                vm("vm-b", ""),
                vm("template-win11", "/run/ozma/template.qmp"),
            ])
            .await;
        mgr.inner.supervise().await;

        let status = mgr.status().await;
        assert!(status.ok);
        assert_eq!(status.profile, "hypervisor");
        assert_eq!(status.node_id, crate::status::SUPERVISOR_NODE_ID);
        assert_eq!(status.supervisor.discovered, 3);
        assert_eq!(status.supervisor.tracked, 2);
        assert_eq!(status.supervisor.running, 1);
        assert_eq!(status.supervisor.failed, 0);
        assert_eq!(status.supervisor.skipped, 1);
        assert_eq!(status.supervisor.excluded, 1);
        assert_eq!(status.supervisor.next_hid_port, Some(7401));
        assert_eq!(status.supervisor.next_http_port, Some(7501));

        let vm_a = status.vms.iter().find(|vm| vm.name == "vm-a").unwrap();
        assert_eq!(vm_a.state, "running");
        assert!(vm_a.spawned);
        assert!(!vm_a.skipped);
        assert_eq!(vm_a.hid_udp_port, Some(7400));
        assert_eq!(vm_a.http_port, Some(7500));
        assert_eq!(vm_a.child_pid, Some(1));
        assert_eq!(vm_a.qmp_socket, "/run/ozma/a.qmp");
        assert_eq!(vm_a.agent_status, "pending");

        let vm_b = status.vms.iter().find(|vm| vm.name == "vm-b").unwrap();
        assert_eq!(vm_b.state, "skipped");
        assert!(vm_b.skipped);
        assert_eq!(vm_b.hid_udp_port, None);
        assert_eq!(vm_b.qmp_socket, "");

        let excluded = status
            .vms
            .iter()
            .find(|vm| vm.name == "template-win11")
            .unwrap();
        assert_eq!(excluded.state, "excluded");
        assert!(excluded.excluded);
        assert!(excluded.skipped);
        assert_eq!(excluded.hid_udp_port, None);
    }

    #[tokio::test]
    async fn child_poll_aggregates_connection_state_without_leaking_tokens() {
        let (port, authorization, server) = spawn_fake_child(child_doc(), Duration::ZERO).await;
        let launcher = Arc::new(FakeNodeLauncher::new());
        let mut options = options(true);
        options.node_http_base = port;
        options.child_poll_interval = Duration::from_millis(50);
        options.child_poll_timeout = Duration::from_millis(250);
        options.child_api_token = "supervisor-token".to_owned();
        let mgr = manager(options, launcher);

        mgr.inner
            .reconcile(vec![vm("vm-a", "/run/ozma/a.qmp")])
            .await;
        mgr.inner.supervise().await;
        mgr.inner.poll_children().await;

        let status = mgr.status().await;
        assert_eq!(status.child_poll_interval_ms, 50);
        assert_eq!(status.child_poll_timeout_ms, 250);
        let child = status.vms[0].child.as_ref().expect("child summary");
        assert!(child.reachable);
        assert_eq!(child.node_id.as_deref(), Some("vm-a"));
        assert_eq!(child.profile.as_deref(), Some("vm"));
        assert_eq!(child.api_port, Some(7700));
        assert_eq!(child.hls.as_deref(), Some("running"));
        assert_eq!(child.webrtc.as_deref(), Some("not-configured"));
        assert_eq!(child.audio.as_deref(), Some("running"));
        assert_eq!(
            child.power.as_ref().map(|power| power.state.as_str()),
            Some("running")
        );
        assert_eq!(
            child.input.as_ref().map(|input| input.kind.as_str()),
            Some("qmp")
        );
        assert!(child.input.as_ref().unwrap().injecting);
        assert!(!child.stale);
        assert!(child.last_seen_unix_ms.unwrap() > 0);
        assert!(child.last_poll_age_ms.is_some());

        assert_eq!(
            authorization.lock().unwrap().as_deref(),
            Some("Bearer supervisor-token"),
            "the configured supervisor token is sent to children"
        );
        let json = serde_json::to_string(&status).expect("serialize status");
        assert!(!json.contains("supervisor-token"));
        assert!(!json.contains("child-secret-that-must-not-leak"));

        server.abort();
    }

    #[tokio::test]
    async fn child_poll_timeout_is_reported_and_never_fabricates_state() {
        let (port, _, server) = spawn_fake_child(child_doc(), Duration::from_millis(300)).await;
        let launcher = Arc::new(FakeNodeLauncher::new());
        let mut options = options(true);
        options.node_http_base = port;
        options.child_poll_interval = Duration::from_millis(20);
        options.child_poll_timeout = Duration::from_millis(40);
        let mgr = manager(options, launcher);

        mgr.inner
            .reconcile(vec![vm("vm-a", "/run/ozma/a.qmp")])
            .await;
        mgr.inner.supervise().await;
        mgr.inner.poll_children().await;

        let status = mgr.status().await;
        let child = status.vms[0].child.as_ref().expect("poll record");
        assert!(!child.reachable);
        assert!(
            child.hls.is_none(),
            "a timeout must not fabricate HLS state"
        );
        assert!(child.input.is_none());
        assert!(child.stale);
        assert!(child.last_seen_age_ms.is_none());
        let error = child.last_error.as_deref().unwrap_or_default();
        assert!(
            error.contains("timed out"),
            "unexpected timeout error: {error}"
        );

        server.abort();
    }

    #[tokio::test]
    async fn child_summary_becomes_stale_after_the_configured_interval() {
        let (port, _, server) = spawn_fake_child(child_doc(), Duration::ZERO).await;
        let launcher = Arc::new(FakeNodeLauncher::new());
        let mut options = options(true);
        options.node_http_base = port;
        options.child_poll_interval = Duration::from_millis(20);
        options.child_poll_timeout = Duration::from_millis(20);
        let mgr = manager(options, launcher);

        mgr.inner
            .reconcile(vec![vm("vm-a", "/run/ozma/a.qmp")])
            .await;
        mgr.inner.supervise().await;
        mgr.inner.poll_children().await;

        // stale_after is interval * 2 + timeout (60 ms here).
        tokio::time::sleep(Duration::from_millis(120)).await;
        let status = mgr.status().await;
        let child = status.vms[0].child.as_ref().expect("poll record");
        assert!(
            child.reachable,
            "the last successful poll is still reachable"
        );
        assert!(
            child.stale,
            "the cached summary is older than the threshold"
        );
        assert!(child.last_seen_age_ms.unwrap_or_default() >= 60);
        assert!(child.last_seen_unix_ms.unwrap() > 0);

        server.abort();
    }

    #[tokio::test]
    async fn child_aggregation_is_absent_when_polling_is_disabled() {
        let launcher = Arc::new(FakeNodeLauncher::new());
        let mgr = manager(options(true), launcher);

        mgr.inner
            .reconcile(vec![vm("vm-a", "/run/ozma/a.qmp")])
            .await;
        mgr.inner.supervise().await;

        let status = mgr.status().await;
        assert_eq!(status.child_poll_interval_ms, 0);
        assert!(status.vms[0].child.is_none());
        let metrics = crate::status::render_metrics(&status);
        assert!(
            !metrics.contains("ozma_supervisor_vm_child_up"),
            "disabled child polling must not emit child metrics"
        );
    }

    #[test]
    fn validate_child_poll_timeout_only_when_polling_is_enabled() {
        let disabled = VirtualNodeManager::new(ManagerOptions {
            child_poll_interval: Duration::ZERO,
            child_poll_timeout: Duration::ZERO,
            ..options(true)
        });
        assert!(disabled.validate().is_ok());

        let missing_timeout = VirtualNodeManager::new(ManagerOptions {
            child_poll_interval: Duration::from_millis(500),
            child_poll_timeout: Duration::ZERO,
            ..options(true)
        });
        assert!(missing_timeout.validate().is_err());

        let enabled = VirtualNodeManager::new(ManagerOptions {
            child_poll_interval: Duration::from_millis(500),
            child_poll_timeout: Duration::from_millis(250),
            ..options(true)
        });
        assert!(enabled.validate().is_ok());
    }

    #[tokio::test]
    async fn status_reports_failure_backoff_restart_and_recovery() {
        let launcher = Arc::new(FakeNodeLauncher::new());
        let mgr = manager(options(true), Arc::clone(&launcher));

        mgr.inner
            .reconcile(vec![vm("vm-a", "/run/ozma/a.qmp")])
            .await;
        mgr.inner.supervise().await;
        assert_eq!(launcher.spawns().len(), 1);

        launcher.exit("vm-a");
        mgr.inner.supervise().await;

        let status = mgr.status().await;
        let vm_a = &status.vms[0];
        assert_eq!(vm_a.state, "backoff");
        assert!(!vm_a.spawned);
        assert_eq!(vm_a.restart_count, 0);
        assert_eq!(vm_a.last_exit.as_deref(), Some("exit code 1"));
        assert!(vm_a.last_error.as_deref().unwrap().contains("exit code 1"));
        assert!(vm_a.restart_in_ms.is_some());
        assert_eq!(status.supervisor.failed, 1);
        assert_eq!(status.supervisor.running, 0);

        tokio::time::sleep(Duration::from_millis(10)).await;
        mgr.inner.supervise().await;

        let status = mgr.status().await;
        let vm_a = &status.vms[0];
        assert_eq!(vm_a.state, "running");
        assert!(vm_a.spawned);
        assert_eq!(vm_a.restart_count, 1);
        assert_eq!(vm_a.last_exit.as_deref(), Some("exit code 1"));
        assert!(
            vm_a.last_error.is_none(),
            "a successful start clears the error"
        );
        assert_eq!(vm_a.restart_in_ms, None);
        assert_eq!(status.supervisor.failed, 0);
        assert_eq!(status.supervisor.running, 1);
    }

    #[tokio::test]
    async fn status_reports_spawn_failures_without_fabricating_a_pid() {
        let launcher = Arc::new(FakeNodeLauncher::new());
        launcher.fail_spawn("vm-a");
        let mgr = manager(options(true), Arc::clone(&launcher));

        mgr.inner
            .reconcile(vec![vm("vm-a", "/run/ozma/a.qmp")])
            .await;
        mgr.inner.supervise().await;

        let status = mgr.status().await;
        let vm_a = &status.vms[0];
        assert_eq!(vm_a.state, "backoff");
        assert!(!vm_a.spawned);
        assert_eq!(vm_a.child_pid, None);
        assert_eq!(vm_a.restart_count, 0);
        assert!(vm_a.restart_in_ms.is_some());
        assert!(vm_a
            .last_error
            .as_deref()
            .unwrap()
            .contains("injected spawn failure"));
        assert_eq!(status.supervisor.failed, 1);
    }

    #[tokio::test]
    async fn status_drops_removed_vms_and_keeps_port_bookkeeping() {
        let launcher = Arc::new(FakeNodeLauncher::new());
        let mgr = manager(options(true), Arc::clone(&launcher));

        mgr.inner
            .reconcile(vec![
                vm("vm-a", "/run/ozma/a.qmp"),
                vm("vm-b", "/run/ozma/b.qmp"),
            ])
            .await;
        mgr.inner.supervise().await;
        assert_eq!(mgr.status().await.supervisor.running, 2);

        mgr.inner
            .reconcile(vec![vm("vm-a", "/run/ozma/a.qmp")])
            .await;

        let status = mgr.status().await;
        assert_eq!(status.supervisor.discovered, 1);
        assert_eq!(status.supervisor.tracked, 1);
        assert_eq!(status.supervisor.running, 1);
        assert!(status.vms.iter().all(|vm| vm.name == "vm-a"));
        // vm-b held offset 1; the allocator stays monotonic and deterministic.
        assert_eq!(status.supervisor.next_hid_port, Some(7402));
        assert_eq!(status.supervisor.next_http_port, Some(7502));
    }

    #[tokio::test]
    async fn status_reports_needs_restart_and_provisioning_failure() {
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

        let status = mgr.status().await;
        let vm_a = &status.vms[0];
        assert_eq!(vm_a.state, "needs-restart");
        assert!(vm_a.needs_restart);
        assert!(vm_a.skipped);
        assert!(vm_a
            .last_error
            .as_deref()
            .unwrap()
            .contains("restart required"));
        assert_eq!(status.supervisor.skipped, 1);

        provisioner.set_outcome(FakeOutcome::Failed);
        mgr.inner.reconcile(vec![vm("vm-a", "")]).await;

        let status = mgr.status().await;
        let vm_a = &status.vms[0];
        assert_eq!(vm_a.state, "failed");
        assert!(vm_a
            .last_error
            .as_deref()
            .unwrap()
            .contains("fake dumpxml failure"));
        assert_eq!(status.supervisor.failed, 1);
        assert_eq!(status.supervisor.skipped, 0);
    }

    #[tokio::test]
    async fn status_reports_evdev_audio_sink_and_agent_state() {
        let launcher = Arc::new(FakeNodeLauncher::new());
        let provisioner = Arc::new(FakeVmProvisioner::new(FakeOutcome::Ready));
        let evdev = Arc::new(FakeEvdevProvisioner::new("/dev/input"));
        let audio = Arc::new(FakeAudioSinkProvisioner::new("ozma-"));
        let mut opts = ManagerOptions {
            provision: true,
            audio: AudioProvisionConfig {
                enabled: true,
                backend: AudioBackend::PipeWire,
                ..AudioProvisionConfig::default()
            },
            ..options(true)
        };
        opts.provision_config.evdev.enabled = true;
        let mgr = manager_with_device_provisioners(
            opts,
            Arc::clone(&launcher),
            Arc::clone(&provisioner),
            Some(Arc::clone(&evdev)),
            Some(Arc::clone(&audio)),
        );

        mgr.inner.reconcile(vec![vm("vm-a", "")]).await;
        mgr.inner.supervise().await;

        let status = mgr.status().await;
        let vm_a = &status.vms[0];
        assert_eq!(vm_a.state, "running");
        assert_eq!(vm_a.evdev_status, "ready");
        assert_eq!(vm_a.evdev_keyboard.as_deref(), Some("/dev/input/vm-a-kbd"));
        assert_eq!(vm_a.evdev_mouse.as_deref(), Some("/dev/input/vm-a-mouse"));
        assert_eq!(vm_a.audio_sink.as_deref(), Some("ozma-vm-a"));
        assert_eq!(vm_a.agent_status, "pending");

        // Simulate agent outcomes without touching a guest.
        {
            let mut state = mgr.inner.state.lock().await;
            let managed = state.vms.get_mut("vm-a").unwrap();
            managed.agent_provisioned = true;
        }
        assert_eq!(mgr.status().await.vms[0].agent_status, "provisioned");
    }

    #[tokio::test]
    async fn status_reports_discovery_only_and_agent_disabled() {
        let launcher = Arc::new(FakeNodeLauncher::new());
        let mgr = manager(
            ManagerOptions {
                auto_agent: false,
                ..options(false)
            },
            Arc::clone(&launcher),
        );

        mgr.inner
            .reconcile(vec![vm("vm-a", "/run/ozma/a.qmp")])
            .await;
        mgr.inner.supervise().await;

        let status = mgr.status().await;
        let vm_a = &status.vms[0];
        assert_eq!(vm_a.state, "discovery-only");
        assert_eq!(vm_a.agent_status, "disabled");
        assert!(launcher.spawns().is_empty());
    }
}
