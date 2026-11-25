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

use crate::audio::{SeatAudioRouter, SeatAudioSpec};
use crate::drm_lease::{
    apply_lease_environment, clear_lease_environment, DrmCardSelector, DrmLeaseBackend,
    DrmLeaseError, DrmLeaseHandle, DrmLeaseNote, DrmLeaseNoteKind, DrmLeaseRequest, DrmLeaseStatus,
};
use crate::error::MultiseatError;
use crate::input::InputGroup;
use crate::input_isolation::{helper_devices, SeatInputHelper};
use crate::input_targeting::{
    InputTargetingMode, SeatInputTargetRequest, SeatInputTargeter, SeatInputTargeting,
};
use crate::ownership::{DisplayOwnership, DisplayOwnershipController};
use crate::routing::{InputRoutePlan, InputRouter, InputRoutingStrategy, InputTarget};
use crate::seat::{
    ResourceAction, ResourceKind, SeatPlan, SeatResourceStatus, SeatRuntimeStatus, SeatState,
};
use crate::seat_display::{
    SeatDisplayBackend, SeatDisplayError, SeatDisplayHandle, SeatDisplayLauncher, SeatDisplayNote,
    SeatDisplayNoteKind, SeatDisplayOptions, SeatDisplayRequest, SeatDisplayStatus, SeatWaylandEnv,
};
use crate::seat_input::{
    SeatInputInjector, SeatInputMode, SeatInputRequest, WaylandSeatInputInjector,
};

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
    /// Apply each seat's xrandr output ownership before spawning.
    ///
    /// Off by default. When enabled without a controller, the seat fails with
    /// a typed `Unsupported` error instead of spawning unowned.
    pub apply_ownership: bool,
    /// Apply each seat's xinput/evdev input routing before spawning.
    ///
    /// Off by default. The evdev strategy still requires an injected backend;
    /// the default backend is typed unsupported.
    pub apply_routing: bool,
    /// Ask the privileged helper to grab each seat's devices before spawning.
    ///
    /// Off by default. When enabled, routing is not applied (the helper owns
    /// input) and a helper that is missing or fails a grab is a typed start
    /// failure, never a silent unisolated spawn.
    pub input_isolation: bool,
    /// How a private display is restricted to the seat's helper uinput devices.
    /// `Warn` is the default: targeting failure is recorded but the seat still
    /// starts. `Strict` prevents the spawn. `Off` never runs `xinput`.
    pub input_targeting: InputTargetingMode,
    /// Opt-in per-seat X server strategy. `Shared` (the default) starts
    /// nothing; `Xvfb`/`Xephyr`/`Xorg` start a private display before the node and stop
    /// it after the node. `Weston`/`Sway` start a private Wayland compositor.
    pub seat_display: SeatDisplayOptions,
    /// Opt-in synthetic input strategy. `Off` (the default) creates no
    /// injector; `Wayland` connects a virtual keyboard/pointer to the seat's
    /// own private Wayland socket. It never creates a global uinput device.
    pub seat_input: SeatInputMode,
    /// Opt-in DRM leases keyed by seat id. Empty by default. A lease hands the
    /// named connector/CRTC object IDs to the seat's inherited fd; it does not
    /// create scanout or compositor isolation.
    pub drm_leases: BTreeMap<String, DrmLeaseRequest>,
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
            apply_ownership: false,
            apply_routing: false,
            input_isolation: false,
            input_targeting: InputTargetingMode::Warn,
            seat_display: SeatDisplayOptions::default(),
            seat_input: SeatInputMode::Off,
            drm_leases: BTreeMap::new(),
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
    audio_spec: SeatAudioSpec,
    ownership: Option<DisplayOwnership>,
    input: Option<InputGroup>,
    input_route: Option<InputRoutePlan>,
    resources: SeatResourceStatus,
    /// Private X server for this seat, when the backend is nested.
    display: Option<Box<dyn SeatDisplayHandle>>,
    /// Status for the private X server (or the shared no-op).
    display_status: SeatDisplayStatus,
    /// Owned DRM lease for this seat, when opt-in leasing is configured.
    lease: Option<Box<dyn DrmLeaseHandle>>,
    /// DRM lease status, omitted when no lease was requested.
    drm_lease_status: DrmLeaseStatus,
    /// Shared display used as the Xephyr parent window.
    parent_display: Option<String>,
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
    fn new(plan: &SeatPlan) -> Self {
        Self {
            launch: NodeLaunchSpec::from_plan(plan),
            audio_spec: seat_audio_spec(plan),
            ownership: plan.ownership.clone(),
            input: plan.input.clone(),
            input_route: plan.input_route.clone(),
            resources: SeatResourceStatus::default(),
            display: None,
            display_status: SeatDisplayStatus::shared(),
            lease: None,
            drm_lease_status: DrmLeaseStatus::default(),
            parent_display: plan_parent_display(plan),
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
            .with_restarts(self.restarts())
            .with_resources(self.resources.clone())
            .with_display(self.display_status.clone())
            .with_drm_lease(self.drm_lease_status.clone());
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
    ownership: Option<Box<dyn DisplayOwnershipController>>,
    routing: Option<Box<dyn InputRouter>>,
    evdev_routing: Option<Box<dyn InputRouter>>,
    input_helper: Option<Box<dyn SeatInputHelper>>,
    input_targeter: Option<Box<dyn SeatInputTargeter>>,
    input_injector: Option<Box<dyn SeatInputInjector>>,
    audio: Option<Box<dyn SeatAudioRouter>>,
    display_launcher: Option<Box<dyn SeatDisplayLauncher>>,
    drm_lease_backend: Option<Box<dyn DrmLeaseBackend>>,
    seats: BTreeMap<String, ManagedSeat>,
}

impl ProcessSeatSupervisor {
    /// Production supervisor: real processes and HTTP health probes.
    ///
    /// When the seat display backend is `Xvfb` or `Xephyr`, a production
    /// [`crate::seat_display::ProcessSeatDisplayLauncher`] is installed
    /// automatically. Tests that inject parts must also inject a display
    /// launcher with [`Self::with_display_launcher`].
    pub fn new(options: SupervisorOptions) -> Self {
        let display_launcher = display_launcher_for(&options);
        let wants_lease = !options.drm_leases.is_empty();
        let mut supervisor = Self::with_parts(
            options,
            Arc::new(ProcessLauncher),
            Arc::new(HttpHealthProbe::default()),
        )
        .with_optional_display_launcher(display_launcher);
        if supervisor.options.seat_input.is_wayland() {
            supervisor.input_injector = Some(Box::new(WaylandSeatInputInjector::new()));
        }
        #[cfg(target_os = "linux")]
        if wants_lease {
            supervisor.drm_lease_backend =
                Some(Box::new(crate::drm_lease::RealDrmLeaseBackend::new()));
        }
        supervisor
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
            ownership: None,
            routing: None,
            evdev_routing: None,
            input_helper: None,
            input_targeter: None,
            input_injector: None,
            audio: None,
            display_launcher: None,
            drm_lease_backend: None,
            seats: BTreeMap::new(),
        }
    }

    fn with_optional_display_launcher(
        mut self,
        launcher: Option<Box<dyn SeatDisplayLauncher>>,
    ) -> Self {
        self.display_launcher = launcher;
        self
    }

    /// Install the private-X-server spawner used when
    /// [`SupervisorOptions::seat_display`] selects `xvfb`, `xephyr`, or `xorg`.
    ///
    /// Without a launcher, a nested start fails with a typed `Unsupported`
    /// error before a node is spawned. The CLI installs
    /// [`crate::seat_display::ProcessSeatDisplayLauncher`]; tests inject
    /// `FakeSeatDisplayLauncher`.
    pub fn with_display_launcher(mut self, launcher: Box<dyn SeatDisplayLauncher>) -> Self {
        self.display_launcher = Some(launcher);
        self
    }

    /// Install the xrandr ownership controller used by
    /// [`SupervisorOptions::apply_ownership`].
    ///
    /// Without a controller, an ownership-enabled start fails with a typed
    /// `Unsupported` error before any child is spawned.
    pub fn with_ownership_controller(
        mut self,
        controller: Box<dyn DisplayOwnershipController>,
    ) -> Self {
        self.ownership = Some(controller);
        self
    }

    /// Install the xinput input router used by
    /// [`SupervisorOptions::apply_routing`] for
    /// [`InputRoutingStrategy::XInputMapToOutput`] seats.
    pub fn with_routing_controller(mut self, router: Box<dyn InputRouter>) -> Self {
        self.routing = Some(router);
        self
    }

    /// Install the optional evdev router used by
    /// [`InputRoutingStrategy::EvdevGrab`] seats.
    ///
    /// The production CLI deliberately installs the default
    /// [`crate::routing::EvdevGrabRouter`] with its unsupported backend, so a
    /// grab request fails with a typed `Unsupported` error instead of a fake
    /// grab. Tests may inject a router backed by `FakeEvdevGrabBackend`.
    pub fn with_evdev_routing_controller(mut self, router: Box<dyn InputRouter>) -> Self {
        self.evdev_routing = Some(router);
        self
    }

    /// Install the privileged input-isolation helper client used by
    /// [`SupervisorOptions::input_isolation`].
    ///
    /// Start asks the helper to grab the seat's complete device set before the
    /// child is spawned. A missing helper, failed grab, or cross-seat conflict
    /// is a typed failure: the seat is not spawned and earlier seats are
    /// released. Stop/remove/shutdown release every confirmed seat.
    pub fn with_input_isolation_helper(mut self, helper: Box<dyn SeatInputHelper>) -> Self {
        self.input_helper = Some(helper);
        self
    }

    /// Install the `xinput` targeter that restricts a private X display to the
    /// seat's helper-created uinput devices.
    ///
    /// Applied after the helper confirms its grab and before the node child is
    /// spawned, only when the display backend is `Xvfb`/`Xephyr`/`Xorg`. Without a
    /// targeter a private display is left unrestricted and a typed
    /// `input_targeting_failed` note is recorded under the default warn
    /// policy; strict policy refuses to spawn.
    pub fn with_input_targeter(mut self, targeter: Box<dyn SeatInputTargeter>) -> Self {
        self.input_targeter = Some(targeter);
        self
    }

    /// Install the per-seat synthetic Wayland input injector used by
    /// [`SupervisorOptions::seat_input`].
    ///
    /// [`Self::new`] installs [`WaylandSeatInputInjector`] when the mode is
    /// `wayland`. Tests should inject [`crate::fake::FakeSeatInputInjector`].
    /// Without an injector a requested mode records an honest
    /// `input_injection_unavailable` note and never claims the resource.
    pub fn with_seat_input_injector(mut self, injector: Box<dyn SeatInputInjector>) -> Self {
        self.input_injector = Some(injector);
        self
    }

    /// Install the per-seat audio sink router used by `--audio-router`.
    ///
    /// With a router installed, start ensures the seat's null sink (and its
    /// `<sink>.monitor` target) before spawning, and stop/remove/shutdown
    /// removes it after the child exits. An ensure/remove failure is recorded
    /// as a typed [`ResourceKind::Audio`] error in status and does not abort the
    /// seat lifecycle. Without a router no audio command is ever run.
    pub fn with_audio_router(mut self, router: Box<dyn SeatAudioRouter>) -> Self {
        self.audio = Some(router);
        self
    }

    /// Install the DRM lease backend used by
    /// [`SupervisorOptions::drm_leases`].
    ///
    /// [`Self::new`] installs [`crate::drm_lease::RealDrmLeaseBackend`] when at
    /// least one lease request is configured. `with_parts` (tests and the CLI)
    /// must call this explicitly; without a backend, a requested lease fails
    /// with a typed `Unsupported` error before a child is spawned.
    pub fn with_drm_lease_backend(mut self, backend: Box<dyn DrmLeaseBackend>) -> Self {
        self.drm_lease_backend = Some(backend);
        self
    }

    /// Configuration in use.
    pub fn options(&self) -> &SupervisorOptions {
        &self.options
    }

    /// Number of tracked seats.
    pub fn seat_count(&self) -> usize {
        self.seats.len()
    }

    /// Whether this seat currently has a private X server.
    fn display_is_nested(&self, seat_id: &str) -> bool {
        self.seats
            .get(seat_id)
            .is_some_and(|seat| seat.display_status.display_backend.is_nested())
    }

    fn push_display_note(&mut self, seat_id: &str, note: SeatDisplayNote) {
        if let Some(seat) = self.seats.get_mut(seat_id) {
            seat.display_status.push_note(note);
        }
    }

    fn set_display_failure(&mut self, seat_id: &str, error: &MultiseatError) {
        if let Some(seat) = self.seats.get_mut(seat_id) {
            seat.display_status.display_ready = false;
            seat.display_status.display_pid = None;
            seat.display_status.display_error = Some(SeatDisplayError::from_error(error));
        }
    }

    /// Start the seat's private X server if the backend is nested and no ready
    /// server is already tracked.
    ///
    /// The returned handle is only stored after the X11 setup handshake
    /// succeeds. A timeout or crash terminates and releases the server, so a
    /// display is never claimed without readiness.
    fn ensure_seat_display(&mut self, seat_id: &str) -> Result<(), MultiseatError> {
        let backend = self.options.seat_display.backend;
        if !backend.is_nested() {
            if let Some(seat) = self.seats.get_mut(seat_id) {
                seat.display_status = SeatDisplayStatus::shared();
            }
            // Synthetic input needs a private Wayland socket; on a shared X
            // display this records the honest unavailable note instead of
            // claiming an injector.
            self.apply_seat_input(seat_id)?;
            return Ok(());
        }

        if self
            .seats
            .get(seat_id)
            .is_some_and(|seat| seat.display.is_some() && seat.display_status.display_ready)
        {
            return Ok(());
        }

        let request = {
            let Some(seat) = self.seats.get(seat_id) else {
                return Err(MultiseatError::UnknownSeat(seat_id.to_owned()));
            };
            SeatDisplayRequest {
                seat_id: seat_id.to_owned(),
                backend,
                size: self.options.seat_display.size,
                parent_display: seat
                    .parent_display
                    .clone()
                    .or_else(|| self.options.seat_display.parent_display.clone()),
            }
        };

        {
            let seat = self.seats.get_mut(seat_id).expect("tracked seat");
            seat.display_status = SeatDisplayStatus::for_backend(backend);
        }

        let launcher = match self.display_launcher.as_ref() {
            Some(launcher) => launcher,
            None => {
                let error = MultiseatError::Unsupported(format!(
                    "seat {seat_id} requested the {backend} seat display backend but no display \
                     launcher is configured"
                ));
                self.set_display_failure(seat_id, &error);
                return Err(error);
            }
        };

        let mut handle = match launcher.spawn(&request) {
            Ok(handle) => handle,
            Err(error) => {
                self.set_display_failure(seat_id, &error);
                return Err(error);
            }
        };

        let display = handle.display().to_owned();
        let pid = handle.pid();
        let auth_disabled = handle.auth_disabled();
        let wayland = handle.wayland().cloned();
        match self.wait_display_ready(handle.as_mut()) {
            Ok(()) => {}
            Err(error) => {
                let _ = handle.terminate();
                self.set_display_failure(seat_id, &error);
                return Err(error);
            }
        }

        let mut status = SeatDisplayStatus::for_backend(backend);
        status.display = Some(display.clone());
        status.display_pid = pid;
        status.display_ready = true;
        if auth_disabled {
            status.push_note(SeatDisplayNote::new(
                SeatDisplayNoteKind::AuthDisabled,
                "xauth was unavailable; the private server uses -ac on its Unix socket with \
                 -nolisten tcp",
            ));
        }
        if backend.is_wayland() {
            let detail = if backend == SeatDisplayBackend::Sway {
                "synthetic virtual input is available only when --seat-input wayland is enabled; \
                 physical libinput/uinput device targeting is not"
            } else {
                "this compositor exposes no libinput seat and no wlroots virtual-pointer global; \
                 physical libinput/uinput device targeting is not"
            };
            status.push_note(SeatDisplayNote::new(
                SeatDisplayNoteKind::InputTargetingUnavailable,
                format!("seat {seat_id} uses a headless {backend} compositor; {detail} claimed"),
            ));
        }

        let size = self.options.seat_display.size;
        let xauthority = handle.xauthority().map(std::path::Path::to_path_buf);
        let seat = self.seats.get_mut(seat_id).expect("tracked seat");
        match wayland.as_ref() {
            Some(wayland) => apply_wayland_to_launch(&mut seat.launch, wayland),
            None => {
                apply_display_to_launch(&mut seat.launch, &display, xauthority.as_deref(), size)
            }
        }
        seat.display = Some(handle);
        seat.display_status = status;
        self.apply_seat_input(seat_id)?;
        Ok(())
    }

    /// Acquire the seat's opt-in DRM lease and export its fd to the launch
    /// environment.
    ///
    /// A lease is independent of the X display and is deliberately lease-only:
    /// the status note says that no scanout or compositor isolation is created.
    /// An existing lease is reused across restarts and its environment is
    /// re-applied to the current launch spec.
    fn ensure_seat_lease(&mut self, seat_id: &str) -> Result<(), MultiseatError> {
        let Some(request) = self.options.drm_leases.get(seat_id).cloned() else {
            return Ok(());
        };

        if self
            .seats
            .get(seat_id)
            .is_some_and(|seat| seat.lease.is_some())
        {
            let seat = self.seats.get_mut(seat_id).expect("tracked seat");
            apply_lease_environment(&seat.drm_lease_status, &mut seat.launch.env);
            return Ok(());
        }

        let Some(backend) = self.drm_lease_backend.as_ref() else {
            let error = MultiseatError::Unsupported(format!(
                "seat {seat_id} requested a DRM lease but no DRM lease backend is configured"
            ));
            if let Some(seat) = self.seats.get_mut(seat_id) {
                seat.drm_lease_status = DrmLeaseStatus::requested(&request);
                seat.drm_lease_status.applied = false;
                seat.drm_lease_status.error = Some(crate::drm_lease::DrmLeaseErrorStatus {
                    kind: "unsupported_lease".to_owned(),
                    message: error.to_string(),
                });
                seat.resources.push_error(
                    ResourceKind::DrmLease,
                    ResourceAction::Apply,
                    error.to_string(),
                );
            }
            return Err(error);
        };

        let handle = match backend.acquire(&request) {
            Ok(handle) => handle,
            Err(error) => {
                if let Some(seat) = self.seats.get_mut(seat_id) {
                    let mut status = DrmLeaseStatus::requested(&request);
                    status.mark_error(&error);
                    seat.drm_lease_status = status;
                    seat.resources.push_error(
                        ResourceKind::DrmLease,
                        ResourceAction::Apply,
                        error.to_string(),
                    );
                }
                return Err(MultiseatError::DrmLease(error.to_string()));
            }
        };

        let mut status = DrmLeaseStatus::from_handle(handle.as_ref());
        if let DrmCardSelector::Explicit(path) = &request.card {
            if let Ok(cards) = backend.enumerate() {
                if let Some(card) = cards.iter().find(|card| &card.path == path) {
                    if !card.is_virtual() {
                        status.push_note(DrmLeaseNote::new(
                            DrmLeaseNoteKind::RealGpuNamed,
                            format!(
                                "operator explicitly named real card {} ({}); only the requested \
                                 connector/CRTC objects are leased",
                                card.path.display(),
                                card.driver
                            ),
                        ));
                    }
                }
            }
        }

        let seat = self.seats.get_mut(seat_id).expect("tracked seat");
        apply_lease_environment(&status, &mut seat.launch.env);
        seat.lease = Some(handle);
        seat.drm_lease_status = status;
        seat.resources.drm_lease_applied = true;
        seat.resources.drm_lease_released = false;
        Ok(())
    }

    /// Revoke and close the seat's DRM lease. The node child must already be
    /// gone (or must not have been spawned) before this is called.
    fn release_seat_lease(&mut self, seat_id: &str) -> Result<(), MultiseatError> {
        let Some(mut handle) = self
            .seats
            .get_mut(seat_id)
            .and_then(|seat| seat.lease.take())
        else {
            if self
                .seats
                .get(seat_id)
                .is_some_and(|seat| seat.resources.drm_lease_applied)
            {
                return Err(MultiseatError::Unsupported(format!(
                    "seat {seat_id} has an applied DRM lease but no lease handle is available"
                )));
            }
            return Ok(());
        };

        match handle.revoke() {
            Ok(()) => {
                if let Some(seat) = self.seats.get_mut(seat_id) {
                    seat.drm_lease_status.mark_released();
                    seat.resources.drm_lease_applied = false;
                    seat.resources.drm_lease_released = true;
                    clear_lease_environment(&mut seat.launch.env);
                }
                Ok(())
            }
            Err(error) => {
                if let Some(seat) = self.seats.get_mut(seat_id) {
                    // Keep the active lease and its applied flag so a later stop
                    // or Drop can retry; the fd is still owned by this handle.
                    seat.lease = Some(handle);
                    seat.drm_lease_status.error =
                        Some(crate::drm_lease::DrmLeaseErrorStatus::from_error(&error));
                }
                Err(MultiseatError::DrmLease(error.to_string()))
            }
        }
    }

    /// Set `FD_CLOEXEC` off before a spawn so the lessee fd is inherited by the
    /// child, then restore it. The supervisor is single-threaded, so the short
    /// window cannot leak the fd into an unrelated spawn.
    fn set_lease_inheritable(&self, seat_id: &str, inheritable: bool) -> Result<(), DrmLeaseError> {
        let Some(lease) = self.seats.get(seat_id).and_then(|seat| seat.lease.as_ref()) else {
            return Ok(());
        };
        lease.set_inheritable(inheritable)
    }

    fn wait_display_ready(&self, handle: &mut dyn SeatDisplayHandle) -> Result<(), MultiseatError> {
        let display = handle.display().to_owned();
        let backend = handle.backend();
        let timeout = self.options.seat_display.ready_timeout;
        let poll = self.options.seat_display.ready_poll;
        let deadline = Instant::now() + timeout;
        let mut detail = if backend.is_wayland() {
            "compositor did not accept a Wayland client connection".to_owned()
        } else {
            "server did not answer the X11 setup handshake".to_owned()
        };

        loop {
            match handle.try_wait() {
                Ok(Some(exit)) => {
                    return Err(MultiseatError::SeatDisplayCrashed {
                        display,
                        backend: backend.as_str().to_owned(),
                        detail: format!("before readiness ({exit})"),
                    })
                }
                Ok(None) => {}
                Err(error) => {
                    return Err(MultiseatError::SeatDisplay(format!(
                        "polling {display} during readiness: {error}"
                    )))
                }
            }

            match handle.is_ready() {
                Ok(true) => return Ok(()),
                Ok(false) => {}
                Err(error) => detail = error.to_string(),
            }

            if Instant::now() >= deadline {
                return Err(MultiseatError::SeatDisplayStartupTimeout {
                    display,
                    timeout_ms: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
                    detail,
                });
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                continue;
            }
            std::thread::sleep(poll.min(remaining));
        }
    }

    /// Stop the seat's private X server. The node child must already be gone.
    fn release_seat_display(&mut self, seat_id: &str) -> Result<(), MultiseatError> {
        let Some(mut handle) = self
            .seats
            .get_mut(seat_id)
            .and_then(|seat| seat.display.take())
        else {
            if let Some(seat) = self.seats.get_mut(seat_id) {
                seat.display_status.display = None;
                seat.display_status.display_pid = None;
                seat.display_status.display_ready = false;
            }
            self.clear_input_targeting(seat_id);
            return Ok(());
        };
        let display = handle.display().to_owned();
        match handle.terminate() {
            Ok(()) => {
                if let Some(seat) = self.seats.get_mut(seat_id) {
                    seat.display_status.display = None;
                    seat.display_status.display_pid = None;
                    seat.display_status.display_ready = false;
                }
                // The private server is gone, so any XInput restriction it
                // held is gone with it. Drop local state without running
                // commands against a dead display.
                self.clear_input_targeting(seat_id);
                Ok(())
            }
            Err(error) => {
                if let Some(seat) = self.seats.get_mut(seat_id) {
                    seat.display = Some(handle);
                    seat.display_status.display_ready = false;
                    seat.display_status.display_error = Some(SeatDisplayError::from_error(&error));
                }
                Err(MultiseatError::SeatDisplay(format!(
                    "stopping display {display} for seat {seat_id}: {error}"
                )))
            }
        }
    }

    /// Apply both opt-in resources for one seat in deterministic order:
    /// xrandr ownership first, then input routing or helper isolation.
    fn apply_seat_resources(&mut self, seat_id: &str) -> Result<(), MultiseatError> {
        if self.options.apply_ownership {
            self.apply_ownership(seat_id)?;
        }
        // Helper isolation owns input when selected. Never apply xinput mapping
        // to a device that is about to be grabbed (or was already grabbed).
        if self.options.apply_routing && !self.options.input_isolation {
            if let Err(error) = self.apply_routing(seat_id) {
                // The ownership applied a moment ago must not leak when
                // routing cannot be reserved.
                self.release_seat_resources(seat_id);
                return Err(error);
            }
        }
        if self.options.input_isolation {
            if let Err(error) = self.apply_input_isolation(seat_id) {
                self.release_seat_resources(seat_id);
                return Err(error);
            }
        }
        self.ensure_seat_audio(seat_id);
        Ok(())
    }

    /// Forget targeter state without running `xinput`. Used when the private
    /// server that owned the devices has already exited.
    fn clear_input_targeting(&mut self, seat_id: &str) {
        if let Some(targeter) = self.input_targeter.as_mut() {
            targeter.clear(seat_id);
        }
        if let Some(seat) = self.seats.get_mut(seat_id) {
            seat.resources.input_targeting_applied = false;
            seat.resources.targeted_devices.clear();
            seat.resources.disabled_devices.clear();
        }
    }

    fn record_targeting_applied(&mut self, seat_id: &str, targeting: &SeatInputTargeting) {
        if let Some(seat) = self.seats.get_mut(seat_id) {
            seat.resources.input_targeting_applied = true;
            seat.resources.input_targeting_released = false;
            seat.resources.targeted_devices = targeting.enabled.clone();
            seat.resources.disabled_devices = targeting.disabled.clone();
        }
    }

    /// Restrict the seat's private X display to its helper-created uinput
    /// devices. Shared displays and `InputTargetingMode::Off` are no-ops.
    fn apply_input_targeting(
        &mut self,
        seat_id: &str,
        own_devices: &[String],
    ) -> Result<(), MultiseatError> {
        if own_devices.is_empty() || !self.display_is_nested(seat_id) {
            return Ok(());
        }
        // The headless Weston backend has no libinput seat. There is no X
        // display to run `xinput` against and no compositor seat to attach the
        // helper's uinput nodes to, so never claim targeting. The typed
        // `input_targeting_unavailable` note is recorded when the display is
        // created.
        if self
            .seats
            .get(seat_id)
            .is_some_and(|seat| seat.display_status.display_backend.is_wayland())
        {
            self.clear_input_targeting(seat_id);
            return Ok(());
        }
        let mode = self.options.input_targeting;
        if mode == InputTargetingMode::Off {
            self.push_display_note(
                seat_id,
                SeatDisplayNote::new(
                    SeatDisplayNoteKind::InputTargetingUnavailable,
                    format!(
                        "private display input targeting is disabled for seat {seat_id}; the \
                         display may expose other seats' uinput devices"
                    ),
                ),
            );
            self.clear_input_targeting(seat_id);
            return Ok(());
        }

        let Some((display, xauthority)) = self.seats.get(seat_id).and_then(|seat| {
            seat.display_status.display.clone().map(|display| {
                (
                    display,
                    seat.launch
                        .env
                        .get("XAUTHORITY")
                        .map(std::path::PathBuf::from),
                )
            })
        }) else {
            let error = MultiseatError::InputTargeting(format!(
                "seat {seat_id} has no ready private display to target"
            ));
            return self.targeting_failure(seat_id, mode, error);
        };

        if let Some(targeter) = self.input_targeter.as_ref() {
            if let Some(existing) = targeter.status_for(seat_id) {
                if existing.matches(&display, own_devices) {
                    self.record_targeting_applied(seat_id, &existing);
                    return Ok(());
                }
            }
        }

        let Some(targeter) = self.input_targeter.as_mut() else {
            let error = MultiseatError::Unsupported(format!(
                "seat {seat_id} has a private display and helper input isolation but no XInput \
                 targeter is configured"
            ));
            return self.targeting_failure(seat_id, mode, error);
        };

        let request = SeatInputTargetRequest {
            seat_id: seat_id.to_owned(),
            display: display.clone(),
            xauthority,
            devices: own_devices.to_vec(),
        };
        match targeter.apply(&request) {
            Ok(targeting) => {
                self.record_targeting_applied(seat_id, &targeting);
                Ok(())
            }
            Err(error) => {
                targeter.clear(seat_id);
                self.targeting_failure(seat_id, mode, error)
            }
        }
    }

    /// Record a typed targeting failure and apply the warn/strict policy.
    fn targeting_failure(
        &mut self,
        seat_id: &str,
        mode: InputTargetingMode,
        error: MultiseatError,
    ) -> Result<(), MultiseatError> {
        self.record_resource_error(
            seat_id,
            ResourceKind::InputTargeting,
            ResourceAction::Apply,
            &error.to_string(),
        );
        self.push_display_note(
            seat_id,
            SeatDisplayNote::new(
                SeatDisplayNoteKind::InputTargetingFailed,
                format!(
                    "private display for seat {seat_id} is not restricted to the seat's uinput \
                     devices: {error}"
                ),
            ),
        );
        if let Some(seat) = self.seats.get_mut(seat_id) {
            seat.resources.input_targeting_applied = false;
            seat.resources.targeted_devices.clear();
            seat.resources.disabled_devices.clear();
        }
        if mode == InputTargetingMode::Strict {
            return Err(error);
        }
        Ok(())
    }

    /// Connect the seat's synthetic Wayland keyboard/pointer to its own
    /// compositor socket.
    ///
    /// Availability is only reported after [`SeatInputInjector::connect`]
    /// returns both protocol globals. A missing protocol, refused compositor,
    /// or missing injector records a typed error and an
    /// `input_injection_unavailable` display note; it never sets
    /// `input_injection_applied` or any physical-isolation flag.
    fn apply_seat_input(&mut self, seat_id: &str) -> Result<(), MultiseatError> {
        if !self.options.seat_input.is_wayland() {
            return Ok(());
        }
        let Some((seat_display, wayland)) = self.seats.get(seat_id).and_then(|seat| {
            let display = seat.display_status.display.clone()?;
            let wayland = seat.display.as_ref()?.wayland()?.clone();
            Some((display, wayland))
        }) else {
            let message = format!(
                "seat {seat_id} requested Wayland synthetic input but has no ready private \
                 Wayland display"
            );
            self.record_resource_error(
                seat_id,
                ResourceKind::SeatInput,
                ResourceAction::Apply,
                &format!("unsupported: {message}"),
            );
            self.seat_input_failure(seat_id, &message);
            return Ok(());
        };

        let Some(injector) = self.input_injector.as_mut() else {
            let message = format!(
                "seat {seat_id} requested Wayland synthetic input but no injector is configured"
            );
            self.record_resource_error(
                seat_id,
                ResourceKind::SeatInput,
                ResourceAction::Apply,
                &format!("unsupported: {message}"),
            );
            self.seat_input_failure(seat_id, &message);
            return Ok(());
        };

        let request = SeatInputRequest {
            seat_id: seat_id.to_owned(),
            wayland,
        };
        match injector.connect(&request) {
            Ok(capabilities) => {
                let available = capabilities.is_complete();
                if let Some(seat) = self.seats.get_mut(seat_id) {
                    seat.resources.input_injection_available = available;
                    seat.resources.input_injection_applied = capabilities.supports_text();
                    seat.resources.input_injection_released = false;
                    seat.resources.input_injection_events = 0;
                }
                if !capabilities.pointer {
                    self.push_display_note(
                        seat_id,
                        SeatDisplayNote::new(
                            SeatDisplayNoteKind::InputInjectionUnavailable,
                            format!(
                                "seat {seat_id} compositor did not expose \
                                 zwlr_virtual_pointer_manager_v1; keyboard text injection is \
                                 available but pointer injection is not claimed"
                            ),
                        ),
                    );
                }
                Ok(())
            }
            Err(error) => {
                self.record_resource_error(
                    seat_id,
                    ResourceKind::SeatInput,
                    ResourceAction::Apply,
                    &format!("{}: {error}", error.kind()),
                );
                let message = format!(
                    "seat {seat_id} on display {seat_display} cannot accept synthetic Wayland \
                     input: {error}"
                );
                self.seat_input_failure(seat_id, &message);
                Ok(())
            }
        }
    }

    fn seat_input_failure(&mut self, seat_id: &str, message: &str) {
        if let Some(seat) = self.seats.get_mut(seat_id) {
            seat.resources.input_injection_available = false;
            seat.resources.input_injection_applied = false;
        }
        self.push_display_note(
            seat_id,
            SeatDisplayNote::new(
                SeatDisplayNoteKind::InputInjectionUnavailable,
                message.to_owned(),
            ),
        );
    }

    /// Disconnect synthetic input without claiming availability. Used before a
    /// display is stopped or replaced.
    fn clear_seat_input(&mut self, seat_id: &str) {
        if let Some(injector) = self.input_injector.as_mut() {
            let events = injector.events_sent(seat_id);
            injector.disconnect(seat_id);
            if let Some(seat) = self.seats.get_mut(seat_id) {
                seat.resources.input_injection_events = events;
            }
        }
        if let Some(seat) = self.seats.get_mut(seat_id) {
            if seat.resources.input_injection_applied {
                seat.resources.input_injection_released = true;
            }
            seat.resources.input_injection_applied = false;
            seat.resources.input_injection_available = false;
        }
    }

    /// Ask the helper to grab the seat's complete device set.
    ///
    /// The applied flag is only set from the helper's confirmed response. If a
    /// previously applied assignment disappeared (helper restart), this
    /// re-grabs instead of trusting the stale flag.
    fn apply_input_isolation(&mut self, seat_id: &str) -> Result<(), MultiseatError> {
        let (group, already_applied) = {
            let Some(seat) = self.seats.get(seat_id) else {
                return Err(MultiseatError::UnknownSeat(seat_id.to_owned()));
            };
            (seat.input.clone(), seat.resources.input_isolation_applied)
        };
        let Some(group) = group else {
            // No input group means there is no physical input to isolate. The
            // planner already emitted a SeatWithoutInput diagnostic.
            return Ok(());
        };
        let devices = helper_devices(&group);
        if devices.is_empty() {
            return Ok(());
        }
        let expected_paths: Vec<String> =
            devices.iter().map(|device| device.path.clone()).collect();

        if self.input_helper.is_none() {
            let error = MultiseatError::Unsupported(format!(
                "seat {seat_id} requested helper input isolation but no input helper is configured"
            ));
            self.record_resource_error(
                seat_id,
                ResourceKind::InputIsolation,
                ResourceAction::Apply,
                &error.to_string(),
            );
            return Err(error);
        }

        if already_applied {
            let status = self
                .input_helper
                .as_mut()
                .expect("helper checked above")
                .seat_status(seat_id);
            match status {
                Ok(Some(status)) if status.grabbed == expected_paths => {
                    let uinput_devices = status.uinput_devices.clone();
                    if let Some(seat) = self.seats.get_mut(seat_id) {
                        seat.resources.grabbed_device_count = status.grabbed.len();
                        seat.resources.uinput_devices = status.uinput_devices;
                    }
                    return self.apply_input_targeting(seat_id, &uinput_devices);
                }
                Ok(_) => {
                    // The helper lost the assignment. Do not keep claiming
                    // isolation; fall through to a fresh grab.
                    self.clear_input_targeting(seat_id);
                    if let Some(seat) = self.seats.get_mut(seat_id) {
                        seat.resources.input_isolation_applied = false;
                        seat.resources.grabbed_device_count = 0;
                        seat.resources.uinput_devices.clear();
                    }
                    self.record_resource_error(
                        seat_id,
                        ResourceKind::InputIsolation,
                        ResourceAction::Apply,
                        "helper no longer reports the seat's grab; re-applying",
                    );
                }
                Err(error) => {
                    self.clear_input_targeting(seat_id);
                    if let Some(seat) = self.seats.get_mut(seat_id) {
                        seat.resources.input_isolation_applied = false;
                        seat.resources.grabbed_device_count = 0;
                        seat.resources.uinput_devices.clear();
                    }
                    self.record_resource_error(
                        seat_id,
                        ResourceKind::InputIsolation,
                        ResourceAction::Apply,
                        &format!("checking helper assignment: {error}"),
                    );
                }
            }
        }

        let assignment = self
            .input_helper
            .as_mut()
            .expect("helper checked above")
            .grab(seat_id, &devices);
        let assignment = match assignment {
            Ok(assignment) => assignment,
            Err(error) => {
                self.record_resource_error(
                    seat_id,
                    ResourceKind::InputIsolation,
                    ResourceAction::Apply,
                    &error.to_string(),
                );
                return Err(error);
            }
        };
        if assignment.grabbed.len() != devices.len() {
            let error = MultiseatError::InputIsolation(format!(
                "helper confirmed {}/{} devices for seat {seat_id}; refusing partial isolation",
                assignment.grabbed.len(),
                devices.len()
            ));
            self.record_resource_error(
                seat_id,
                ResourceKind::InputIsolation,
                ResourceAction::Apply,
                &error.to_string(),
            );
            return Err(error);
        }

        if let Some(seat) = self.seats.get_mut(seat_id) {
            seat.resources.input_isolation_applied = true;
            seat.resources.input_isolation_released = false;
            seat.resources.grabbed_device_count = assignment.grabbed.len();
            seat.resources.uinput_devices = assignment.uinput_devices;
        }
        let uinput_devices = self
            .seats
            .get(seat_id)
            .map(|seat| seat.resources.uinput_devices.clone())
            .unwrap_or_default();
        self.apply_input_targeting(seat_id, &uinput_devices)
    }

    /// Ensure the seat's audio sink when a router is installed.
    ///
    /// Audio is intentionally non-fatal: a missing `pactl` or audio server is
    /// recorded as a typed `Audio` apply error and the seat still starts. No
    /// sink is reported as created unless the router returned success.
    fn ensure_seat_audio(&mut self, seat_id: &str) {
        let Some(spec) = self.seats.get(seat_id).map(|seat| seat.audio_spec.clone()) else {
            return;
        };
        if self
            .seats
            .get(seat_id)
            .is_some_and(|seat| seat.resources.audio_applied)
        {
            return;
        }
        let result = match self.audio.as_ref() {
            Some(router) => router.ensure_sink(&spec),
            None => return,
        };
        match result {
            Ok(_) => {
                if let Some(seat) = self.seats.get_mut(seat_id) {
                    seat.resources.audio_applied = true;
                    seat.resources.audio_released = false;
                }
            }
            Err(error) => self.record_resource_error(
                seat_id,
                ResourceKind::Audio,
                ResourceAction::Apply,
                &error.to_string(),
            ),
        }
    }

    fn apply_ownership(&mut self, seat_id: &str) -> Result<(), MultiseatError> {
        let ownership = {
            let Some(seat) = self.seats.get(seat_id) else {
                return Err(MultiseatError::UnknownSeat(seat_id.to_owned()));
            };
            if seat.resources.ownership_applied {
                return Ok(());
            }
            seat.ownership.clone()
        };
        let Some(ownership) = ownership else {
            return Ok(());
        };
        if self.display_is_nested(seat_id) {
            let kind = if self
                .seats
                .get(seat_id)
                .is_some_and(|seat| seat.display_status.display_backend.is_wayland())
            {
                "a private headless Wayland compositor with no physical output"
            } else {
                "a private X root with no physical output"
            };
            self.push_display_note(
                seat_id,
                SeatDisplayNote::new(
                    SeatDisplayNoteKind::OwnershipUnsupported,
                    format!(
                        "xrandr ownership of output {} is not applied: seat {seat_id} draws on \
                         {kind}",
                        ownership.output
                    ),
                ),
            );
            return Ok(());
        }

        let result = match self.ownership.as_mut() {
            Some(controller) => controller.apply(&ownership),
            None => Err(MultiseatError::Unsupported(format!(
                "seat {seat_id} requested display ownership but no xrandr ownership controller is configured"
            ))),
        };

        match result {
            Ok(()) => {
                if let Some(seat) = self.seats.get_mut(seat_id) {
                    seat.resources.ownership_applied = true;
                    seat.resources.ownership_released = false;
                }
                Ok(())
            }
            Err(error) => {
                self.record_resource_error(
                    seat_id,
                    ResourceKind::Ownership,
                    ResourceAction::Apply,
                    &error.to_string(),
                );
                Err(error)
            }
        }
    }

    fn apply_routing(&mut self, seat_id: &str) -> Result<(), MultiseatError> {
        let (route, group) = {
            let Some(seat) = self.seats.get(seat_id) else {
                return Err(MultiseatError::UnknownSeat(seat_id.to_owned()));
            };
            if seat.resources.routing_applied {
                return Ok(());
            }
            (seat.input_route.clone(), seat.input.clone())
        };
        let Some(route) = route else {
            return Ok(());
        };
        let Some(group) = group else {
            let error = MultiseatError::InputRouting(format!(
                "seat {seat_id} has an input route but no assigned input group"
            ));
            self.record_resource_error(
                seat_id,
                ResourceKind::Routing,
                ResourceAction::Apply,
                &error.to_string(),
            );
            return Err(error);
        };
        if route.strategy == InputRoutingStrategy::XInputMapToOutput
            && self.display_is_nested(seat_id)
        {
            let kind = if self
                .seats
                .get(seat_id)
                .is_some_and(|seat| seat.display_status.display_backend.is_wayland())
            {
                "the private Wayland compositor has no X pointer; use --input-isolation helper \
                 for exclusive input"
            } else {
                "the private X server has no physical pointer; use --input-isolation helper for \
                 exclusive input"
            };
            self.push_display_note(
                seat_id,
                SeatDisplayNote::new(
                    SeatDisplayNoteKind::XinputRoutingUnsupported,
                    format!(
                        "xinput map-to-output for {} is not applied: {kind}",
                        route.display_output
                    ),
                ),
            );
            return Ok(());
        }

        let target = match InputTarget::new(&route.seat_id, &route.display_output, &route.x_screen)
        {
            Ok(target) => target,
            Err(error) => {
                self.record_resource_error(
                    seat_id,
                    ResourceKind::Routing,
                    ResourceAction::Apply,
                    &error.to_string(),
                );
                return Err(error);
            }
        };

        let result = match route.strategy {
            InputRoutingStrategy::XInputMapToOutput => match self.routing.as_mut() {
                Some(router) => router.assign(&group, &target).map(|_| ()),
                None => Err(MultiseatError::Unsupported(format!(
                    "seat {seat_id} requested xinput routing but no input router is configured"
                ))),
            },
            InputRoutingStrategy::EvdevGrab => match self.evdev_routing.as_mut() {
                Some(router) => router.assign(&group, &target).map(|_| ()),
                None => Err(MultiseatError::Unsupported(format!(
                    "seat {seat_id} requested evdev routing but no evdev backend is configured; \
                     the default backend is unsupported"
                ))),
            },
            InputRoutingStrategy::InputIsolation => Err(MultiseatError::Unsupported(format!(
                "seat {seat_id} requires the privileged input-isolation helper; \
                 enable SupervisorOptions::input_isolation"
            ))),
        };

        match result {
            Ok(()) => {
                if let Some(seat) = self.seats.get_mut(seat_id) {
                    seat.resources.routing_applied = true;
                    seat.resources.routing_released = false;
                }
                Ok(())
            }
            Err(error) => {
                self.record_resource_error(
                    seat_id,
                    ResourceKind::Routing,
                    ResourceAction::Apply,
                    &error.to_string(),
                );
                Err(error)
            }
        }
    }

    /// Release both resources for one seat, best effort.
    ///
    /// Routing is released before ownership (reverse of apply order). A
    /// release failure leaves the applied flag set, records a typed error, and
    /// does not hide the seat's lifecycle state.
    fn release_seat_resources(&mut self, seat_id: &str) {
        let Some((
            routing_applied,
            ownership_applied,
            audio_applied,
            isolation_applied,
            targeting_applied,
            strategy,
            audio_sink,
        )) = self.seats.get(seat_id).map(|seat| {
            (
                seat.resources.routing_applied,
                seat.resources.ownership_applied,
                seat.resources.audio_applied,
                seat.resources.input_isolation_applied,
                seat.resources.input_targeting_applied,
                seat.input_route
                    .as_ref()
                    .map(|route| route.strategy)
                    .unwrap_or(InputRoutingStrategy::XInputMapToOutput),
                seat.audio_spec.name.clone(),
            )
        })
        else {
            return;
        };

        // Display targeting is released before the helper grab (reverse of
        // apply order). Synthetic Wayland input is closed first: it belongs to
        // the display socket and must never outlive/reconnect to a replacement
        // compositor.
        self.clear_seat_input(seat_id);

        // Re-enabling the other seats' devices while the private
        // server still exists keeps the shared XInput state honest. A failure
        // leaves the applied flag set for a later retry.
        if targeting_applied {
            let result = match self.input_targeter.as_mut() {
                Some(targeter) => targeter.release(seat_id).map(|_| ()),
                None => Err(MultiseatError::Unsupported(format!(
                    "seat {seat_id} has applied display input targeting but no targeter is \
                     configured"
                ))),
            };
            match result {
                Ok(()) => {
                    if let Some(seat) = self.seats.get_mut(seat_id) {
                        seat.resources.input_targeting_applied = false;
                        seat.resources.input_targeting_released = true;
                        seat.resources.targeted_devices.clear();
                        seat.resources.disabled_devices.clear();
                    }
                }
                Err(error) => self.record_resource_error(
                    seat_id,
                    ResourceKind::InputTargeting,
                    ResourceAction::Release,
                    &error.to_string(),
                ),
            }
        }

        // Isolation is released before routing/ownership (reverse of apply
        // order). A failure leaves the applied flag set for a later retry.
        if isolation_applied {
            let result = match self.input_helper.as_mut() {
                Some(helper) => helper.release(seat_id).map(|_| ()),
                None => Err(MultiseatError::Unsupported(format!(
                    "seat {seat_id} has applied input isolation but no helper is configured"
                ))),
            };
            match result {
                Ok(()) => {
                    if let Some(seat) = self.seats.get_mut(seat_id) {
                        seat.resources.input_isolation_applied = false;
                        seat.resources.input_isolation_released = true;
                        seat.resources.grabbed_device_count = 0;
                        seat.resources.uinput_devices.clear();
                    }
                }
                Err(error) => self.record_resource_error(
                    seat_id,
                    ResourceKind::InputIsolation,
                    ResourceAction::Release,
                    &error.to_string(),
                ),
            }
        }

        if routing_applied {
            let result = match strategy {
                InputRoutingStrategy::XInputMapToOutput => match self.routing.as_mut() {
                    Some(router) => router.release(seat_id).map(|_| ()),
                    None => Err(MultiseatError::Unsupported(format!(
                        "seat {seat_id} has an applied xinput route but no input router is configured"
                    ))),
                },
                InputRoutingStrategy::EvdevGrab => match self.evdev_routing.as_mut() {
                    Some(router) => router.release(seat_id).map(|_| ()),
                    None => Err(MultiseatError::Unsupported(format!(
                        "seat {seat_id} has an applied evdev route but no evdev backend is configured"
                    ))),
                },
                InputRoutingStrategy::InputIsolation => {
                    // A helper-owned seat never has routing_applied, so this
                    // arm is only reachable if a caller mixed strategies.
                    Err(MultiseatError::Unsupported(format!(
                        "seat {seat_id} has input-isolation routing; release it through \
                         SupervisorOptions::input_isolation"
                    )))
                }
            };
            match result {
                Ok(()) => {
                    if let Some(seat) = self.seats.get_mut(seat_id) {
                        seat.resources.routing_applied = false;
                        seat.resources.routing_released = true;
                    }
                }
                Err(error) => self.record_resource_error(
                    seat_id,
                    ResourceKind::Routing,
                    ResourceAction::Release,
                    &error.to_string(),
                ),
            }
        }

        if ownership_applied {
            let result = match self.ownership.as_mut() {
                Some(controller) => controller.release(seat_id),
                None => Err(MultiseatError::Unsupported(format!(
                    "seat {seat_id} has applied display ownership but no ownership controller is configured"
                ))),
            };
            match result {
                Ok(()) => {
                    if let Some(seat) = self.seats.get_mut(seat_id) {
                        seat.resources.ownership_applied = false;
                        seat.resources.ownership_released = true;
                    }
                }
                Err(error) => self.record_resource_error(
                    seat_id,
                    ResourceKind::Ownership,
                    ResourceAction::Release,
                    &error.to_string(),
                ),
            }
        }

        if audio_applied {
            let result = match self.audio.as_ref() {
                Some(router) => router.remove_sink(&audio_sink),
                None => Err(MultiseatError::Unsupported(format!(
                    "seat {seat_id} has an applied audio sink but no audio router is configured"
                ))),
            };
            match result {
                Ok(()) => {
                    if let Some(seat) = self.seats.get_mut(seat_id) {
                        seat.resources.audio_applied = false;
                        seat.resources.audio_released = true;
                    }
                }
                Err(error) => self.record_resource_error(
                    seat_id,
                    ResourceKind::Audio,
                    ResourceAction::Release,
                    &error.to_string(),
                ),
            }
        }

        // The DRM lease is released after the other resources (reverse of apply
        // order). A revoke failure keeps the handle and applied flag for a later
        // stop/Drop retry.
        if self
            .seats
            .get(seat_id)
            .is_some_and(|seat| seat.resources.drm_lease_applied)
        {
            if let Err(error) = self.release_seat_lease(seat_id) {
                self.record_resource_error(
                    seat_id,
                    ResourceKind::DrmLease,
                    ResourceAction::Release,
                    &error.to_string(),
                );
            }
        }

        // Do not keep a privileged helper alive with no confirmed grabs. It is
        // started again lazily on the next seat start.
        let any_isolation = self
            .seats
            .values()
            .any(|seat| seat.resources.input_isolation_applied);
        if self.options.input_isolation && !any_isolation {
            if let Some(helper) = self.input_helper.as_mut() {
                helper.shutdown();
            }
        }
    }

    /// Release every applied resource in reverse seat order.
    ///
    /// Used when a later seat cannot reserve its resources: earlier seats are
    /// released before the start attempt fails, so a partially applied seat set
    /// is not left behind. Child processes are not touched here.
    fn rollback_applied_resources(&mut self) {
        let mut seat_ids: Vec<String> = self
            .seats
            .iter()
            .filter(|(_, seat)| {
                seat.resources.ownership_applied
                    || seat.resources.routing_applied
                    || seat.resources.input_isolation_applied
                    || seat.resources.input_targeting_applied
                    || seat.resources.audio_applied
                    || seat.resources.drm_lease_applied
            })
            .map(|(seat_id, _)| seat_id.clone())
            .collect();
        seat_ids.reverse();
        for seat_id in seat_ids {
            self.release_seat_resources(&seat_id);
        }

        // A private X server with no child cannot be in use. Release it in
        // reverse seat order so a failed start does not leak a display. A seat
        // that still owns a child keeps its display until `stop`/`Drop`.
        let mut display_ids: Vec<String> = self
            .seats
            .iter()
            .filter(|(_, seat)| seat.display.is_some() && seat.child.is_none())
            .map(|(seat_id, _)| seat_id.clone())
            .collect();
        display_ids.reverse();
        for seat_id in display_ids {
            let _ = self.release_seat_display(&seat_id);
        }
    }

    fn record_resource_error(
        &mut self,
        seat_id: &str,
        kind: ResourceKind,
        action: ResourceAction,
        message: &str,
    ) {
        if let Some(seat) = self.seats.get_mut(seat_id) {
            seat.resources.push_error(kind, action, message);
        }
    }

    /// Poll children and start any restart whose backoff has elapsed.
    pub fn tick(&mut self) {
        self.tick_at(Instant::now());
    }

    /// Poll children and start due restarts using an explicit clock.
    pub fn tick_at(&mut self, now: Instant) {
        let seat_ids: Vec<String> = self.seats.keys().cloned().collect();

        for seat_id in seat_ids {
            let mut display_exit = None;
            let mut display_poll_error = None;
            {
                let Some(seat) = self.seats.get_mut(&seat_id) else {
                    continue;
                };
                if let Some(display) = seat.display.as_mut() {
                    match display.try_wait() {
                        Ok(Some(exit)) => display_exit = Some(exit),
                        Ok(None) => {}
                        Err(error) => display_poll_error = Some(error.to_string()),
                    }
                }
            }

            if display_exit.is_some() || display_poll_error.is_some() {
                let (display_name, backend) = self
                    .seats
                    .get(&seat_id)
                    .map(|seat| {
                        (
                            seat.display_status
                                .display
                                .clone()
                                .unwrap_or_else(|| "(unknown)".to_owned()),
                            seat.display_status.display_backend,
                        )
                    })
                    .unwrap_or_else(|| ("(unknown)".to_owned(), self.options.seat_display.backend));
                let detail = match display_exit {
                    Some(exit) => exit.to_string(),
                    None => display_poll_error.unwrap_or_else(|| "poll failed".to_owned()),
                };
                let error = MultiseatError::SeatDisplayCrashed {
                    display: display_name.clone(),
                    backend: backend.as_str().to_owned(),
                    detail,
                };
                // The private server is gone; drop targeting state so the
                // restart re-applies it to the replacement display instead of
                // trusting a stale `input_targeting_applied` flag.
                self.clear_input_targeting(&seat_id);
                self.clear_seat_input(&seat_id);
                let seat = self.seats.get_mut(&seat_id).expect("tracked seat");
                let mut cleanup_failure = None;
                let mut display_released = false;
                if let Some(mut display) = seat.display.take() {
                    match display.terminate() {
                        Ok(()) => display_released = true,
                        Err(cleanup) => {
                            seat.display = Some(display);
                            cleanup_failure = Some(format!("stopping display: {cleanup}"));
                        }
                    }
                }
                if cleanup_failure.is_none() {
                    if let Some(mut child) = seat.child.take() {
                        if let Err(cleanup) = child.terminate() {
                            seat.child = Some(child);
                            cleanup_failure = Some(format!("stopping node: {cleanup}"));
                        }
                    }
                }
                seat.display_status.display_ready = false;
                seat.display_status.display_error = Some(SeatDisplayError::from_error(&error));
                if let Some(detail) = cleanup_failure {
                    // Keep every handle so a later stop can retry; do not
                    // restart over a process that could not be reaped.
                    if display_released {
                        seat.display_status.display = None;
                        seat.display_status.display_pid = None;
                    }
                    seat.state = SeatState::Failed;
                    seat.last_error = Some(format!(
                        "display for {seat_id} disappeared ({error}); cleanup failed: {detail}"
                    ));
                    seat.next_restart = None;
                    continue;
                }
                seat.pid = None;
                seat.started_at = None;
                seat.display_status.display = None;
                seat.display_status.display_pid = None;
                seat.last_error = Some(format!("display for {seat_id} disappeared: {error}"));
                seat.message = None;
                schedule_restart(&self.options, seat, now);
                continue;
            }

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
                // A nested display must be ready before the node can use it.
                // A failed display spawn backs off without touching resources.
                if let Err(error) = self.ensure_seat_display(&seat_id) {
                    let text = error.to_string();
                    let seat = self.seats.get_mut(&seat_id).expect("tracked seat");
                    seat.last_error = Some(text);
                    seat.message = None;
                    schedule_restart(&self.options, seat, now);
                    continue;
                }
                if let Err(error) = self.ensure_seat_lease(&seat_id) {
                    let text = error.to_string();
                    let seat = self.seats.get_mut(&seat_id).expect("tracked seat");
                    seat.last_error = Some(text);
                    seat.message = None;
                    schedule_restart(&self.options, seat, now);
                    continue;
                }
                // A restart re-applies resources only when they were released
                // by a previous failed attempt; an already-applied resource is
                // left in place. A failed re-apply is reported and backed off.
                if let Err(error) = self.apply_seat_resources(&seat_id) {
                    let text = error.to_string();
                    let seat = self.seats.get_mut(&seat_id).expect("tracked seat");
                    seat.last_error = Some(text.clone());
                    seat.message = None;
                    schedule_restart(&self.options, seat, now);
                    continue;
                }
                // A failed restart schedules the next backoff internally.
                let _ = self.launch_once(&seat_id, now);
            }
        }

        if self.options.input_isolation {
            self.reconcile_input_isolation(now);
        }
    }

    /// Detect a helper that restarted or lost a seat's grab and repair it.
    ///
    /// Isolation is never left claimed after a helper restart. If a fresh grab
    /// also fails, the unisolated child is stopped and scheduled for a restart
    /// rather than left running with a false isolation claim.
    fn reconcile_input_isolation(&mut self, now: Instant) {
        let applied: Vec<String> = self
            .seats
            .iter()
            .filter(|(_, seat)| seat.resources.input_isolation_applied)
            .map(|(seat_id, _)| seat_id.clone())
            .collect();
        if applied.is_empty() {
            return;
        }

        for seat_id in applied {
            let expected: Vec<String> = self
                .seats
                .get(&seat_id)
                .and_then(|seat| seat.input.as_ref())
                .map(|group| {
                    helper_devices(group)
                        .into_iter()
                        .map(|device| device.path)
                        .collect()
                })
                .unwrap_or_default();
            let helper_status = match self.input_helper.as_mut() {
                Some(helper) => Some(helper.seat_status(&seat_id)),
                None => None,
            };
            let intact = match helper_status {
                Some(Ok(Some(status))) => status.grabbed == expected,
                Some(Ok(None)) => false,
                Some(Err(error)) => {
                    self.record_resource_error(
                        &seat_id,
                        ResourceKind::InputIsolation,
                        ResourceAction::Apply,
                        &format!("checking helper assignment: {error}"),
                    );
                    false
                }
                None => false,
            };
            if intact {
                continue;
            }

            if let Some(seat) = self.seats.get_mut(&seat_id) {
                seat.resources.input_isolation_applied = false;
                seat.resources.grabbed_device_count = 0;
                seat.resources.uinput_devices.clear();
            }
            // A restarted helper creates new uinput devices, so any previous
            // display restriction must be re-applied rather than trusted.
            self.clear_input_targeting(&seat_id);
            self.record_resource_error(
                &seat_id,
                ResourceKind::InputIsolation,
                ResourceAction::Apply,
                "helper lost the seat's grab; re-applying isolation",
            );

            if let Err(error) = self.apply_input_isolation(&seat_id) {
                let stop_child = self
                    .seats
                    .get(&seat_id)
                    .is_some_and(|seat| seat.child.is_some());
                if !stop_child {
                    continue;
                }
                let seat = self.seats.get_mut(&seat_id).expect("tracked seat");
                if let Some(mut child) = seat.child.take() {
                    if let Err(cleanup) = child.terminate() {
                        seat.child = Some(child);
                        seat.state = SeatState::Failed;
                        seat.last_error = Some(format!(
                            "input isolation lost ({error}); cleanup failed: {cleanup}"
                        ));
                        seat.next_restart = None;
                    } else {
                        seat.pid = None;
                        seat.started_at = None;
                        seat.last_error = Some(format!(
                            "input isolation lost and could not be restored: {error}"
                        ));
                        schedule_restart(&self.options, seat, now);
                    }
                }
            }
        }
    }

    /// Stop a seat and forget it. Unknown ids are an error.
    ///
    /// If a best-effort resource release fails, the seat is retained in
    /// `stopped` state with the typed error in `resources.errors` so a later
    /// `remove` can retry the release. Forgetting the seat would otherwise drop
    /// the only handle to still-applied ownership/routing.
    pub fn remove(&mut self, seat_id: &str) -> Result<SeatRuntimeStatus, MultiseatError> {
        let status = self.stop(seat_id)?;
        if status.resources.ownership_applied
            || status.resources.routing_applied
            || status.resources.input_isolation_applied
            || status.resources.input_targeting_applied
            || status.resources.audio_applied
            || status.resources.drm_lease_applied
            || status.display.display.is_some()
        {
            return Ok(status);
        }
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

        // An active DRM lease is exported by fd number. Clear FD_CLOEXEC only
        // for the spawn call so the child inherits the lessee fd, then restore
        // it immediately. The supervisor is single-threaded.
        let lease_fd = self
            .seats
            .get(seat_id)
            .and_then(|seat| seat.lease.as_ref())
            .and_then(|lease| lease.fd());
        let spawn = if lease_fd.is_some() {
            match self.set_lease_inheritable(seat_id, true) {
                Ok(()) => {
                    let result = self.launcher.spawn(&spec);
                    let _ = self.set_lease_inheritable(seat_id, false);
                    result
                }
                Err(error) => Err(MultiseatError::DrmLease(error.to_string())),
            }
        } else {
            self.launcher.spawn(&spec)
        };

        match spawn {
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
            let backend = self.options.seat_display.backend;
            let seat = self
                .seats
                .entry(seat_id.clone())
                .or_insert_with(|| ManagedSeat::new(plan));
            seat.launch = spec;
            seat.audio_spec = seat_audio_spec(plan);
            seat.ownership = plan.ownership.clone();
            seat.input = plan.input.clone();
            seat.input_route = plan.input_route.clone();
            seat.parent_display = plan_parent_display(plan);
            // A fresh start after stop/release begins a new resource cycle;
            // keep applied state when this is a retry of the same reservation.
            if !seat.resources.ownership_applied
                && !seat.resources.routing_applied
                && !seat.resources.input_isolation_applied
                && !seat.resources.input_targeting_applied
                && !seat.resources.input_injection_applied
                && !seat.resources.drm_lease_applied
            {
                seat.resources = SeatResourceStatus::default();
            }
            if seat.display.is_none() {
                seat.display_status = SeatDisplayStatus::for_backend(backend);
            }
            seat.state = SeatState::Starting;
            seat.next_restart = None;
            seat.failures = 0;
            seat.last_error = None;
            seat.last_exit = None;
            seat.message = None;

            if self.options.dry_run {
                seat.state = SeatState::Planned;
                seat.pid = None;
                let mut message = format!(
                    "dry-run: would launch {} ({})",
                    seat.launch.program, seat.launch.http_port
                );
                if backend.is_nested() {
                    message.push_str(&format!(
                        "; would start {backend} display {} for the seat",
                        self.options.seat_display.size
                    ));
                }
                if self.options.apply_ownership && seat.ownership.is_some() {
                    if backend.is_nested() {
                        message.push_str("; xrandr ownership has no physical output to own");
                    } else {
                        message.push_str("; would apply xrandr ownership");
                    }
                }
                if self.options.apply_routing && seat.input_route.is_some() {
                    if backend.is_nested() {
                        message.push_str("; xinput routing has no physical pointer to map");
                    } else {
                        message.push_str("; would apply input routing");
                    }
                }
                if self.options.input_isolation && seat.input.is_some() {
                    message.push_str("; would grab input through the seat-input helper");
                    if backend.is_nested() {
                        message.push_str(&format!(
                            "; would restrict the private display to seat uinput devices \
                             (policy={})",
                            self.options.input_targeting
                        ));
                    }
                }
                if self.options.seat_input.is_wayland() {
                    message.push_str(
                        "; would connect a per-seat Wayland virtual keyboard/pointer to the \
                         private compositor socket",
                    );
                }
                if let Some(request) = self.options.drm_leases.get(&seat_id) {
                    message.push_str(&format!(
                        "; would acquire a DRM lease for {}:{} (lease-only; no scanout or \
                         compositor isolation)",
                        request.card.display_name(),
                        request.connector
                    ));
                }
                seat.message = Some(message);
                return Ok(seat.status(now));
            }
        }

        // A private X server starts before any seat resource or node process.
        // A failure here never claims a display and never spawns the node.
        if let Err(error) = self.ensure_seat_display(&seat_id) {
            let text = error.to_string();
            let seat = self.seats.get_mut(&seat_id).expect("tracked seat");
            seat.child = None;
            seat.pid = None;
            seat.started_at = None;
            seat.last_error = Some(text);
            seat.message = None;
            schedule_restart(&self.options, seat, now);
            return Err(error);
        }

        // The opt-in DRM lease is acquired after a private display exists and
        // before any resource or node spawn. A lease failure never leaves a
        // display or earlier seat resource behind.
        if let Err(error) = self.ensure_seat_lease(&seat_id) {
            self.release_seat_resources(&seat_id);
            let _ = self.release_seat_display(&seat_id);
            self.rollback_applied_resources();
            let text = error.to_string();
            let seat = self.seats.get_mut(&seat_id).expect("tracked seat");
            seat.child = None;
            seat.pid = None;
            seat.started_at = None;
            seat.state = SeatState::Failed;
            seat.next_restart = None;
            seat.last_error = Some(text);
            seat.message = None;
            return Err(error);
        }

        // Opt-in resources are reserved before the child exists. An apply
        // failure never spawns this seat, and earlier seats are rolled back so
        // a partially applied seat set is not left behind.
        if self.options.apply_ownership
            || self.options.apply_routing
            || self.options.input_isolation
        {
            if let Err(error) = self.apply_seat_resources(&seat_id) {
                self.release_seat_resources(&seat_id);
                let _ = self.release_seat_display(&seat_id);
                self.rollback_applied_resources();
                let text = error.to_string();
                let seat = self.seats.get_mut(&seat_id).expect("tracked seat");
                seat.child = None;
                seat.pid = None;
                seat.started_at = None;
                seat.state = SeatState::Failed;
                seat.next_restart = None;
                seat.last_error = Some(text);
                seat.message = None;
                return Err(error);
            }
        }

        // The audio sink is optional and non-fatal: a typed ensure failure is
        // recorded in status but never prevents the node from starting.
        self.ensure_seat_audio(&seat_id);

        // A start attempt owns the spawn/readiness sequence. On failure the
        // seat is retained in `failed` state with a scheduled restart so a
        // caller can either tick or retry explicitly.
        match self.launch_once(&seat_id, now) {
            Ok(()) => self
                .seats
                .get(&seat_id)
                .map(|seat| seat.status(now))
                .ok_or_else(|| MultiseatError::UnknownSeat(seat_id)),
            Err(error) => {
                // A child that never became ready must not keep ownership or
                // pointer mappings reserved. If cleanup failed, the handle is
                // retained and the resource stays reserved until a later stop
                // reaps the child.
                let child_retained = self
                    .seats
                    .get(&seat_id)
                    .is_some_and(|seat| seat.child.is_some());
                if !child_retained {
                    self.release_seat_resources(&seat_id);
                    let _ = self.release_seat_display(&seat_id);
                }
                Err(error)
            }
        }
    }
}

