// SPDX-License-Identifier: AGPL-3.0-only
//! Seat supervision: spawn, readiness, restart, stop, and status.
//!
//! [`ProcessSeatSupervisor`] launches one `ozma-node --profile desktop` child
//! per [`SeatPlan`], using the plan's exact argv and environment (`DISPLAY`,
//! `PULSE_SINK`, `OZMA_SEAT_ID`, ...). It waits for the child's HTTP health
//! endpoint (or a bounded TCP connect), restarts exited children with bounded
//! exponential backoff, and reports pid/ports/state/restarts/last error.
//!
//! Process spawning, readiness probing, and child lifetime are behind the
//! [`NodeLauncher`] and [`ReadinessProbe`] traits so tests never execute a real
//! node. Production uses [`ProcessLauncher`] and [`HttpHealthProbe`].
//!
//! The supervisor is synchronous because the planner and status surface are
//! synchronous. A caller with an async runtime runs [`ProcessSeatSupervisor::tick`]
//! from its own task. Nothing here injects input or creates displays/audio.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::error::MultiseatError;
use crate::seat::{SeatPlan, SeatRuntimeStatus, SeatState};

/// Default node binary used by the planner and the process launcher.
pub const DEFAULT_NODE_BIN: &str = "ozma-node";
/// Default readiness timeout for a newly spawned child.
pub const DEFAULT_READINESS_TIMEOUT: Duration = Duration::from_secs(15);
/// Default interval between readiness probes.
pub const DEFAULT_READINESS_POLL: Duration = Duration::from_millis(200);
/// Default first restart delay.
pub const DEFAULT_RESTART_BACKOFF: Duration = Duration::from_secs(1);
/// Default maximum restart delay.
pub const DEFAULT_RESTART_BACKOFF_MAX: Duration = Duration::from_secs(30);
/// A child that stayed up this long resets its failure count.
pub const DEFAULT_HEALTHY_UPTIME: Duration = Duration::from_secs(30);
/// Default child health path.
pub const DEFAULT_HEALTH_PATH: &str = "/health";
/// How long a child gets to exit after SIGTERM before it is killed.
pub const TERMINATE_GRACE: Duration = Duration::from_secs(5);

/// Everything needed to launch one seat's `ozma-node` child.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeLaunchSpec {
    /// Stable seat id.
    pub seat_id: String,
    /// Controller-visible node name.
    pub node_name: String,
    /// Node binary or path.
    pub program: String,
    /// Arguments after the program name.
    pub args: Vec<String>,
    /// Additional environment variables for the child.
    pub env: BTreeMap<String, String>,
    /// Planned HID UDP port.
    pub udp_port: u16,
    /// Planned HTTP API / health port.
    pub http_port: u16,
}

impl NodeLaunchSpec {
    /// Build a launch specification from a resolved seat plan.
    pub fn from_plan(plan: &SeatPlan) -> Self {
        Self {
            seat_id: plan.config.id.clone(),
            node_name: plan.node_name.clone(),
            program: plan.command.program.clone(),
            args: plan.command.args.clone(),
            env: plan.command.env.clone(),
            udp_port: plan.config.udp_port,
            http_port: plan.config.http_port,
        }
    }

    /// Full argv, including the program as element zero.
    pub fn argv(&self) -> Vec<String> {
        let mut argv = Vec::with_capacity(self.args.len() + 1);
        argv.push(self.program.clone());
        argv.extend(self.args.iter().cloned());
        argv
    }

    /// Address used by the readiness probe.
    pub fn health_addr(&self) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], self.http_port))
    }
}

/// Exit information for a supervised child.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChildExit {
    /// Whether the child exited successfully.
    pub success: bool,
    /// Exit code, or `None` when it was terminated by a signal.
    pub code: Option<i32>,
}

impl std::fmt::Display for ChildExit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.code {
            Some(code) => write!(formatter, "exit code {code}"),
            None => formatter.write_str(if self.success { "success" } else { "signal" }),
        }
    }
}

/// Opaque handle to one spawned child process.
pub trait ChildHandle: Send {
    /// Process id, when the launcher reported one.
    fn pid(&self) -> Option<u32>;

    /// Non-blocking child check.
    fn try_wait(&mut self) -> std::io::Result<Option<ChildExit>>;

    /// Stop and reap the child. Implementations should request a graceful exit
    /// first and only then force termination.
    fn terminate(&mut self) -> Result<(), MultiseatError>;
}

