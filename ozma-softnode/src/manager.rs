//! `VirtualNodeManager` — one softnode task per VM, lifecycle management.
//!
//! Spawns a Tokio task per VM that runs `ozma-softnode` as a child process,
//! restarting it on failure. VM discovery runs every 10 s; agent provisioning
//! retries every 30 s.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::agent::AgentProvisioner;
use crate::discovery;
use crate::vm_info::VMInfo;

// ── ManagedVM ──────────────────────────────────────────────────────────────

struct ManagedVM {
    vm: VMInfo,
    handle: JoinHandle<()>,
    agent_provisioned: bool,
    agent_check_failures: u32,
}

// ── VirtualNodeManager ─────────────────────────────────────────────────────

pub struct VirtualNodeManager {
    controller_url: String,
    qmp_dir: String,
    audio_prefix: String,
    auto_manage: bool,
    auto_agent: bool,
    exclude_patterns: Vec<String>,
    managed: Arc<Mutex<HashMap<String, ManagedVM>>>,
    provisioner: Arc<AgentProvisioner>,
    stop_tx: tokio::sync::watch::Sender<bool>,
    stop_rx: tokio::sync::watch::Receiver<bool>,
    next_port: Arc<Mutex<u16>>,
}

impl VirtualNodeManager {
    pub fn new(
        controller_url: impl Into<String>,
        qmp_dir: impl Into<String>,
        base_port: u16,
        audio_prefix: impl Into<String>,
        auto_manage: bool,
        auto_agent: bool,
        exclude_patterns: Vec<String>,
    ) -> Self {
        let controller_url = controller_url.into();
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        Self {
            provisioner: Arc::new(AgentProvisioner::new(controller_url.clone())),
            controller_url,
            qmp_dir: qmp_dir.into(),
            audio_prefix: audio_prefix.into(),
            auto_manage,
            auto_agent,
            exclude_patterns,
            managed: Arc::new(Mutex::new(HashMap::new())),
            stop_tx,
            stop_rx,
            next_port: Arc::new(Mutex::new(base_port)),
        }
    }

    /// Run until signalled to stop.
    pub async fn run(&self) {
        info!("Virtual Node Manager starting...");
        info!(
            "  Controller: {}",
            if self.controller_url.is_empty() {
                "(auto-discover)"
            } else {
                &self.controller_url
            }
        );
        info!(
            "  Auto-manage: {}",
            if self.auto_manage { "on" } else { "off" }
        );
        info!(
            "  Auto-agent:  {}",
            if self.auto_agent { "on" } else { "off" }
        );
        if !self.exclude_patterns.is_empty() {
            info!("  Exclude:     {}", self.exclude_patterns.join(", "));
        }

        self.discover_and_sync().await;

        // Spawn background tasks sharing Arc state via inner().
        let inner = self.inner();

        let inner_watch = Arc::clone(&inner);
        tokio::spawn(async move { inner_watch.watch_loop().await });

        if self.auto_agent {
            let inner_agent = Arc::clone(&inner);
            tokio::spawn(async move { inner_agent.agent_loop().await });
        }

        let mut rx = self.stop_rx.clone();
        let _ = rx.wait_for(|v| *v).await;
    }

    /// Signal the manager to stop and abort all VM tasks.
    pub async fn stop(&self) {
        let _ = self.stop_tx.send(true);
        let mut managed = self.managed.lock().await;
        for (_, m) in managed.drain() {
            m.handle.abort();
        }
    }

    // ── Shared inner state ────────────────────────────────────────────────

    fn inner(&self) -> Arc<ManagerInner> {
        Arc::new(ManagerInner {
            controller_url: self.controller_url.clone(),
            qmp_dir: self.qmp_dir.clone(),
            audio_prefix: self.audio_prefix.clone(),
            auto_manage: self.auto_manage,
            exclude_patterns: self.exclude_patterns.clone(),
            managed: Arc::clone(&self.managed),
            provisioner: Arc::clone(&self.provisioner),
            stop_rx: self.stop_rx.clone(),
            next_port: Arc::clone(&self.next_port),
        })
    }

    async fn discover_and_sync(&self) {
        ManagerInner {
            controller_url: self.controller_url.clone(),
            qmp_dir: self.qmp_dir.clone(),
            audio_prefix: self.audio_prefix.clone(),
            auto_manage: self.auto_manage,
            exclude_patterns: self.exclude_patterns.clone(),
            managed: Arc::clone(&self.managed),
            provisioner: Arc::clone(&self.provisioner),
            stop_rx: self.stop_rx.clone(),
            next_port: Arc::clone(&self.next_port),
        }
        .discover_and_sync()
        .await;
    }
}

// ── ManagerInner — Arc-shareable state for spawned tasks ──────────────────

struct ManagerInner {
    controller_url: String,
    qmp_dir: String,
    audio_prefix: String,
    auto_manage: bool,
    exclude_patterns: Vec<String>,
    managed: Arc<Mutex<HashMap<String, ManagedVM>>>,
    provisioner: Arc<AgentProvisioner>,
    stop_rx: tokio::sync::watch::Receiver<bool>,
    next_port: Arc<Mutex<u16>>,
}

impl ManagerInner {
    fn is_excluded(&self, name: &str) -> bool {
        self.exclude_patterns.iter().any(|pat| glob_match(pat, name))
    }

