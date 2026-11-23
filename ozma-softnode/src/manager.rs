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
//! This is the subprocess transition, not the full Phase 2 provisioning story.
//! QMP/display socket creation, libvirt XML edits, D-Bus p2p display, evdev
//! `input-linux`, PulseAudio routing, and libvirt power remain separate work.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::sync::{watch, Mutex};
use tracing::{debug, info, warn};

use crate::agent::AgentProvisioner;
use crate::discovery;
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
    state: Mutex<SupervisorState>,
}

impl VirtualNodeManager {
    /// Production constructor: spawns real `ozma-node` children.
    pub fn new(options: ManagerOptions) -> Self {
        Self::with_launcher(options, Arc::new(ProcessNodeLauncher))
    }

    /// Constructor with an injected launcher, used by tests.
    pub fn with_launcher(options: ManagerOptions, launcher: Arc<dyn NodeLauncher>) -> Self {
        let options = Arc::new(options);
        let (stop_tx, stop_rx) = watch::channel(false);
        let inner = Arc::new(ManagerInner {
            options: Arc::clone(&options),
            launcher,
            provisioner: Arc::new(AgentProvisioner::new(options.controller_url.clone())),
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
        if !self.options.exclude_patterns.is_empty() {
            info!(
                "  Exclude:     {}",
                self.options.exclude_patterns.join(", ")
            );
        }
    }
}

// ── ManagerInner — shared supervision state ────────────────────────────────

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

    /// Apply one discovery snapshot. New VMs are inserted, removed VMs are
    /// stopped, and tracked VMs that are still present are left untouched.
    async fn reconcile(&self, mut vms: Vec<VMInfo>) {
        vms.sort_by(|a, b| a.name.cmp(&b.name));
        vms.dedup_by(|a, b| a.name == b.name);

        let current: HashSet<String> = vms.iter().map(|vm| vm.name.clone()).collect();
        let mut to_stop: Vec<NodeHandle> = Vec::new();

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
                    info!("VM no longer running: {}", name);
                }
            }

            for vm in vms {
                if state.vms.contains_key(&vm.name) {
                    continue;
                }
                if self.is_excluded(&vm.name) {
                    debug!("Excluded VM: {}", vm.name);
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

                let node = if !self.options.auto_manage {
                    None
                } else if vm.qmp_path.trim().is_empty() {
                    warn!(
                        "VM {} has no QMP socket; not starting ozma-node. \
                         QMP socket provisioning is not part of the subprocess \
                         supervisor, and no socket path will be fabricated.",
                        vm.name
                    );
                    None
                } else {
                    match state.ports.allocate(&vm.name) {
                        Ok(ports) => Some(NodeState::new(build_node_spec(
                            &vm,
                            ports,
                            &self.options.node_bin,
                            &self.options.controller_url,
                        ))),
                        Err(error) => {
                            warn!("VM {}: cannot allocate per-VM ports: {}", vm.name, error);
                            None
                        }
                    }
                };

                state.vms.insert(
                    vm.name.clone(),
                    ManagedVm {
                        vm,
                        agent_provisioned: false,
                        agent_check_failures: 0,
                        node,
                    },
                );
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
        let handles = {
            let mut state = self.state.lock().await;
            let mut names: Vec<String> = state.vms.keys().cloned().collect();
            names.sort();

            let mut handles = Vec::new();
            for name in names {
                if let Some(mut managed) = state.vms.remove(&name) {
                    state.ports.release(&name);
                    if let Some(node) = managed.node.take() {
                        if let Some(handle) = node.handle {
                            handles.push(handle);
                        }
                    }
                }
            }
            state.ports.clear();
            handles
        };

        for handle in handles {
            let name = handle.vm_name().to_owned();
            info!("Stopping ozma-node for VM {}", name);
            if let Err(error) = self.launcher.stop(handle).await {
                warn!("Failed to stop ozma-node for VM {}: {}", name, error);
            }
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