/// Spawn/stop seam. Tests inject a fake implementation; production uses
/// [`ProcessLauncher`].
pub trait NodeLauncher: Send + Sync {
    /// Spawn one node process.
    fn spawn(&self, spec: &NodeLaunchSpec) -> Result<Box<dyn ChildHandle>, MultiseatError>;
}

/// Readiness seam. Returning `Ok(false)` means "not ready yet"; an `Err` is a
/// probe failure that aborts the current start attempt.
pub trait ReadinessProbe: Send + Sync {
    /// Probe one child once.
    fn probe(&self, spec: &NodeLaunchSpec) -> Result<bool, MultiseatError>;
}

/// Production launcher backed by [`std::process::Command`].
///
/// The child inherits the supervisor's stdout/stderr so node logs remain
/// visible. Environment variables from the plan are added to the inherited
/// environment rather than replacing it.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProcessLauncher;

impl NodeLauncher for ProcessLauncher {
    fn spawn(&self, spec: &NodeLaunchSpec) -> Result<Box<dyn ChildHandle>, MultiseatError> {
        let child = Command::new(&spec.program)
            .args(&spec.args)
            .envs(&spec.env)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| {
                MultiseatError::Supervisor(format!(
                    "failed to spawn {} for seat {}: {error}",
                    spec.program, spec.seat_id
                ))
            })?;
        Ok(Box::new(ProcessChild { child }))
    }
}

struct ProcessChild {
    child: Child,
}

impl ChildHandle for ProcessChild {
    fn pid(&self) -> Option<u32> {
        Some(self.child.id())
    }

    fn try_wait(&mut self) -> std::io::Result<Option<ChildExit>> {
        self.child.try_wait().map(|status| {
            status.map(|status| ChildExit {
                success: status.success(),
                code: status.code(),
            })
        })
    }

    fn terminate(&mut self) -> Result<(), MultiseatError> {
        if self
            .child
            .try_wait()
            .map_err(|error| MultiseatError::Supervisor(format!("polling child: {error}")))?
            .is_some()
        {
            return Ok(());
        }

        #[cfg(unix)]
        {
            // SIGTERM lets the node deregister and close sockets. A dead pid
            // is harmless; the wait below still reaps a zombie.
            let pid = self.child.id();
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid as i32),
                nix::sys::signal::Signal::SIGTERM,
            );

            let deadline = Instant::now() + TERMINATE_GRACE;
            loop {
                match self.child.try_wait() {
                    Ok(Some(_)) => return Ok(()),
                    Ok(None) => {}
                    Err(error) => {
                        return Err(MultiseatError::Supervisor(format!(
                            "polling child for shutdown: {error}"
                        )))
                    }
                }
                if Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }

        self.child
            .kill()
            .map_err(|error| MultiseatError::Supervisor(format!("killing child: {error}")))?;
        self.child
            .wait()
            .map_err(|error| MultiseatError::Supervisor(format!("reaping child: {error}")))?;
        Ok(())
    }
}

/// Readiness probe that performs a bounded HTTP GET against the child.
#[derive(Clone, Debug)]
pub struct HttpHealthProbe {
    path: String,
    timeout: Duration,
}

impl Default for HttpHealthProbe {
    fn default() -> Self {
        Self::new(DEFAULT_HEALTH_PATH)
    }
}

impl HttpHealthProbe {
    /// Probe `path` on the child's HTTP port.
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            timeout: Duration::from_millis(500),
        }
    }

    /// Override the connect/read/write timeout for one probe.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Configured health path.
    pub fn path(&self) -> &str {
        &self.path
    }
}

impl ReadinessProbe for HttpHealthProbe {
    fn probe(&self, spec: &NodeLaunchSpec) -> Result<bool, MultiseatError> {
        let mut stream = match TcpStream::connect_timeout(&spec.health_addr(), self.timeout) {
            Ok(stream) => stream,
            // Connection refused/timeout means "not listening yet", which is
            // the normal case while the child starts.
            Err(_) => return Ok(false),
        };
        let _ = stream.set_read_timeout(Some(self.timeout));
        let _ = stream.set_write_timeout(Some(self.timeout));

        let request = format!(
            "GET {} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
            self.path
        );
        if stream.write_all(request.as_bytes()).is_err() {
            return Ok(false);
        }

        let mut response = [0u8; 256];
        let read = match stream.read(&mut response) {
            Ok(read) => read,
            Err(_) => return Ok(false),
        };
        let head = String::from_utf8_lossy(&response[..read]);
        Ok(head
            .lines()
            .next()
            .is_some_and(|line| line.split_whitespace().nth(1) == Some("200")))
    }
}