impl Drop for ProcessSeatSupervisor {
    fn drop(&mut self) {
        for seat in self.seats.values_mut() {
            if let Some(mut child) = seat.child.take() {
                let _ = child.terminate();
            }
        }
        // Best-effort shutdown release; errors are dropped with the
        // supervisor. `stop`/`stop_all` surface them in status first.
        self.rollback_applied_resources();
        if let Some(helper) = self.input_helper.as_mut() {
            helper.shutdown();
        }
    }
}

impl SeatSupervisor for ProcessSeatSupervisor {
    fn start(&mut self, plan: &SeatPlan) -> Result<SeatRuntimeStatus, MultiseatError> {
        self.start_at(plan, Instant::now())
    }

    fn stop(&mut self, seat_id: &str) -> Result<SeatRuntimeStatus, MultiseatError> {
        let now = Instant::now();
        if !self.seats.contains_key(seat_id) {
            return Err(MultiseatError::UnknownSeat(seat_id.to_owned()));
        }

        {
            let seat = self.seats.get_mut(seat_id).expect("tracked seat");
            if let Some(mut child) = seat.child.take() {
                if let Err(error) = child.terminate() {
                    let text = error.to_string();
                    // Keep the handle so Drop or a later stop can retry
                    // cleanup. Resources stay reserved while the child may
                    // still be alive.
                    seat.child = Some(child);
                    seat.state = SeatState::Failed;
                    seat.last_error = Some(text.clone());
                    seat.next_restart = None;
                    return Err(MultiseatError::Supervisor(text));
                }
            }
        }

        // Child is gone: release routing before ownership. Failures are
        // best-effort and remain visible in `resources.errors`.
        self.release_seat_resources(seat_id);

        // The private X server is stopped only after the seat's child is gone
        // and its display-dependent resource releases have run.
        if let Err(error) = self.release_seat_display(seat_id) {
            let text = error.to_string();
            let seat = self.seats.get_mut(seat_id).expect("tracked seat");
            seat.state = SeatState::Failed;
            seat.last_error = Some(text.clone());
            seat.next_restart = None;
            return Err(MultiseatError::Supervisor(text));
        }

        let seat = self.seats.get_mut(seat_id).expect("tracked seat");
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

fn seat_audio_spec(plan: &SeatPlan) -> SeatAudioSpec {
    SeatAudioSpec::for_seat(
        &plan.config.id,
        plan.config.audio_sink.clone(),
        plan.config.profile.defaults().audio_channels,
    )
}

fn display_launcher_for(options: &SupervisorOptions) -> Option<Box<dyn SeatDisplayLauncher>> {
    if !options.seat_display.backend.is_nested() {
        return None;
    }
    Some(Box::new(
        crate::seat_display::ProcessSeatDisplayLauncher::new(options.seat_display.clone()),
    ))
}

fn plan_parent_display(plan: &SeatPlan) -> Option<String> {
    match &plan.config.capture {
        crate::seat::CapturePlan::X11 { display, .. } => {
            let display = display.trim();
            (!display.is_empty()).then(|| display.to_owned())
        }
        crate::seat::CapturePlan::PipeWire { .. } => None,
    }
}

/// Point the node launch specification at the private Wayland compositor.
///
/// `WAYLAND_DISPLAY` and `XDG_RUNTIME_DIR` are exported to the child. The host
/// `DISPLAY`/`XAUTHORITY` are removed so a headless Weston seat cannot fall back
/// to the operator's X session. A PipeWire capture target is left unchanged;
/// the planned X11 capture target is not rewritten to a Wayland name (the node
/// capture integration for Wayland remains a documented gap).
fn apply_wayland_to_launch(launch: &mut NodeLaunchSpec, wayland: &SeatWaylandEnv) {
    launch.env.insert(
        "WAYLAND_DISPLAY".to_owned(),
        wayland.wayland_display.clone(),
    );
    launch.env.insert(
        "XDG_RUNTIME_DIR".to_owned(),
        wayland.xdg_runtime_dir.display().to_string(),
    );
    launch.env.remove("DISPLAY");
    launch.env.remove("XAUTHORITY");
}

/// Point the node launch specification at the private display.
///
/// `DISPLAY`/`XAUTHORITY` are exported to the child and the X11 capture flags
/// are rewritten to the private screen at offset zero. A PipeWire seat keeps
/// its capture target; only the application display changes.
fn apply_display_to_launch(
    launch: &mut NodeLaunchSpec,
    display: &str,
    xauthority: Option<&std::path::Path>,
    size: crate::seat_display::SeatDisplaySize,
) {
    launch.env.insert("DISPLAY".to_owned(), display.to_owned());
    match xauthority {
        Some(path) => {
            launch
                .env
                .insert("XAUTHORITY".to_owned(), path.display().to_string());
        }
        None => {
            launch.env.remove("XAUTHORITY");
        }
    }
    if launch.args.iter().any(|arg| arg == "--x11-display") {
        set_arg_value(&mut launch.args, "--x11-display", display);
        set_arg_value(&mut launch.args, "--x11-offset", "0,0");
        set_arg_value(&mut launch.args, "--x11-width", &size.width.to_string());
        set_arg_value(&mut launch.args, "--x11-height", &size.height.to_string());
    }
}

fn set_arg_value(args: &mut Vec<String>, flag: &str, value: &str) {
    match args.iter().position(|arg| arg == flag) {
        Some(index) if index + 1 < args.len() => args[index + 1] = value.to_owned(),
        Some(_) => args.push(value.to_owned()),
        None => {
            args.push(flag.to_owned());
            args.push(value.to_owned());
        }
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
