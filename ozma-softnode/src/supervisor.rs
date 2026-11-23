//! Per-VM `ozma-node` subprocess supervision.
//!
//! This module is deliberately limited to the runtime half of the
//! `ozma-softnode` supervisor:
//!
//! * allocate a stable, distinct HID UDP / HTTP port pair per VM,
//! * build the exact `ozma-node --profile vm` argv for a discovered VM,
//! * spawn, poll, restart, and stop child processes.
//!
//! QMP/display socket provisioning lives in the `provisioning` module. This
//! module only consumes a QMP path that discovery or the provisioner proved
//! usable. A VM without such a socket is skipped rather than given a fabricated
//! path.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use tracing::warn;

use crate::vm_info::VMInfo;

/// Default child binary. The real per-VM runtime is `ozma-node`, not
/// `ozma-softnode`.
pub const DEFAULT_NODE_BIN: &str = "ozma-node";
/// First HID UDP port handed to a VM. 7331 is `ozma-node`'s own default, so
/// 7332 leaves room for a hardware node on the same host.
pub const DEFAULT_BASE_PORT: u16 = 7332;
/// First HTTP port handed to a VM.
pub const DEFAULT_NODE_HTTP_BASE: u16 = 7382;
/// First restart delay.
pub const DEFAULT_RESTART_BACKOFF_MS: u64 = 1_000;
/// Maximum restart delay.
pub const DEFAULT_RESTART_BACKOFF_MAX_MS: u64 = 30_000;

/// A child that stayed up this long is considered healthy; its next failure
/// starts the backoff schedule from the initial delay again.
pub(crate) const HEALTHY_UPTIME: Duration = Duration::from_secs(30);
/// How long a child gets to exit after SIGTERM before it is killed.
const TERMINATE_GRACE: Duration = Duration::from_secs(5);

/// Ports allocated to one VM's `ozma-node`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NodePorts {
    pub hid_udp: u16,
    pub http: u16,
}

/// Everything needed to launch one per-VM `ozma-node`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeSpec {
    pub vm_name: String,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub hid_udp_port: u16,
    pub http_port: u16,
    pub qmp_socket: String,
}

impl NodeSpec {
    /// Program plus arguments, for logging and tests.
    pub fn argv(&self) -> Vec<String> {
        let mut argv = Vec::with_capacity(self.args.len() + 1);
        argv.push(self.program.to_string_lossy().into_owned());
        argv.extend(self.args.iter().cloned());
        argv
    }
}

/// Build the argv for `ozma-node --profile vm`.
///
/// `--capture-provider qmp`, `--input-provider qmp`, and `--power-provider qmp`
/// are always explicit. A dedicated `--qmp-input-socket` is added when the VM
/// exposes one; otherwise `ozma-qmp` uses the control socket in single-socket
/// mode, so input still targets the guest rather than the hypervisor host's
/// uinput default.
pub fn build_node_spec(
    vm: &VMInfo,
    ports: NodePorts,
    node_bin: &Path,
    register_url: &str,
) -> NodeSpec {
    let mut args = vec![
        "--profile".to_owned(),
        "vm".to_owned(),
        "--name".to_owned(),
        vm.name.clone(),
        "--qmp-socket".to_owned(),
        vm.qmp_path.clone(),
    ];

    let qmp_input = vm
        .qmp_input_path
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty());

    if let Some(input) = qmp_input {
        args.push("--qmp-input-socket".to_owned());
        args.push(input.to_owned());
    }

    args.extend([
        "--hid-udp-port".to_owned(),
        ports.hid_udp.to_string(),
        "--http-port".to_owned(),
        ports.http.to_string(),
        "--capture-provider".to_owned(),
        "qmp".to_owned(),
        "--input-provider".to_owned(),
        "qmp".to_owned(),
    ]);

    args.push("--power-provider".to_owned());
    args.push("qmp".to_owned());

    let register_url = register_url.trim();
    if !register_url.is_empty() {
        args.push("--register-url".to_owned());
        args.push(register_url.to_owned());
    }

    NodeSpec {
        vm_name: vm.name.clone(),
        program: node_bin.to_path_buf(),
        args,
        hid_udp_port: ports.hid_udp,
        http_port: ports.http,
        qmp_socket: vm.qmp_path.clone(),
    }
}