/// Readiness probe that only checks the HTTP port is accepting connections.
///
/// This is the honest fallback for a node build without `/health`; it proves
/// the listener is up, not that the node completed initialization.
#[derive(Clone, Copy, Debug)]
pub struct PortProbe {
    timeout: Duration,
}

impl Default for PortProbe {
    fn default() -> Self {
        Self {
            timeout: Duration::from_millis(500),
        }
    }
}

impl PortProbe {
    /// Use an explicit connect timeout.
    pub fn new(timeout: Duration) -> Self {
        Self { timeout }
    }
}

impl ReadinessProbe for PortProbe {
    fn probe(&self, spec: &NodeLaunchSpec) -> Result<bool, MultiseatError> {
        Ok(TcpStream::connect_timeout(&spec.health_addr(), self.timeout).is_ok())
    }
}

/// Supervisor configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SupervisorOptions {
    /// Override the node binary from the plan. `None` uses each plan's program.
    pub node_bin: Option<String>,
    /// How long to wait for a newly spawned child to become ready.
    pub readiness_timeout: Duration,
    /// Interval between readiness probes.
    pub readiness_poll: Duration,
    /// First restart delay.
    pub restart_backoff: Duration,
    /// Maximum restart delay.
    pub restart_backoff_max: Duration,
    /// Uptime that resets the failure count.
    pub healthy_uptime: Duration,
    /// Record plans without spawning processes.
    pub dry_run: bool,
}

impl Default for SupervisorOptions {
    fn default() -> Self {
        Self {
            node_bin: None,
            readiness_timeout: DEFAULT_READINESS_TIMEOUT,
            readiness_poll: DEFAULT_READINESS_POLL,
            restart_backoff: DEFAULT_RESTART_BACKOFF,
            restart_backoff_max: DEFAULT_RESTART_BACKOFF_MAX,
            healthy_uptime: DEFAULT_HEALTHY_UPTIME,
            dry_run: false,
        }
    }
}

/// Bounded exponential restart delay: `initial * 2^attempt`, capped at `max`.
pub fn restart_delay(initial: Duration, max: Duration, attempt: u32) -> Duration {
    if initial.is_zero() || max.is_zero() {
        return Duration::ZERO;
    }
    let initial_ms = initial.as_millis().max(1);
    let max_ms = max.as_millis().max(initial_ms);
    let factor = 1u128 << attempt.min(31);
    let delay_ms = initial_ms.saturating_mul(factor).min(max_ms);
    Duration::from_millis(u64::try_from(delay_ms).unwrap_or(u64::MAX))
}

struct ManagedSeat {
    launch: NodeLaunchSpec,
    state: SeatState,
    child: Option<Box<dyn ChildHandle>>,
    pid: Option<u32>,
    /// Successful starts, including the initial one.
    starts: u32,
    /// Consecutive failures, used for the exponential backoff.
    failures: u32,
    started_at: Option<Instant>,
    next_restart: Option<Instant>,
    last_error: Option<String>,
    last_exit: Option<String>,
    message: Option<String>,
}

impl ManagedSeat {
    fn new(launch: NodeLaunchSpec) -> Self {
        Self {
            launch,
            state: SeatState::Planned,
            child: None,
            pid: None,
            starts: 0,
            failures: 0,
            started_at: None,
            next_restart: None,
            last_error: None,
            last_exit: None,
            message: None,
        }
    }

    fn restarts(&self) -> u32 {
        self.starts.saturating_sub(1)
    }

    fn status(&self, now: Instant) -> SeatRuntimeStatus {
        let mut status = SeatRuntimeStatus::new(&self.launch.seat_id, self.state)
            .with_process(self.pid, self.launch.udp_port, self.launch.http_port)
            .with_restarts(self.restarts());
        status.last_error = self.last_error.clone();
        if let Some(next) = self.next_restart {
            status.next_restart_in_ms = Some(
                u64::try_from(next.saturating_duration_since(now).as_millis()).unwrap_or(u64::MAX),
            );
        }
        status.message = self.message.clone();
        status
    }
}