    async fn discover_and_sync(&self) {
        if !self.auto_manage {
            return;
        }
        let vms = match discovery::discover_vms(&self.qmp_dir).await {
            Ok(v) => v,
            Err(e) => {
                warn!("VM discovery error: {}", e);
                return;
            }
        };

        let current_names: std::collections::HashSet<String> =
            vms.iter().map(|v| v.name.clone()).collect();

        let managed_names: Vec<String> = self.managed.lock().await.keys().cloned().collect();

        // Start nodes for new VMs
        for vm in vms {
            let already = self.managed.lock().await.contains_key(&vm.name);
            if !already && !self.is_excluded(&vm.name) {
                self.start_node(vm).await;
            }
        }

        // Stop nodes for VMs that disappeared
        let gone: Vec<String> = managed_names
            .into_iter()
            .filter(|n| !current_names.contains(n))
            .collect();
        for name in gone {
            self.stop_node(&name).await;
        }
    }

    async fn start_node(&self, vm: VMInfo) {
        let port = {
            let mut p = self.next_port.lock().await;
            let cur = *p;
            *p += 1;
            cur
        };

        let name = vm.name.clone();
        let vm_clone = vm.clone();
        let controller = self.controller_url.clone();
        let audio_prefix = self.audio_prefix.clone();
        let mut stop_rx = self.stop_rx.clone();

        let handle = tokio::spawn(async move {
            run_softnode_task(vm_clone, port, controller, audio_prefix, &mut stop_rx).await;
        });

        info!(
            "Auto-managed VM: {} (port={}, os={}, gpu_passthrough={})",
            name,
            port,
            if vm.guest_os.is_empty() { "unknown" } else { &vm.guest_os },
            vm.has_gpu_passthrough,
        );

        self.managed.lock().await.insert(
            name,
            ManagedVM {
                vm,
                handle,
                agent_provisioned: false,
                agent_check_failures: 0,
            },
        );
    }

    async fn stop_node(&self, name: &str) {
        if let Some(m) = self.managed.lock().await.remove(name) {
            m.handle.abort();
        }
        info!("Stopped virtual node: {}", name);
    }

    async fn watch_loop(&self) {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
        loop {
            interval.tick().await;
            if *self.stop_rx.borrow() {
                break;
            }
            self.discover_and_sync().await;
        }
    }

    async fn agent_loop(&self) {
        // Let nodes stabilise before first attempt
        tokio::time::sleep(std::time::Duration::from_secs(15)).await;

        let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
        loop {
            interval.tick().await;
            if *self.stop_rx.borrow() {
                break;
            }

            let names: Vec<String> = self.managed.lock().await.keys().cloned().collect();

            for name in names {
                let (already_provisioned, failures, has_gpu, vm) = {
                    let m = self.managed.lock().await;
                    let Some(mv) = m.get(&name) else { continue };
                    (
                        mv.agent_provisioned,
                        mv.agent_check_failures,
                        mv.vm.has_gpu_passthrough,
                        mv.vm.clone(),
                    )
                };

                if already_provisioned {
                    continue;
                }

                // GPU passthrough VMs: never give up (agent is the only display path).
                // Other VMs: stop after 5 failures.
                let max_failures: Option<u32> = if has_gpu { None } else { Some(5) };
                if max_failures.map(|m| failures > m).unwrap_or(false) {
                    continue;
                }

                if self.provisioner.provision(&vm).await {
                    if let Some(mv) = self.managed.lock().await.get_mut(&name) {
                        mv.agent_provisioned = true;
                    }
                    info!("Agent provisioned in {}", name);
                } else {
                    if let Some(mv) = self.managed.lock().await.get_mut(&name) {
                        mv.agent_check_failures += 1;
                        if has_gpu {
                            debug!(
                                "GPU passthrough VM {}: agent not yet installed \
                                 (attempt {}) — will keep retrying",
                                name, mv.agent_check_failures
                            );
                        }
                    }
                }
            }
        }
    }
}

// ── Per-VM softnode task ───────────────────────────────────────────────────

/// Runs `ozma-softnode` as a child process for one VM, restarting on failure.
///
/// In a full integration this would call into the softnode library directly.
/// For now it shells out to the `ozma-softnode` binary so the manager can be
/// deployed independently of the per-VM softnode implementation.
async fn run_softnode_task(
    vm: VMInfo,
    port: u16,
    controller_url: String,
    audio_prefix: String,
    stop_rx: &mut tokio::sync::watch::Receiver<bool>,
) {
    loop {
        if *stop_rx.borrow() {
            break;
        }

        let mut cmd = tokio::process::Command::new("ozma-softnode");
        cmd.arg("--name").arg(&vm.name);
        cmd.arg("--port").arg(port.to_string());
        cmd.arg("--audio-sink")
            .arg(format!("{}{}", audio_prefix, vm.name));

        if !vm.qmp_path.is_empty() {
            cmd.arg("--qmp").arg(&vm.qmp_path);
        }
        if vm.vnc_port > 0 && !vm.has_gpu_passthrough {
            cmd.arg("--vnc-port").arg(vm.vnc_port.to_string());
            cmd.arg("--vnc-host").arg(&vm.vnc_host);
        }
        if !controller_url.is_empty() {
            cmd.arg("--controller").arg(&controller_url);
        }

        match cmd.spawn() {
            Ok(mut child) => {
                tokio::select! {
                    status = child.wait() => {
                        match status {
                            Ok(s) if s.success() => {
                                info!("softnode for {} exited cleanly", vm.name);
                                break;
                            }
                            Ok(s) => {
                                warn!(
                                    "softnode for {} exited with {}, restarting in 5s",
                                    vm.name, s
                                );
                            }
                            Err(e) => {
                                warn!(
                                    "softnode for {} wait error: {}, restarting in 5s",
                                    vm.name, e
                                );
                            }
                        }
                    }
                    _ = stop_rx.changed() => {
                        let _ = child.kill().await;
                        break;
                    }
                }
            }
            Err(e) => {
                warn!(
                    "Failed to spawn ozma-softnode for {}: {} — retrying in 5s",
                    vm.name, e
                );
            }
        }

        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
            _ = stop_rx.changed() => break,
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