/// Deterministic per-VM port allocator.
///
/// Ports are held for as long as the VM is tracked, including while its child
/// is restarting, and released when discovery reports the VM gone. Allocation
/// scans a shared used-set so overlapping HID/HTTP base ranges still produce
/// distinct ports.
pub struct PortAllocator {
    hid_base: u16,
    http_base: u16,
    next_offset: u32,
    assigned: HashMap<String, NodePorts>,
    used: HashSet<u16>,
}

impl PortAllocator {
    pub fn new(hid_base: u16, http_base: u16) -> Self {
        Self {
            hid_base,
            http_base,
            next_offset: 0,
            assigned: HashMap::new(),
            used: HashSet::new(),
        }
    }

    /// Allocate or return the existing port pair for `vm_name`.
    pub fn allocate(&mut self, vm_name: &str) -> Result<NodePorts> {
        if let Some(ports) = self.assigned.get(vm_name) {
            return Ok(*ports);
        }

        loop {
            let hid = self.hid_base as u32 + self.next_offset;
            let http = self.http_base as u32 + self.next_offset;
            self.next_offset += 1;

            if hid > u16::MAX as u32 || http > u16::MAX as u32 {
                anyhow::bail!(
                    "no free per-VM port pair left (hid base {}, http base {})",
                    self.hid_base,
                    self.http_base
                );
            }

            let hid = hid as u16;
            let http = http as u16;
            if hid == http || self.used.contains(&hid) || self.used.contains(&http) {
                continue;
            }

            let ports = NodePorts { hid_udp: hid, http };
            self.used.insert(hid);
            self.used.insert(http);
            self.assigned.insert(vm_name.to_owned(), ports);
            return Ok(ports);
        }
    }

    /// Release a removed VM's ports.
    pub fn release(&mut self, vm_name: &str) {
        if let Some(ports) = self.assigned.remove(vm_name) {
            self.used.remove(&ports.hid_udp);
            self.used.remove(&ports.http);
        }
    }

    pub fn clear(&mut self) {
        self.assigned.clear();
        self.used.clear();
        self.next_offset = 0;
    }
}

/// Bounded exponential restart delay: `initial * 2^attempt`, capped at `max`.
pub(crate) fn restart_delay(initial: Duration, max: Duration, attempt: u32) -> Duration {
    if initial.is_zero() || max.is_zero() {
        return Duration::ZERO;
    }
    let initial_ms = initial.as_millis().max(1);
    let max_ms = max.as_millis().max(initial_ms);
    let factor = 1u128 << attempt.min(31);
    let delay_ms = initial_ms.saturating_mul(factor).min(max_ms);
    Duration::from_millis(u64::try_from(delay_ms).unwrap_or(u64::MAX))
}

/// Exit information for a supervised child.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NodeExitStatus {
    pub success: bool,
    pub code: Option<i32>,
}

impl std::fmt::Display for NodeExitStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.code {
            Some(code) => write!(formatter, "exit code {code}"),
            None => formatter.write_str(if self.success { "success" } else { "signal" }),
        }
    }
}

/// Opaque handle to one spawned child, owned by the supervisor per VM.
pub struct NodeHandle {
    vm_name: String,
    pid: Option<u32>,
    process: Box<dyn NodeProcess>,
}

impl NodeHandle {
    fn new(vm_name: String, pid: Option<u32>, process: Box<dyn NodeProcess>) -> Self {
        Self {
            vm_name,
            pid,
            process,
        }
    }

    pub fn vm_name(&self) -> &str {
        &self.vm_name
    }

    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// Non-blocking child check.
    pub fn try_wait(&mut self) -> std::io::Result<Option<NodeExitStatus>> {
        self.process.try_wait()
    }

    async fn terminate(&mut self) -> Result<()> {
        self.process.terminate().await
    }
}

/// Test seam over `tokio::process::Child` and fake children.
#[async_trait]
trait NodeProcess: Send {
    fn try_wait(&mut self) -> std::io::Result<Option<NodeExitStatus>>;
    async fn terminate(&mut self) -> Result<()>;
}

/// Spawn/stop seam. Tests inject a fake implementation; production spawns
/// real `ozma-node` children.
#[async_trait]
pub trait NodeLauncher: Send + Sync {
    async fn spawn(&self, spec: &NodeSpec) -> Result<NodeHandle>;

    async fn stop(&self, mut handle: NodeHandle) -> Result<()> {
        handle.terminate().await
    }
}

/// Production launcher backed by `tokio::process::Command`.
#[derive(Default)]
pub struct ProcessNodeLauncher;