/// Real per-seat supervisor.
///
/// The public [`SeatSupervisor`] methods are the synchronous lifecycle
/// contract. [`Self::tick`] performs child polling and due restarts; call it
/// from a poll loop (the `ozma-multiseat` binary does this every
/// `--poll-interval-ms`).
pub struct ProcessSeatSupervisor {
    options: SupervisorOptions,
    launcher: Arc<dyn NodeLauncher>,
    probe: Arc<dyn ReadinessProbe>,
    seats: BTreeMap<String, ManagedSeat>,
}

impl ProcessSeatSupervisor {
    /// Production supervisor: real processes and HTTP health probes.
    pub fn new(options: SupervisorOptions) -> Self {
        Self::with_parts(
            options,
            Arc::new(ProcessLauncher),
            Arc::new(HttpHealthProbe::default()),
        )
    }

    /// Supervisor with injected launcher and readiness probe.
    pub fn with_parts(
        options: SupervisorOptions,
        launcher: Arc<dyn NodeLauncher>,
        probe: Arc<dyn ReadinessProbe>,
    ) -> Self {
        Self {
            options,
            launcher,
            probe,
            seats: BTreeMap::new(),
        }
    }

    /// Configuration in use.
    pub fn options(&self) -> &SupervisorOptions {
        &self.options
    }

    /// Number of tracked seats.
    pub fn seat_count(&self) -> usize {
        self.seats.len()
    }

    /// Poll children and start any restart whose backoff has elapsed.
    pub fn tick(&mut self) {
        self.tick_at(Instant::now());
    }

    /// Poll children and start due restarts using an explicit clock.
    pub fn tick_at(&mut self, now: Instant) {
        let seat_ids: Vec<String> = self.seats.keys().cloned().collect();

        for seat_id in seat_ids {
            let mut exit = None;
            let mut poll_error = None;
            {
                let Some(seat) = self.seats.get_mut(&seat_id) else {
                    continue;
                };
                if let Some(child) = seat.child.as_mut() {
                    match child.try_wait() {
                        Ok(Some(status)) => exit = Some(status),
                        Ok(None) => {}
                        Err(error) => poll_error = Some(error.to_string()),
                    }
                }
            }

            if let Some(exit) = exit {
                let seat = self.seats.get_mut(&seat_id).expect("tracked seat");
                // The child was reaped by `try_wait`; drop the handle.
                seat.child = None;
                seat.pid = None;
                seat.last_exit = Some(exit.to_string());
                seat.last_error = Some(format!("node for {seat_id} exited ({exit})"));
                seat.started_at = None;
                schedule_restart(&self.options, seat, now);
                continue;
            }

            if let Some(error) = poll_error {
                let seat = self.seats.get_mut(&seat_id).expect("tracked seat");
                if let Some(mut child) = seat.child.take() {
                    if let Err(cleanup) = child.terminate() {
                        // Keep ownership rather than leaking a process that
                        // could not be stopped; a later stop can retry.
                        seat.child = Some(child);
                        seat.state = SeatState::Failed;
                        seat.last_error = Some(format!(
                            "polling node for {seat_id}: {error}; cleanup failed: {cleanup}"
                        ));
                        seat.next_restart = None;
                        continue;
                    }
                }
                seat.pid = None;
                seat.last_error = Some(format!("polling node for {seat_id}: {error}"));
                seat.started_at = None;
                schedule_restart(&self.options, seat, now);
                continue;
            }

            {
                let seat = self.seats.get_mut(&seat_id).expect("tracked seat");
                if seat.state == SeatState::Running && seat.failures > 0 {
                    if let Some(started) = seat.started_at {
                        if now.saturating_duration_since(started) >= self.options.healthy_uptime {
                            seat.failures = 0;
                        }
                    }
                }
            }

            let due = self.seats.get(&seat_id).is_some_and(|seat| {
                seat.child.is_none()
                    && seat.state == SeatState::Failed
                    && seat.next_restart.is_some_and(|at| at <= now)
            });
            if due {
                // A failed restart schedules the next backoff internally.
                let _ = self.launch_once(&seat_id, now);
            }
        }
    }

    /// Stop a seat and forget it. Unknown ids are an error.
    pub fn remove(&mut self, seat_id: &str) -> Result<SeatRuntimeStatus, MultiseatError> {
        let status = self.stop(seat_id)?;
        self.seats.remove(seat_id);
        Ok(status)
    }

    /// Stop every tracked seat, returning the final statuses that stopped
    /// cleanly. Errors are left in the seat's `last_error`.
    pub fn stop_all(&mut self) -> Vec<SeatRuntimeStatus> {
        let seat_ids: Vec<String> = self.seats.keys().cloned().collect();
        let mut statuses = Vec::with_capacity(seat_ids.len());
        for seat_id in seat_ids {
            if let Ok(status) = self.stop(&seat_id) {
                statuses.push(status);
            }
        }
        statuses
    }

    fn launch_once(&mut self, seat_id: &str, now: Instant) -> Result<(), MultiseatError> {
        let mut spec = match self.seats.get(seat_id) {
            Some(seat) => seat.launch.clone(),
            None => {
                return Err(MultiseatError::UnknownSeat(seat_id.to_owned()));
            }
        };
        if let Some(node_bin) = self.options.node_bin.as_deref() {
            if !node_bin.trim().is_empty() {
                spec.program = node_bin.to_owned();
            }
        }

        match self.launcher.spawn(&spec) {
            Ok(mut child) => {
                let pid = child.pid();
                if let Some(seat) = self.seats.get_mut(seat_id) {
                    seat.state = SeatState::Starting;
                    seat.pid = pid;
                    seat.message = None;
                }

                match self.wait_ready(&spec, child.as_mut()) {
                    Ok(()) => {
                        let seat = self.seats.get_mut(seat_id).expect("tracked seat");
                        seat.child = Some(child);
                        seat.pid = pid;
                        seat.state = SeatState::Running;
                        seat.starts = seat.starts.saturating_add(1);
                        seat.started_at = Some(Instant::now());
                        seat.next_restart = None;
                        seat.failures = 0;
                        seat.last_error = None;
                        seat.last_exit = None;
                        seat.message = None;
                        Ok(())
                    }
                    Err(error) => {
                        let cleanup_failure =
                            child.terminate().err().map(|error| error.to_string());
                        let text = error.to_string();
                        let seat = self.seats.get_mut(seat_id).expect("tracked seat");
                        seat.started_at = None;
                        seat.message = None;
                        if let Some(cleanup) = cleanup_failure {
                            // Do not spawn a second child over one that could
                            // not be stopped. Keep the handle for a later stop.
                            let combined = format!("{text}; cleanup failed: {cleanup}");
                            seat.child = Some(child);
                            seat.pid = pid;
                            seat.state = SeatState::Failed;
                            seat.last_error = Some(combined.clone());
                            seat.next_restart = None;
                            return Err(MultiseatError::Supervisor(combined));
                        }
                        seat.child = None;
                        seat.pid = None;
                        seat.last_error = Some(text.clone());
                        schedule_restart(&self.options, seat, now);
                        Err(MultiseatError::Supervisor(text))
                    }
                }
            }
            Err(error) => {
                let text = error.to_string();
                let seat = self.seats.get_mut(seat_id).expect("tracked seat");
                seat.child = None;
                seat.pid = None;
                seat.started_at = None;
                seat.last_error = Some(text.clone());
                seat.message = None;
                schedule_restart(&self.options, seat, now);
                Err(MultiseatError::Supervisor(text))
            }
        }
    }

    fn wait_ready(
        &self,
        spec: &NodeLaunchSpec,
        child: &mut dyn ChildHandle,
    ) -> Result<(), MultiseatError> {
        let deadline = Instant::now() + self.options.readiness_timeout;
        loop {
            match child.try_wait() {
                Ok(Some(exit)) => {
                    return Err(MultiseatError::Supervisor(format!(
                        "node for {} exited before readiness ({exit})",
                        spec.seat_id
                    )))
                }
                Ok(None) => {}
                Err(error) => {
                    return Err(MultiseatError::Supervisor(format!(
                        "polling node for {} during readiness: {error}",
                        spec.seat_id
                    )))
                }
            }

            if Instant::now() >= deadline {
                return Err(MultiseatError::Supervisor(format!(
                    "node for {} was not ready within {:?} (expected {}:{} on 127.0.0.1)",
                    spec.seat_id, self.options.readiness_timeout, spec.program, spec.http_port
                )));
            }

            match self.probe.probe(spec) {
                Ok(true) => return Ok(()),
                Ok(false) => {}
                Err(error) => {
                    return Err(MultiseatError::Supervisor(format!(
                        "readiness probe for {} failed: {error}",
                        spec.seat_id
                    )))
                }
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                continue;
            }
            std::thread::sleep(self.options.readiness_poll.min(remaining));
        }
    }