#[async_trait]
impl NodeLauncher for ProcessNodeLauncher {
    async fn spawn(&self, spec: &NodeSpec) -> Result<NodeHandle> {
        let mut command = tokio::process::Command::new(&spec.program);
        command
            .args(&spec.args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true);

        let child = command.spawn().with_context(|| {
            format!(
                "spawning {} for VM {}",
                spec.program.display(),
                spec.vm_name
            )
        })?;
        let pid = child.id();
        Ok(NodeHandle::new(
            spec.vm_name.clone(),
            pid,
            Box::new(ProcessChild(child)),
        ))
    }
}

struct ProcessChild(tokio::process::Child);

#[async_trait]
impl NodeProcess for ProcessChild {
    fn try_wait(&mut self) -> std::io::Result<Option<NodeExitStatus>> {
        self.0.try_wait().map(|status| {
            status.map(|status| NodeExitStatus {
                success: status.success(),
                code: status.code(),
            })
        })
    }

    async fn terminate(&mut self) -> Result<()> {
        // Already exited: `wait` below still reaps and returns the cached
        // status on tokio's Child.
        if self.0.try_wait().ok().flatten().is_some() {
            return Ok(());
        }

        #[cfg(unix)]
        {
            if let Some(pid) = self.0.id() {
                // SIGTERM first so the node can deregister and close sockets.
                // A dead pid is harmless.
                unsafe {
                    libc::kill(pid as i32, libc::SIGTERM);
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = self.0.start_kill();
        }

        match tokio::time::timeout(TERMINATE_GRACE, self.0.wait()).await {
            Ok(result) => {
                result.context("waiting for ozma-node child")?;
                Ok(())
            }
            Err(_) => {
                warn!(
                    "ozma-node child did not exit within {:?}; killing it",
                    TERMINATE_GRACE
                );
                let _ = self.0.start_kill();
                let _ = self.0.wait().await;
                Ok(())
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! In-memory launcher and child used by unit tests. It never executes a
    //! process and lets tests simulate both spawn failures and child exits.

    use std::collections::{HashMap, HashSet};
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;

    use super::*;

    #[derive(Default)]
    pub struct FakeNodeLauncher {
        state: Arc<Mutex<FakeState>>,
    }

    #[derive(Default)]
    struct FakeState {
        next_id: u64,
        spawns: Vec<NodeSpec>,
        stops: Vec<String>,
        running: HashMap<u64, bool>,
        latest: HashMap<String, u64>,
        failing: HashSet<String>,
    }

    impl FakeNodeLauncher {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn spawns(&self) -> Vec<NodeSpec> {
            self.state.lock().unwrap().spawns.clone()
        }

        pub fn stopped(&self) -> Vec<String> {
            self.state.lock().unwrap().stops.clone()
        }

        /// Make the next (and all subsequent) spawn for `vm_name` fail.
        pub fn fail_spawn(&self, vm_name: &str) {
            self.state
                .lock()
                .unwrap()
                .failing
                .insert(vm_name.to_owned());
        }

        /// Simulate the latest child for `vm_name` exiting.
        pub fn exit(&self, vm_name: &str) {
            let mut state = self.state.lock().unwrap();
            if let Some(id) = state.latest.get(vm_name).copied() {
                state.running.insert(id, false);
            }
        }

        pub fn active(&self, vm_name: &str) -> bool {
            let state = self.state.lock().unwrap();
            state
                .latest
                .get(vm_name)
                .and_then(|id| state.running.get(id))
                .copied()
                .unwrap_or(false)
        }
    }

    #[async_trait]
    impl NodeLauncher for FakeNodeLauncher {
        async fn spawn(&self, spec: &NodeSpec) -> Result<NodeHandle> {
            let mut state = self.state.lock().unwrap();
            if state.failing.contains(&spec.vm_name) {
                anyhow::bail!("injected spawn failure for {}", spec.vm_name);
            }
            state.next_id += 1;
            let id = state.next_id;
            state.spawns.push(spec.clone());
            state.running.insert(id, true);
            state.latest.insert(spec.vm_name.clone(), id);
            drop(state);

            Ok(NodeHandle::new(
                spec.vm_name.clone(),
                Some(id as u32),
                Box::new(FakeProcess {
                    id,
                    state: Arc::clone(&self.state),
                }),
            ))
        }

        async fn stop(&self, mut handle: NodeHandle) -> Result<()> {
            self.state
                .lock()
                .unwrap()
                .stops
                .push(handle.vm_name().to_owned());
            handle.terminate().await
        }
    }

    struct FakeProcess {
        id: u64,
        state: Arc<Mutex<FakeState>>,
    }

    #[async_trait]
    impl NodeProcess for FakeProcess {
        fn try_wait(&mut self) -> std::io::Result<Option<NodeExitStatus>> {
            let running = self
                .state
                .lock()
                .unwrap()
                .running
                .get(&self.id)
                .copied()
                .unwrap_or(false);
            Ok(if running {
                None
            } else {
                Some(NodeExitStatus {
                    success: false,
                    code: Some(1),
                })
            })
        }

        async fn terminate(&mut self) -> Result<()> {
            self.state.lock().unwrap().running.insert(self.id, false);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm_info::VMInfo;

    fn vm(name: &str, qmp: &str) -> VMInfo {
        let mut vm = VMInfo::new(name);
        vm.qmp_path = qmp.to_owned();
        vm
    }

    #[test]
    fn port_allocator_is_deterministic_and_distinct() {
        let mut allocator = PortAllocator::new(7400, 7500);
        let a = allocator.allocate("vm-a").unwrap();
        let b = allocator.allocate("vm-b").unwrap();
        let a_again = allocator.allocate("vm-a").unwrap();

        assert_eq!(
            a,
            NodePorts {
                hid_udp: 7400,
                http: 7500
            }
        );
        assert_eq!(
            b,
            NodePorts {
                hid_udp: 7401,
                http: 7501
            }
        );
        assert_eq!(a, a_again);
        assert_ne!(a.hid_udp, b.hid_udp);
        assert_ne!(a.http, b.http);
    }

    #[test]
    fn port_allocator_handles_overlapping_ranges() {
        let mut allocator = PortAllocator::new(7400, 7401);
        let a = allocator.allocate("vm-a").unwrap();
        let b = allocator.allocate("vm-b").unwrap();

        // Offset 0 would give a HTTP=7401 and b HID=7401, so b advances.
        assert_eq!(
            a,
            NodePorts {
                hid_udp: 7400,
                http: 7401
            }
        );
        assert_eq!(
            b,
            NodePorts {
                hid_udp: 7402,
                http: 7403
            }
        );
    }

    #[test]
    fn restart_delay_doubles_and_caps() {
        let initial = Duration::from_millis(100);
        let max = Duration::from_millis(250);

        assert_eq!(restart_delay(initial, max, 0), Duration::from_millis(100));
        assert_eq!(restart_delay(initial, max, 1), Duration::from_millis(200));
        assert_eq!(restart_delay(initial, max, 2), Duration::from_millis(250));
        assert_eq!(restart_delay(initial, max, 10), max);
    }

    #[test]
    fn argv_includes_qmp_input_only_when_the_model_exposes_it() {
        let mut with_input = vm("vm-a", "/run/ozma/a.qmp");
        with_input.qmp_input_path = Some("/run/ozma/a-input.qmp".to_owned());

        let ports = NodePorts {
            hid_udp: 7332,
            http: 7382,
        };
        let spec = build_node_spec(
            &with_input,
            ports,
            Path::new("/usr/bin/ozma-node"),
            "http://10.0.0.1:7380",
        );

        assert_eq!(
            spec.argv(),
            vec![
                "/usr/bin/ozma-node",
                "--profile",
                "vm",
                "--name",
                "vm-a",
                "--qmp-socket",
                "/run/ozma/a.qmp",
                "--qmp-input-socket",
                "/run/ozma/a-input.qmp",
                "--hid-udp-port",
                "7332",
                "--http-port",
                "7382",
                "--capture-provider",
                "qmp",
                "--input-provider",
                "qmp",
                "--power-provider",
                "qmp",
                "--register-url",
                "http://10.0.0.1:7380",
            ]
        );

        let without_input = vm("vm-b", "/run/ozma/b.qmp");
        let spec = build_node_spec(&without_input, ports, Path::new("ozma-node"), "");
        let argv = spec.argv();
        assert!(!argv.iter().any(|arg| arg == "--qmp-input-socket"));
        let input = argv
            .iter()
            .position(|arg| arg == "--input-provider")
            .expect("input provider flag");
        // No dedicated input socket: `ozma-qmp` uses the control socket.
        assert_eq!(argv[input + 1], "qmp");
        assert!(!argv.iter().any(|arg| arg == "--register-url"));
    }
}