    fn start_at(
        &mut self,
        plan: &SeatPlan,
        now: Instant,
    ) -> Result<SeatRuntimeStatus, MultiseatError> {
        let spec = NodeLaunchSpec::from_plan(plan);
        let seat_id = spec.seat_id.clone();

        if let Some(existing) = self.seats.get(&seat_id) {
            // A retained child means a previous stop/cleanup failed. Never
            // spawn a second process over it; let the operator retry stop.
            if matches!(existing.state, SeatState::Running | SeatState::Starting)
                || existing.child.is_some()
            {
                return Err(MultiseatError::SeatAlreadyRunning(seat_id));
            }
        }

        {
            let seat = self
                .seats
                .entry(seat_id.clone())
                .or_insert_with(|| ManagedSeat::new(spec.clone()));
            seat.launch = spec;
            seat.state = SeatState::Starting;
            seat.next_restart = None;
            seat.failures = 0;
            seat.last_error = None;
            seat.last_exit = None;
            seat.message = None;

            if self.options.dry_run {
                seat.state = SeatState::Planned;
                seat.pid = None;
                seat.message = Some(format!(
                    "dry-run: would launch {} ({})",
                    seat.launch.program, seat.launch.http_port
                ));
                return Ok(seat.status(now));
            }
        }

        // A start attempt owns the spawn/readiness sequence. On failure the
        // seat is retained in `failed` state with a scheduled restart so a
        // caller can either tick or retry explicitly.
        self.launch_once(&seat_id, now)?;

        self.seats
            .get(&seat_id)
            .map(|seat| seat.status(now))
            .ok_or_else(|| MultiseatError::UnknownSeat(seat_id))
    }
}

impl Drop for ProcessSeatSupervisor {
    fn drop(&mut self) {
        for seat in self.seats.values_mut() {
            if let Some(mut child) = seat.child.take() {
                let _ = child.terminate();
            }
        }
    }
}

impl SeatSupervisor for ProcessSeatSupervisor {
    fn start(&mut self, plan: &SeatPlan) -> Result<SeatRuntimeStatus, MultiseatError> {
        self.start_at(plan, Instant::now())
    }

    fn stop(&mut self, seat_id: &str) -> Result<SeatRuntimeStatus, MultiseatError> {
        let now = Instant::now();
        let seat = self
            .seats
            .get_mut(seat_id)
            .ok_or_else(|| MultiseatError::UnknownSeat(seat_id.to_owned()))?;

        if let Some(mut child) = seat.child.take() {
            if let Err(error) = child.terminate() {
                let text = error.to_string();
                // Keep the handle so Drop or a later stop can retry cleanup.
                seat.child = Some(child);
                seat.state = SeatState::Failed;
                seat.last_error = Some(text.clone());
                seat.next_restart = None;
                return Err(MultiseatError::Supervisor(text));
            }
        }

        seat.pid = None;
        seat.state = SeatState::Stopped;
        seat.started_at = None;
        seat.next_restart = None;
        seat.failures = 0;
        seat.message = None;
        Ok(seat.status(now))
    }

    fn status(&self) -> Vec<SeatRuntimeStatus> {
        let now = Instant::now();
        self.seats.values().map(|seat| seat.status(now)).collect()
    }
}

fn schedule_restart(options: &SupervisorOptions, seat: &mut ManagedSeat, now: Instant) {
    seat.state = SeatState::Failed;
    seat.failures = seat.failures.saturating_add(1);
    let delay = restart_delay(
        options.restart_backoff,
        options.restart_backoff_max,
        seat.failures.saturating_sub(1),
    );
    seat.next_restart = Some(now + delay);
}

/// Starts, stops, and reports seats.
///
/// Implementations must be idempotent where noted and must never report
/// [`SeatRuntimeStatus`] for a seat they did not start unless the state is
/// explicitly a failure.
pub trait SeatSupervisor {
    /// Start one planned seat. Returns an error if the seat is already running
    /// or the launch fails.
    fn start(&mut self, plan: &SeatPlan) -> Result<SeatRuntimeStatus, MultiseatError>;

    /// Stop one seat. Returns an error if the seat id is unknown.
    fn stop(&mut self, seat_id: &str) -> Result<SeatRuntimeStatus, MultiseatError>;

    /// Current status for every known seat.
    fn status(&self) -> Vec<SeatRuntimeStatus>;
}
