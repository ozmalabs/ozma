// SPDX-License-Identifier: AGPL-3.0-only
//! Test-only fakes.
//!
//! These types never touch `/dev`, sysfs, X11, PipeWire, or processes. They are
//! provided so planner and supervisor behavior can be tested deterministically.
//! Production code must never select a fake from detection.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};

use crate::audio::{SeatAudioRouter, SeatAudioSpec, SeatAudioState};
use crate::display::{DisplayBackend, DisplayOutput};
use crate::error::MultiseatError;
use crate::hotplug::{HotplugEvent, HotplugMonitor};
use crate::input::{InputBackend, InputDevice};
use crate::input_isolation::{
    default_uinput_name, is_uinput_name, HelperBackend, HelperBackendKind, HelperDevice,
    HelperError, HelperErrorCode, HelperResponse, HelperSeatStatus, InputIsolationAssignment,
    InputIsolationRelease, SeatInputHelper, HELPER_PROTOCOL_VERSION,
};
use crate::input_targeting::{
    SeatInputTargetRequest, SeatInputTargeter, SeatInputTargeting, SeatInputTargetingRelease,
};
use crate::routing::{
    CommandOutput, CommandRunner, DeviceNameResolver, EvdevGrabBackend, EvdevGrabRequest,
    RouteCommand,
};
use crate::seat::{SeatPlan, SeatRuntimeStatus, SeatState};
use crate::seat_display::{
    wayland_display_name, DisplayExit, SeatDisplayBackend, SeatDisplayHandle, SeatDisplayLauncher,
    SeatDisplayRequest, SeatWaylandEnv,
};
use crate::supervisor::{
    ChildExit, ChildHandle, NodeLaunchSpec, NodeLauncher, ReadinessProbe, SeatSupervisor,
};

/// Deterministic in-memory display backend.
#[derive(Debug)]
pub struct FakeDisplayBackend {
    displays: Vec<DisplayOutput>,
    created: RefCell<Vec<DisplayOutput>>,
    virtual_supported: bool,
    next_virtual_index: Cell<u32>,
    enumerate_failure: Option<String>,
}

impl FakeDisplayBackend {
    /// Create a fake with the supplied outputs and no virtual-display support.
    pub fn new(displays: Vec<DisplayOutput>) -> Self {
        Self {
            displays,
            created: RefCell::new(Vec::new()),
            virtual_supported: false,
            next_virtual_index: Cell::new(100),
            enumerate_failure: None,
        }
    }

    /// Allow [`DisplayBackend::create_virtual`] to return a deterministic
    /// virtual output. Created outputs live only in this fake.
    pub fn with_virtual_displays(mut self) -> Self {
        self.virtual_supported = true;
        self
    }

    /// Make enumeration fail with a typed backend error.
    pub fn failing(mut self, reason: impl Into<String>) -> Self {
        self.enumerate_failure = Some(reason.into());
        self
    }

    /// Virtual outputs created so far.
    pub fn created_virtual(&self) -> Vec<DisplayOutput> {
        self.created.borrow().clone()
    }

    fn next_virtual(&self, width: u32, height: u32) -> DisplayOutput {
        let index = self.next_virtual_index.get();
        self.next_virtual_index.set(index + 1);

        let existing: Vec<DisplayOutput> = self
            .displays
            .iter()
            .chain(self.created.borrow().iter())
            .cloned()
            .collect();
        let x = existing
            .iter()
            .map(|display| display.x.saturating_add(display.width as i32))
            .max()
            .unwrap_or(0);
        let x_screen = existing
            .iter()
            .find(|display| !display.x_screen.trim().is_empty())
            .map(|display| display.x_screen.clone())
            .unwrap_or_else(|| ":0".to_owned());

        DisplayOutput {
            index,
            name: format!("VIRTUAL-{index}"),
            width,
            height,
            x,
            y: 0,
            x_screen,
            primary: false,
            virtual_display: true,
        }
    }
}

impl DisplayBackend for FakeDisplayBackend {
    fn backend_name(&self) -> &'static str {
        "fake-display"
    }

    fn enumerate(&self) -> Result<Vec<DisplayOutput>, MultiseatError> {
        if let Some(reason) = &self.enumerate_failure {
            return Err(MultiseatError::DisplayBackend(reason.clone()));
        }
        let mut displays = self.displays.clone();
        displays.extend(self.created.borrow().iter().cloned());
        Ok(displays)
    }

    fn create_virtual(
        &self,
        width: u32,
        height: u32,
    ) -> Result<Option<DisplayOutput>, MultiseatError> {
        if !self.virtual_supported {
            return Ok(None);
        }
        let display = self.next_virtual(width, height);
        self.created.borrow_mut().push(display.clone());
        Ok(Some(display))
    }

    fn destroy_virtual(&self, display: &DisplayOutput) -> Result<bool, MultiseatError> {
        if !display.virtual_display {
            return Ok(false);
        }
        let mut created = self.created.borrow_mut();
        let before = created.len();
        created.retain(|candidate| candidate.name != display.name);
        Ok(created.len() != before)
    }
}

/// Deterministic in-memory input backend.
#[derive(Clone, Debug, Default)]
pub struct FakeInputBackend {
    devices: Vec<InputDevice>,
    failure: Option<String>,
}

impl FakeInputBackend {
    /// Create a fake with the supplied devices.
    pub fn new(devices: Vec<InputDevice>) -> Self {
        Self {
            devices,
            failure: None,
        }
    }

    /// An empty fake.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Make enumeration fail with a typed backend error.
    pub fn failing(mut self, reason: impl Into<String>) -> Self {
        self.failure = Some(reason.into());
        self
    }
}

impl InputBackend for FakeInputBackend {
    fn backend_name(&self) -> &'static str {
        "fake-input"
    }

    fn enumerate(&self) -> Result<Vec<InputDevice>, MultiseatError> {
        if let Some(reason) = &self.failure {
            return Err(MultiseatError::InputBackend(reason.clone()));
        }
        Ok(self.devices.clone())
    }
}

#[derive(Clone, Debug)]
struct FakeSeatRecord {
    state: SeatState,
    message: Option<String>,
    starts: usize,
    stops: usize,
}

/// Test-only supervisor that records start/stop calls and can inject failures.
#[derive(Clone, Debug, Default)]
pub struct FakeSeatSupervisor {
    seats: BTreeMap<String, FakeSeatRecord>,
    fail_next_start: BTreeSet<String>,
}

impl FakeSeatSupervisor {
    /// A supervisor with no seats.
    pub fn new() -> Self {
        Self::default()
    }

    /// Make the next start of `seat_id` fail and record a failed state.
    pub fn fail_next_start(mut self, seat_id: impl Into<String>) -> Self {
        self.fail_next_start.insert(seat_id.into());
        self
    }

    /// Number of successful start calls recorded for a seat.
    pub fn start_count(&self, seat_id: &str) -> usize {
        self.seats.get(seat_id).map(|seat| seat.starts).unwrap_or(0)
    }

    /// Number of stop calls recorded for a seat.
    pub fn stop_count(&self, seat_id: &str) -> usize {
        self.seats.get(seat_id).map(|seat| seat.stops).unwrap_or(0)
    }

    /// Current state for a seat.
    pub fn state(&self, seat_id: &str) -> Option<SeatState> {
        self.seats.get(seat_id).map(|seat| seat.state)
    }
}

impl SeatSupervisor for FakeSeatSupervisor {
    fn start(&mut self, plan: &SeatPlan) -> Result<SeatRuntimeStatus, MultiseatError> {
        let seat_id = plan.config.id.clone();

        if self.fail_next_start.remove(&seat_id) {
            let record = self.seats.entry(seat_id.clone()).or_insert(FakeSeatRecord {
                state: SeatState::Failed,
                message: None,
                starts: 0,
                stops: 0,
            });
            record.state = SeatState::Failed;
            record.message = Some("injected start failure".to_owned());
            return Err(MultiseatError::Supervisor(format!(
                "injected start failure for {seat_id}"
            )));
        }

        if self
            .seats
            .get(&seat_id)
            .is_some_and(|record| record.state == SeatState::Running)
        {
            return Err(MultiseatError::SeatAlreadyRunning(seat_id));
        }

        let record = self.seats.entry(seat_id.clone()).or_insert(FakeSeatRecord {
            state: SeatState::Running,
            message: None,
            starts: 0,
            stops: 0,
        });
        record.state = SeatState::Running;
        record.message = None;
        record.starts += 1;

        Ok(SeatRuntimeStatus::new(seat_id, SeatState::Running))
    }

    fn stop(&mut self, seat_id: &str) -> Result<SeatRuntimeStatus, MultiseatError> {
        let record = self
            .seats
            .get_mut(seat_id)
            .ok_or_else(|| MultiseatError::UnknownSeat(seat_id.to_owned()))?;
        record.state = SeatState::Stopped;
        record.message = None;
        record.stops += 1;
        Ok(SeatRuntimeStatus::new(seat_id, SeatState::Stopped))
    }

    fn status(&self) -> Vec<SeatRuntimeStatus> {
        self.seats
            .iter()
            .map(|(seat_id, record)| {
                let mut status = SeatRuntimeStatus::new(seat_id.clone(), record.state);
                status.message = record.message.clone();
                status
            })
            .collect()
    }
}

#[derive(Debug, Default)]
struct FakeLauncherState {
    next_pid: u32,
    spawns: Vec<NodeLaunchSpec>,
    stops: Vec<String>,
    running: BTreeMap<u32, bool>,
    latest: BTreeMap<String, u32>,
    fail_forever: BTreeSet<String>,
    fail_once: BTreeSet<String>,
}

/// Test-only process launcher. It never executes anything; it records spawn
/// specifications and lets a test simulate exits, spawn failures, and stop
/// requests.
#[derive(Clone, Debug, Default)]
pub struct FakeNodeLauncher {
    state: Arc<Mutex<FakeLauncherState>>,
}

impl FakeNodeLauncher {
    /// An empty launcher whose spawns succeed.
    pub fn new() -> Self {
        Self::default()
    }

    /// Make every subsequent spawn for `seat_id` fail.
    pub fn fail_spawn(&self, seat_id: impl Into<String>) {
        self.state
            .lock()
            .expect("fake launcher lock")
            .fail_forever
            .insert(seat_id.into());
    }

    /// Make only the next spawn for `seat_id` fail.
    pub fn fail_spawn_once(&self, seat_id: impl Into<String>) {
        self.state
            .lock()
            .expect("fake launcher lock")
            .fail_once
            .insert(seat_id.into());
    }

    /// Simulate the latest child for `seat_id` exiting.
    pub fn exit(&self, seat_id: &str) {
        let mut state = self.state.lock().expect("fake launcher lock");
        if let Some(pid) = state.latest.get(seat_id).copied() {
            state.running.insert(pid, false);
        }
    }

    /// Spawn specifications recorded so far, oldest first.
    pub fn spawns(&self) -> Vec<NodeLaunchSpec> {
        self.state
            .lock()
            .expect("fake launcher lock")
            .spawns
            .clone()
    }

    /// Number of spawn attempts recorded.
    pub fn spawn_count(&self) -> usize {
        self.state.lock().expect("fake launcher lock").spawns.len()
    }

    /// Seat ids passed to `terminate`, in order.
    pub fn stopped(&self) -> Vec<String> {
        self.state.lock().expect("fake launcher lock").stops.clone()
    }

    /// Whether the latest child for `seat_id` is still running.
    pub fn active(&self, seat_id: &str) -> bool {
        let state = self.state.lock().expect("fake launcher lock");
        state
            .latest
            .get(seat_id)
            .and_then(|pid| state.running.get(pid))
            .copied()
            .unwrap_or(false)
    }
}

impl NodeLauncher for FakeNodeLauncher {
    fn spawn(&self, spec: &NodeLaunchSpec) -> Result<Box<dyn ChildHandle>, MultiseatError> {
        let mut state = self.state.lock().expect("fake launcher lock");
        if state.fail_once.remove(&spec.seat_id) || state.fail_forever.contains(&spec.seat_id) {
            return Err(MultiseatError::Supervisor(format!(
                "injected spawn failure for {}",
                spec.seat_id
            )));
        }
        state.next_pid = state.next_pid.saturating_add(1);
        let pid = state.next_pid;
        state.spawns.push(spec.clone());
        state.running.insert(pid, true);
        state.latest.insert(spec.seat_id.clone(), pid);
        drop(state);

        Ok(Box::new(FakeChildHandle {
            pid,
            seat_id: spec.seat_id.clone(),
            state: Arc::clone(&self.state),
        }))
    }
}

struct FakeChildHandle {
    pid: u32,
    seat_id: String,
    state: Arc<Mutex<FakeLauncherState>>,
}

impl ChildHandle for FakeChildHandle {
    fn pid(&self) -> Option<u32> {
        Some(self.pid)
    }

    fn try_wait(&mut self) -> std::io::Result<Option<ChildExit>> {
        let running = self
            .state
            .lock()
            .expect("fake launcher lock")
            .running
            .get(&self.pid)
            .copied()
            .unwrap_or(false);
        Ok(if running {
            None
        } else {
            Some(ChildExit {
                success: false,
                code: Some(1),
            })
        })
    }

    fn terminate(&mut self) -> Result<(), MultiseatError> {
        let mut state = self.state.lock().expect("fake launcher lock");
        if state.running.insert(self.pid, false).unwrap_or(false) {
            state.stops.push(self.seat_id.clone());
        }
        Ok(())
    }
}

#[derive(Debug, Default)]
struct FakeProbeState {
    calls: usize,
    not_ready_calls: usize,
    fail_reason: Option<String>,
}

/// Test-only readiness probe with deterministic first-call and failure
/// behavior.
#[derive(Clone, Debug, Default)]
pub struct FakeReadinessProbe {
    state: Arc<Mutex<FakeProbeState>>,
}

impl FakeReadinessProbe {
    /// A probe that is ready on the first call.
    pub fn new() -> Self {
        Self::default()
    }

    /// A probe that always succeeds.
    pub fn always_ready() -> Self {
        Self::default()
    }

    /// A probe that always reports a typed failure.
    pub fn failing(reason: impl Into<String>) -> Self {
        let probe = Self::default();
        probe.state.lock().expect("fake probe lock").fail_reason = Some(reason.into());
        probe
    }

    /// A probe that is never ready, causing a readiness timeout.
    pub fn not_ready() -> Self {
        let probe = Self::default();
        probe.state.lock().expect("fake probe lock").not_ready_calls = usize::MAX;
        probe
    }

    /// A probe that reports not-ready for the first `polls` calls, then ready.
    pub fn ready_after(polls: usize) -> Self {
        let probe = Self::default();
        probe.state.lock().expect("fake probe lock").not_ready_calls = polls;
        probe
    }

    /// Number of probe calls made.
    pub fn calls(&self) -> usize {
        self.state.lock().expect("fake probe lock").calls
    }
}

impl ReadinessProbe for FakeReadinessProbe {
    fn probe(&self, _spec: &NodeLaunchSpec) -> Result<bool, MultiseatError> {
        let mut state = self.state.lock().expect("fake probe lock");
        state.calls = state.calls.saturating_add(1);
        if let Some(reason) = &state.fail_reason {
            return Err(MultiseatError::Supervisor(reason.clone()));
        }
        Ok(state.calls > state.not_ready_calls)
    }
}

#[derive(Clone, Debug)]
enum FakeDisplayFailure {
    MissingBinary,
    Unsupported(String),
}

#[derive(Debug, Default)]
struct FakeDisplayState {
    next_pid: u32,
    next_number: u32,
    free_numbers: BTreeSet<u32>,
    exhaust_numbers: bool,
    spawns: Vec<SeatDisplayRequest>,
    display_by_seat: BTreeMap<String, (String, u32)>,
    latest_pid: BTreeMap<String, u32>,
    pid_number: BTreeMap<u32, u32>,
    running: BTreeMap<u32, bool>,
    stops: Vec<String>,
    released: Vec<u32>,
    ready_after: BTreeMap<String, usize>,
    polls: BTreeMap<String, usize>,
    never_ready: BTreeSet<String>,
    crash_on_start: BTreeSet<String>,
    fail_spawn: BTreeMap<String, FakeDisplayFailure>,
    auth_disabled: bool,
}

/// Test-only private-X-server spawner.
///
/// It allocates a display number, records the request, and lets a test simulate
/// a healthy server, a readiness timeout, an immediate crash, a crash after
/// readiness, or a typed spawn failure. It never executes a process or opens a
/// socket.
#[derive(Clone, Debug, Default)]
pub struct FakeSeatDisplayLauncher {
    state: Arc<Mutex<FakeDisplayState>>,
}

impl FakeSeatDisplayLauncher {
    /// A launcher that starts at `:90` and becomes ready on the first probe.
    pub fn new() -> Self {
        Self::with_start_number(90)
    }

    /// A launcher whose first allocated display number is `start`.
    pub fn with_start_number(start: u32) -> Self {
        let launcher = Self::default();
        launcher
            .state
            .lock()
            .expect("fake display lock")
            .next_number = start;
        launcher
    }

    /// Never answer the readiness handshake for `seat_id`.
    pub fn never_ready(&self, seat_id: impl Into<String>) {
        self.state
            .lock()
            .expect("fake display lock")
            .never_ready
            .insert(seat_id.into());
    }

    /// Become ready only after `polls` unsuccessful probes.
    pub fn ready_after(&self, seat_id: impl Into<String>, polls: usize) {
        self.state
            .lock()
            .expect("fake display lock")
            .ready_after
            .insert(seat_id.into(), polls);
    }

    /// Exit before the first readiness probe succeeds.
    pub fn crash_on_start(&self, seat_id: impl Into<String>) {
        self.state
            .lock()
            .expect("fake display lock")
            .crash_on_start
            .insert(seat_id.into());
    }

    /// Simulate the running display for `seat_id` exiting.
    pub fn crash(&self, seat_id: &str) {
        let mut state = self.state.lock().expect("fake display lock");
        if let Some(pid) = state.latest_pid.get(seat_id).copied() {
            state.running.insert(pid, false);
        }
    }

    /// Fail the next spawn for `seat_id` with a typed missing-binary error.
    pub fn fail_spawn_missing_binary(&self, seat_id: impl Into<String>) {
        self.state
            .lock()
            .expect("fake display lock")
            .fail_spawn
            .insert(seat_id.into(), FakeDisplayFailure::MissingBinary);
    }

    /// Fail the next spawn for `seat_id` with a typed unsupported error.
    pub fn fail_spawn_unsupported(&self, seat_id: impl Into<String>, reason: impl Into<String>) {
        self.state
            .lock()
            .expect("fake display lock")
            .fail_spawn
            .insert(
                seat_id.into(),
                FakeDisplayFailure::Unsupported(reason.into()),
            );
    }

    /// Make every allocation report display-number exhaustion.
    pub fn exhaust_numbers(&self) {
        self.state
            .lock()
            .expect("fake display lock")
            .exhaust_numbers = true;
    }

    /// Report `-ac` instead of an auth cookie for subsequently spawned servers.
    pub fn with_auth_disabled(&self) {
        self.state.lock().expect("fake display lock").auth_disabled = true;
    }

    /// Spawn requests recorded so far.
    pub fn spawns(&self) -> Vec<SeatDisplayRequest> {
        self.state.lock().expect("fake display lock").spawns.clone()
    }

    /// Number of display spawn attempts.
    pub fn spawn_count(&self) -> usize {
        self.state.lock().expect("fake display lock").spawns.len()
    }

    /// Display string assigned to a seat.
    pub fn display_for(&self, seat_id: &str) -> Option<String> {
        self.state
            .lock()
            .expect("fake display lock")
            .display_by_seat
            .get(seat_id)
            .map(|(display, _)| display.clone())
    }

    /// Display number assigned to a seat.
    pub fn number_for(&self, seat_id: &str) -> Option<u32> {
        self.state
            .lock()
            .expect("fake display lock")
            .display_by_seat
            .get(seat_id)
            .map(|(_, number)| *number)
    }

    /// Latest display pid for a seat.
    pub fn pid_for(&self, seat_id: &str) -> Option<u32> {
        self.state
            .lock()
            .expect("fake display lock")
            .latest_pid
            .get(seat_id)
            .copied()
    }

    /// Whether the latest display for a seat is still running.
    pub fn active(&self, seat_id: &str) -> bool {
        let state = self.state.lock().expect("fake display lock");
        state
            .latest_pid
            .get(seat_id)
            .and_then(|pid| state.running.get(pid))
            .copied()
            .unwrap_or(false)
    }

    /// Display strings passed to `terminate`, in order.
    pub fn stops(&self) -> Vec<String> {
        self.state.lock().expect("fake display lock").stops.clone()
    }

    /// Display numbers released by `terminate`, in order.
    pub fn released_numbers(&self) -> Vec<u32> {
        self.state
            .lock()
            .expect("fake display lock")
            .released
            .clone()
    }
}

impl SeatDisplayLauncher for FakeSeatDisplayLauncher {
    fn spawn(
        &self,
        request: &SeatDisplayRequest,
    ) -> Result<Box<dyn SeatDisplayHandle>, MultiseatError> {
        let mut state = self.state.lock().expect("fake display lock");
        if let Some(failure) = state.fail_spawn.remove(&request.seat_id) {
            return match failure {
                FakeDisplayFailure::MissingBinary => Err(MultiseatError::SeatDisplayTool {
                    backend: request.backend.as_str().to_owned(),
                    program: format!("fake-{}", request.backend.as_str()),
                    reason: "injected missing binary".to_owned(),
                }),
                FakeDisplayFailure::Unsupported(reason) => Err(MultiseatError::Unsupported(reason)),
            };
        }
        if state.exhaust_numbers {
            return Err(MultiseatError::SeatDisplayNumberExhausted { start: 90, end: 90 });
        }
        let number = match state.free_numbers.iter().next().copied() {
            Some(number) => {
                state.free_numbers.remove(&number);
                number
            }
            None => {
                let number = state.next_number;
                state.next_number = state.next_number.saturating_add(1);
                number
            }
        };
        let (display, wayland) = if request.backend == SeatDisplayBackend::Weston {
            let display = wayland_display_name(number);
            let runtime = std::env::temp_dir()
                .join(format!("ozma-fake-weston-{}-{number}", std::process::id()));
            (display.clone(), Some(SeatWaylandEnv::new(display, runtime)))
        } else {
            (format!(":{number}"), None)
        };
        state.next_pid = state.next_pid.saturating_add(1);
        let pid = state.next_pid;
        state.spawns.push(request.clone());
        state
            .display_by_seat
            .insert(request.seat_id.clone(), (display.clone(), number));
        state.latest_pid.insert(request.seat_id.clone(), pid);
        state.pid_number.insert(pid, number);
        let starts_alive = !state.crash_on_start.contains(&request.seat_id);
        state.running.insert(pid, starts_alive);
        let auth_disabled = state.auth_disabled;
        drop(state);

        Ok(Box::new(FakeDisplayHandle {
            pid,
            number,
            display,
            wayland,
            seat_id: request.seat_id.clone(),
            backend: request.backend,
            auth_disabled,
            state: Arc::clone(&self.state),
        }))
    }
}

struct FakeDisplayHandle {
    pid: u32,
    number: u32,
    display: String,
    wayland: Option<SeatWaylandEnv>,
    seat_id: String,
    backend: SeatDisplayBackend,
    auth_disabled: bool,
    state: Arc<Mutex<FakeDisplayState>>,
}

impl SeatDisplayHandle for FakeDisplayHandle {
    fn backend(&self) -> SeatDisplayBackend {
        self.backend
    }

    fn display(&self) -> &str {
        &self.display
    }

    fn display_number(&self) -> u32 {
        self.number
    }

    fn pid(&self) -> Option<u32> {
        Some(self.pid)
    }

    fn xauthority(&self) -> Option<&std::path::Path> {
        None
    }

    fn auth_disabled(&self) -> bool {
        self.auth_disabled
    }

    fn wayland(&self) -> Option<&SeatWaylandEnv> {
        self.wayland.as_ref()
    }

    fn is_ready(&self) -> Result<bool, MultiseatError> {
        let mut state = self.state.lock().expect("fake display lock");
        if !state.running.get(&self.pid).copied().unwrap_or(false) {
            return Ok(false);
        }
        let polls = state.polls.entry(self.seat_id.clone()).or_insert(0);
        *polls = polls.saturating_add(1);
        let polls = *polls;
        if state.never_ready.contains(&self.seat_id) {
            return Ok(false);
        }
        let threshold = state.ready_after.get(&self.seat_id).copied().unwrap_or(0);
        Ok(polls > threshold)
    }

    fn try_wait(&mut self) -> std::io::Result<Option<DisplayExit>> {
        let running = self
            .state
            .lock()
            .expect("fake display lock")
            .running
            .get(&self.pid)
            .copied()
            .unwrap_or(false);
        Ok(if running {
            None
        } else {
            Some(DisplayExit {
                success: false,
                code: Some(1),
            })
        })
    }

    fn terminate(&mut self) -> Result<(), MultiseatError> {
        let mut state = self.state.lock().expect("fake display lock");
        if state.running.insert(self.pid, false).unwrap_or(false) {
            state.stops.push(self.display.clone());
        }
        let number = state
            .pid_number
            .get(&self.pid)
            .copied()
            .unwrap_or(self.number);
        state.free_numbers.insert(number);
        state.released.push(number);
        Ok(())
    }
}

#[derive(Debug)]
struct FakeCommandRule {
    program: String,
    args: Vec<String>,
    output: CommandOutput,
}

#[derive(Debug, Default)]
struct FakeCommandRunnerState {
    commands: Vec<RouteCommand>,
    rules: Vec<FakeCommandRule>,
    unavailable: BTreeSet<String>,
}

/// Recording command runner for routing and ownership tests.
///
/// Every invocation is recorded, and responses are matched by exact program
/// and argv. A program added with [`Self::with_unavailable`] returns a typed
/// [`MultiseatError::CommandUnavailable`]. An unscripted invocation returns a
/// typed error instead of a fabricated success.
#[derive(Clone, Debug, Default)]
pub struct FakeCommandRunner {
    state: Arc<Mutex<FakeCommandRunnerState>>,
}

impl FakeCommandRunner {
    /// An empty runner with no scripted responses.
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark a program as absent. Any invocation returns `CommandUnavailable`.
    pub fn with_unavailable(self, program: impl Into<String>) -> Self {
        self.state
            .lock()
            .expect("fake command runner lock")
            .unavailable
            .insert(program.into());
        self
    }

    /// Script one exact invocation to return `output`.
    pub fn respond(self, program: impl Into<String>, args: &[&str], output: CommandOutput) -> Self {
        self.state
            .lock()
            .expect("fake command runner lock")
            .rules
            .push(FakeCommandRule {
                program: program.into(),
                args: args.iter().map(|arg| (*arg).to_owned()).collect(),
                output,
            });
        self
    }

    /// Script one exact invocation to succeed with `stdout`.
    pub fn respond_stdout(self, program: impl Into<String>, args: &[&str], stdout: &str) -> Self {
        self.respond(program, args, CommandOutput::stdout(stdout))
    }

    /// Script one exact invocation to fail with an exit status and stderr.
    pub fn respond_failure(
        self,
        program: impl Into<String>,
        args: &[&str],
        status: i32,
        stderr: &str,
    ) -> Self {
        self.respond(program, args, CommandOutput::failure(status, stderr))
    }

    /// Every command passed to [`CommandRunner::run`], in order.
    pub fn commands(&self) -> Vec<RouteCommand> {
        self.state
            .lock()
            .expect("fake command runner lock")
            .commands
            .clone()
    }

    /// Number of invocations.
    pub fn run_count(&self) -> usize {
        self.state
            .lock()
            .expect("fake command runner lock")
            .commands
            .len()
    }

    /// Most recent invocation, if any.
    pub fn last_command(&self) -> Option<RouteCommand> {
        self.state
            .lock()
            .expect("fake command runner lock")
            .commands
            .last()
            .cloned()
    }
}

impl CommandRunner for FakeCommandRunner {
    fn run(&self, command: &RouteCommand) -> Result<CommandOutput, MultiseatError> {
        let mut state = self.state.lock().expect("fake command runner lock");
        state.commands.push(command.clone());

        if state.unavailable.contains(&command.program) {
            return Err(MultiseatError::CommandUnavailable {
                program: command.program.clone(),
                reason: "marked unavailable by the fake runner".to_owned(),
            });
        }

        state
            .rules
            .iter()
            .find(|rule| rule.program == command.program && rule.args == command.args)
            .map(|rule| rule.output.clone())
            .ok_or_else(|| {
                MultiseatError::InputRouting(format!(
                    "fake command runner has no scripted response for {:?} {:?}",
                    command.program, command.args
                ))
            })
    }
}

/// Fake evdev path-to-name resolver backed by an in-memory map.
#[derive(Clone, Debug, Default)]
pub struct FakeDeviceNameResolver {
    names: Arc<Mutex<BTreeMap<String, String>>>,
    failure: Arc<Mutex<Option<String>>>,
}

impl FakeDeviceNameResolver {
    /// An empty resolver.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a name and return `self` for chaining.
    pub fn with_name(self, evdev_path: impl Into<String>, name: impl Into<String>) -> Self {
        self.names
            .lock()
            .expect("fake resolver lock")
            .insert(evdev_path.into(), name.into());
        self
    }

    /// Insert a name in place.
    pub fn insert(&self, evdev_path: impl Into<String>, name: impl Into<String>) {
        self.names
            .lock()
            .expect("fake resolver lock")
            .insert(evdev_path.into(), name.into());
    }

    /// Make every lookup return a typed I/O failure.
    pub fn failing(reason: impl Into<String>) -> Self {
        let resolver = Self::new();
        *resolver.failure.lock().expect("fake resolver lock") = Some(reason.into());
        resolver
    }
}

impl DeviceNameResolver for FakeDeviceNameResolver {
    fn device_name(&self, evdev_path: &str) -> Result<Option<String>, MultiseatError> {
        if let Some(reason) = self.failure.lock().expect("fake resolver lock").as_ref() {
            return Err(MultiseatError::InputRouting(reason.clone()));
        }
        Ok(self
            .names
            .lock()
            .expect("fake resolver lock")
            .get(evdev_path)
            .cloned())
    }
}

/// One fake grab/ungrab event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FakeGrabAction {
    /// Exclusive grab.
    Grab,
    /// Release.
    Ungrab,
}

/// Recorded fake grab event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FakeGrabEvent {
    /// Grab or release.
    pub action: FakeGrabAction,
    /// Evdev path.
    pub device: String,
    /// Seat id.
    pub seat_id: String,
}

#[derive(Debug, Default)]
struct FakeGrabState {
    grabbed: BTreeSet<String>,
    events: Vec<FakeGrabEvent>,
    fail_devices: BTreeSet<String>,
    fail_next_grab: bool,
}

/// In-memory `EvdevGrabBackend` that never opens `/dev/input`.
#[derive(Clone, Debug, Default)]
pub struct FakeEvdevGrabBackend {
    state: Arc<Mutex<FakeGrabState>>,
}

impl FakeEvdevGrabBackend {
    /// An empty backend.
    pub fn new() -> Self {
        Self::default()
    }

    /// Fail the next grab request.
    pub fn fail_next_grab(self) -> Self {
        self.state.lock().expect("fake grab lock").fail_next_grab = true;
        self
    }

    /// Always fail grabs for one device path.
    pub fn fail_device(self, device: impl Into<String>) -> Self {
        self.state
            .lock()
            .expect("fake grab lock")
            .fail_devices
            .insert(device.into());
        self
    }

    /// Currently grabbed device paths, sorted.
    pub fn grabbed(&self) -> Vec<String> {
        self.state
            .lock()
            .expect("fake grab lock")
            .grabbed
            .iter()
            .cloned()
            .collect()
    }

    /// Every grab/ungrab event, in order.
    pub fn events(&self) -> Vec<FakeGrabEvent> {
        self.state.lock().expect("fake grab lock").events.clone()
    }
}

impl EvdevGrabBackend for FakeEvdevGrabBackend {
    fn grab(&mut self, request: &EvdevGrabRequest) -> Result<(), MultiseatError> {
        let mut state = self.state.lock().expect("fake grab lock");
        if state.fail_next_grab || state.fail_devices.contains(&request.device) {
            state.fail_next_grab = false;
            return Err(MultiseatError::InputRouting(format!(
                "fake grab failure for {}",
                request.device
            )));
        }
        state.grabbed.insert(request.device.clone());
        state.events.push(FakeGrabEvent {
            action: FakeGrabAction::Grab,
            device: request.device.clone(),
            seat_id: request.seat_id.clone(),
        });
        Ok(())
    }

    fn ungrab(&mut self, request: &EvdevGrabRequest) -> Result<(), MultiseatError> {
        let mut state = self.state.lock().expect("fake grab lock");
        state.grabbed.remove(&request.device);
        state.events.push(FakeGrabEvent {
            action: FakeGrabAction::Ungrab,
            device: request.device.clone(),
            seat_id: request.seat_id.clone(),
        });
        Ok(())
    }
}

#[derive(Debug, Default)]
struct FakeSeatAudioState {
    ensured: Vec<SeatAudioSpec>,
    states: BTreeMap<String, SeatAudioState>,
    removed: Vec<String>,
    present: BTreeSet<String>,
    fail_ensure: Option<String>,
    fail_remove: Option<String>,
}

/// Test-only per-seat audio router. It records sink ensure/remove calls and can
/// inject a typed failure. It never runs `pactl`, touches PipeWire, or reports a
/// sink it did not record.
#[derive(Clone, Debug, Default)]
pub struct FakeSeatAudioRouter {
    state: Arc<Mutex<FakeSeatAudioState>>,
}

impl FakeSeatAudioRouter {
    /// A router with no sinks and no injected failures.
    pub fn new() -> Self {
        Self::default()
    }

    /// Make the next `ensure_sink` fail with a typed audio error.
    pub fn fail_next_ensure(self, message: impl Into<String>) -> Self {
        self.state.lock().expect("fake audio lock").fail_ensure = Some(message.into());
        self
    }

    /// Make the next `remove_sink` fail with a typed audio error.
    pub fn fail_next_remove(self, message: impl Into<String>) -> Self {
        self.state.lock().expect("fake audio lock").fail_remove = Some(message.into());
        self
    }

    /// Sink specs passed to `ensure_sink`, in call order.
    pub fn ensured(&self) -> Vec<SeatAudioSpec> {
        self.state.lock().expect("fake audio lock").ensured.clone()
    }

    /// Sink names passed to `remove_sink`, in call order.
    pub fn removed(&self) -> Vec<String> {
        self.state.lock().expect("fake audio lock").removed.clone()
    }

    /// Whether `ensure_sink` left a sink present.
    pub fn is_present(&self, name: &str) -> bool {
        self.state
            .lock()
            .expect("fake audio lock")
            .present
            .contains(name)
    }
}

impl SeatAudioRouter for FakeSeatAudioRouter {
    fn ensure_sink(&self, spec: &SeatAudioSpec) -> Result<SeatAudioState, MultiseatError> {
        let mut state = self.state.lock().expect("fake audio lock");
        if let Some(message) = state.fail_ensure.take() {
            return Err(MultiseatError::Audio(message));
        }
        let created = state.present.insert(spec.name.clone());
        let audio_state = SeatAudioState {
            name: spec.name.clone(),
            monitor_target: spec.monitor_target(),
            created,
        };
        state.ensured.push(spec.clone());
        state.states.insert(spec.name.clone(), audio_state.clone());
        Ok(audio_state)
    }

    fn remove_sink(&self, name: &str) -> Result<(), MultiseatError> {
        let mut state = self.state.lock().expect("fake audio lock");
        if let Some(message) = state.fail_remove.take() {
            return Err(MultiseatError::Audio(message));
        }
        state.removed.push(name.to_owned());
        state.present.remove(name);
        Ok(())
    }
}

#[derive(Debug, Default)]
struct FakeHotplugState {
    pending: VecDeque<HotplugEvent>,
    polls: usize,
    fail_poll: Option<String>,
}

/// Test-only hotplug source backed by an in-memory event queue.
#[derive(Clone, Debug, Default)]
pub struct FakeHotplugMonitor {
    state: Arc<Mutex<FakeHotplugState>>,
}

impl FakeHotplugMonitor {
    /// A monitor with no queued events.
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue one event.
    pub fn push(&self, event: HotplugEvent) -> &Self {
        self.state
            .lock()
            .expect("fake hotplug lock")
            .pending
            .push_back(event);
        self
    }

    /// Queue several events.
    pub fn push_many(&self, events: impl IntoIterator<Item = HotplugEvent>) -> &Self {
        let mut state = self.state.lock().expect("fake hotplug lock");
        state.pending.extend(events);
        drop(state);
        self
    }

    /// Make the next poll fail with a typed hotplug error.
    pub fn fail_next_poll(&self, message: impl Into<String>) -> &Self {
        self.state.lock().expect("fake hotplug lock").fail_poll = Some(message.into());
        self
    }

    /// Number of polls served, including a failed poll.
    pub fn polls(&self) -> usize {
        self.state.lock().expect("fake hotplug lock").polls
    }
}

impl HotplugMonitor for FakeHotplugMonitor {
    fn poll(&mut self) -> Result<Vec<HotplugEvent>, MultiseatError> {
        let mut state = self.state.lock().expect("fake hotplug lock");
        state.polls += 1;
        if let Some(message) = state.fail_poll.take() {
            return Err(MultiseatError::Hotplug(message));
        }
        Ok(state.pending.drain(..).collect())
    }
}

#[derive(Debug, Default)]
struct FakeHelperBackendState {
    grabbed: BTreeSet<String>,
    uinputs: BTreeMap<String, String>,
    events: Vec<String>,
    fail_paths: BTreeSet<String>,
    fail_next_grab: Option<String>,
    fail_uinput: Option<String>,
    fail_release: Option<String>,
    next_uinput: usize,
}

/// In-memory ioctl/uinput backend for the helper state machine.
///
/// It records every grab/uinput/relay action and never opens `/dev/input` or
/// `/dev/uinput`. Failures are injectable per path.
#[derive(Clone, Debug, Default)]
pub struct FakeHelperBackend {
    state: Arc<Mutex<FakeHelperBackendState>>,
}

impl FakeHelperBackend {
    /// An empty backend.
    pub fn new() -> Self {
        Self::default()
    }

    /// Always fail grabs for one path.
    pub fn fail_device(self, path: impl Into<String>) -> Self {
        self.state
            .lock()
            .expect("fake helper backend lock")
            .fail_paths
            .insert(path.into());
        self
    }

    /// Fail the next grab with a typed message.
    pub fn fail_next_grab(self, message: impl Into<String>) -> Self {
        self.state
            .lock()
            .expect("fake helper backend lock")
            .fail_next_grab = Some(message.into());
        self
    }

    /// Fail uinput creation for one path.
    pub fn fail_uinput(self, path: impl Into<String>) -> Self {
        self.state
            .lock()
            .expect("fake helper backend lock")
            .fail_uinput = Some(path.into());
        self
    }

    /// Fail the next release.
    pub fn fail_next_release(self, message: impl Into<String>) -> Self {
        self.state
            .lock()
            .expect("fake helper backend lock")
            .fail_release = Some(message.into());
        self
    }

    /// Currently grabbed paths, sorted.
    pub fn grabbed(&self) -> Vec<String> {
        self.state
            .lock()
            .expect("fake helper backend lock")
            .grabbed
            .iter()
            .cloned()
            .collect()
    }

    /// Created uinput names, sorted by name.
    pub fn uinput_names(&self) -> Vec<String> {
        self.state
            .lock()
            .expect("fake helper backend lock")
            .uinputs
            .keys()
            .cloned()
            .collect()
    }

    /// Recorded backend events, in order.
    pub fn events(&self) -> Vec<String> {
        self.state
            .lock()
            .expect("fake helper backend lock")
            .events
            .clone()
    }
}

impl HelperBackend for FakeHelperBackend {
    fn backend_kind(&self) -> HelperBackendKind {
        HelperBackendKind::Fake
    }

    fn grab(&mut self, device: &HelperDevice) -> Result<(), HelperError> {
        let mut state = self.state.lock().expect("fake helper backend lock");
        if let Some(message) = state.fail_next_grab.take() {
            return Err(HelperError::new(HelperErrorCode::GrabFailed, message));
        }
        if state.fail_paths.contains(&device.path) {
            return Err(
                HelperError::new(HelperErrorCode::GrabFailed, "injected grab failure")
                    .with_device(&device.path),
            );
        }
        if !state.grabbed.insert(device.path.clone()) {
            return Err(HelperError::new(
                HelperErrorCode::CrossSeatConflict,
                format!("{} is already grabbed", device.path),
            ));
        }
        state.events.push(format!("grab {}", device.path));
        Ok(())
    }

    fn ungrab(&mut self, path: &str) -> Result<(), HelperError> {
        let mut state = self.state.lock().expect("fake helper backend lock");
        state.grabbed.remove(path);
        state.events.push(format!("ungrab {path}"));
        Ok(())
    }

    fn create_uinput(
        &mut self,
        seat_id: &str,
        device: &HelperDevice,
    ) -> Result<String, HelperError> {
        let mut state = self.state.lock().expect("fake helper backend lock");
        if state.fail_uinput.as_deref() == Some(device.path.as_str()) {
            state.fail_uinput = None;
            return Err(
                HelperError::new(HelperErrorCode::UinputFailed, "injected uinput failure")
                    .with_device(&device.path),
            );
        }
        let name = if state.next_uinput == 0 {
            default_uinput_name(seat_id, device.kind, &device.path)
        } else {
            format!("fake-uinput-{}-{}", seat_id, state.next_uinput)
        };
        state.next_uinput += 1;
        state.uinputs.insert(name.clone(), device.path.clone());
        state
            .events
            .push(format!("uinput {} -> {}", name, device.path));
        Ok(name)
    }

    fn start_relay(&mut self, device: &HelperDevice, uinput_name: &str) -> Result<(), HelperError> {
        let mut state = self.state.lock().expect("fake helper backend lock");
        if !state.grabbed.contains(&device.path) {
            return Err(
                HelperError::new(HelperErrorCode::Backend, "relay without grab")
                    .with_device(&device.path),
            );
        }
        state
            .events
            .push(format!("relay {} -> {}", device.path, uinput_name));
        Ok(())
    }

    fn stop_relay(&mut self, path: &str) -> Result<(), HelperError> {
        let mut state = self.state.lock().expect("fake helper backend lock");
        if let Some(message) = state.fail_release.take() {
            return Err(
                HelperError::new(HelperErrorCode::Backend, message).with_device(path.to_owned())
            );
        }
        state.grabbed.remove(path);
        state.events.push(format!("stop-relay {path}"));
        Ok(())
    }

    fn destroy_uinput(&mut self, name: &str) -> Result<(), HelperError> {
        let mut state = self.state.lock().expect("fake helper backend lock");
        state.uinputs.remove(name);
        state.events.push(format!("destroy-uinput {name}"));
        Ok(())
    }
}

#[derive(Clone, Debug, Default)]
struct FakeSeatInputHelperState {
    alive: bool,
    starts: usize,
    crashes: usize,
    assignments: BTreeMap<String, HelperSeatStatus>,
    events: Vec<String>,
    fail_next_grab: Option<String>,
    fail_seat: Option<String>,
    fail_release: Option<String>,
    fail_ready: Option<String>,
    enforce_conflicts: bool,
}

/// Test-only [`SeatInputHelper`] with the same conflict rules as the real
/// helper. It never starts a process or touches a device.
#[derive(Clone, Debug)]
pub struct FakeSeatInputHelper {
    state: Arc<Mutex<FakeSeatInputHelperState>>,
}

impl Default for FakeSeatInputHelper {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(FakeSeatInputHelperState {
                enforce_conflicts: true,
                ..FakeSeatInputHelperState::default()
            })),
        }
    }
}

impl FakeSeatInputHelper {
    /// A dead helper that starts on the first `ensure_ready`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Fail the next grab with a typed message.
    pub fn fail_next_grab(self, message: impl Into<String>) -> Self {
        self.state
            .lock()
            .expect("fake seat input lock")
            .fail_next_grab = Some(message.into());
        self
    }

    /// Always fail grabs for one seat.
    pub fn fail_seat(self, seat_id: impl Into<String>) -> Self {
        self.state.lock().expect("fake seat input lock").fail_seat = Some(seat_id.into());
        self
    }

    /// Fail the next release with a typed message.
    pub fn fail_next_release(self, message: impl Into<String>) -> Self {
        self.state
            .lock()
            .expect("fake seat input lock")
            .fail_release = Some(message.into());
        self
    }

    /// Fail readiness/restart with a typed message.
    pub fn fail_ready(self, message: impl Into<String>) -> Self {
        self.state.lock().expect("fake seat input lock").fail_ready = Some(message.into());
        self
    }

    /// Simulate a helper crash: the process is gone and the kernel dropped its
    /// grabs. A later `ensure_ready` starts a fresh process.
    pub fn crash(&self) {
        let mut state = self.state.lock().expect("fake seat input lock");
        state.alive = false;
        state.crashes += 1;
        state.assignments.clear();
        state.events.push("crash".to_owned());
    }

    /// Number of helper starts, including the initial one.
    pub fn start_count(&self) -> usize {
        self.state.lock().expect("fake seat input lock").starts
    }

    /// Number of simulated crashes.
    pub fn crash_count(&self) -> usize {
        self.state.lock().expect("fake seat input lock").crashes
    }

    /// Grabbed paths for one seat.
    pub fn grabbed(&self, seat_id: &str) -> Vec<String> {
        self.state
            .lock()
            .expect("fake seat input lock")
            .assignments
            .get(seat_id)
            .map(|seat| seat.grabbed.clone())
            .unwrap_or_default()
    }

    /// uinput names for one seat.
    pub fn uinput_devices(&self, seat_id: &str) -> Vec<String> {
        self.state
            .lock()
            .expect("fake seat input lock")
            .assignments
            .get(seat_id)
            .map(|seat| seat.uinput_devices.clone())
            .unwrap_or_default()
    }

    /// Recorded client-visible events, in order.
    pub fn events(&self) -> Vec<String> {
        self.state
            .lock()
            .expect("fake seat input lock")
            .events
            .clone()
    }

    /// Whether a seat currently has a confirmed assignment.
    pub fn has_assignment(&self, seat_id: &str) -> bool {
        self.state
            .lock()
            .expect("fake seat input lock")
            .assignments
            .contains_key(seat_id)
    }
}

impl SeatInputHelper for FakeSeatInputHelper {
    fn ensure_ready(&mut self) -> Result<HelperResponse, MultiseatError> {
        let mut state = self.state.lock().expect("fake seat input lock");
        if let Some(message) = state.fail_ready.take() {
            state.events.push("ready-failed".to_owned());
            return Err(MultiseatError::InputIsolation(message));
        }
        if !state.alive {
            state.alive = true;
            state.starts += 1;
            let starts = state.starts;
            state.events.push(format!("start {starts}"));
        }
        Ok(HelperResponse::Ready {
            protocol: HELPER_PROTOCOL_VERSION,
            helper: "fake-seat-input".to_owned(),
            backend: HelperBackendKind::Fake,
            pid: 0,
        })
    }

    fn grab(
        &mut self,
        seat_id: &str,
        devices: &[HelperDevice],
    ) -> Result<InputIsolationAssignment, MultiseatError> {
        self.ensure_ready()?;
        let mut state = self.state.lock().expect("fake seat input lock");
        if let Some(message) = state.fail_next_grab.take() {
            state.events.push(format!("grab-failed {seat_id}"));
            return Err(MultiseatError::InputIsolation(message));
        }
        if state.fail_seat.as_deref() == Some(seat_id) {
            state.events.push(format!("grab-failed {seat_id}"));
            return Err(MultiseatError::InputIsolation(format!(
                "injected grab failure for {seat_id}"
            )));
        }
        if let Some(existing) = state.assignments.get(seat_id) {
            let same = existing.grabbed.len() == devices.len()
                && devices
                    .iter()
                    .all(|device| existing.grabbed.iter().any(|path| path == &device.path));
            if same {
                return Ok(InputIsolationAssignment {
                    seat_id: seat_id.to_owned(),
                    grabbed: existing.grabbed.clone(),
                    uinput_devices: existing.uinput_devices.clone(),
                });
            }
            return Err(MultiseatError::InputIsolation(format!(
                "seat {seat_id} already owns a different device set"
            )));
        }
        if state.enforce_conflicts {
            let mut conflict = None;
            for (other, assignment) in &state.assignments {
                for device in devices {
                    if assignment.grabbed.contains(&device.path) {
                        conflict = Some((other.clone(), device.path.clone()));
                        break;
                    }
                }
                if conflict.is_some() {
                    break;
                }
            }
            if let Some((other, path)) = conflict {
                state.events.push(format!("conflict {seat_id} {path}"));
                return Err(MultiseatError::InputIsolation(format!(
                    "device {path} is already grabbed for seat {other}"
                )));
            }
        }
        let grabbed: Vec<String> = devices.iter().map(|device| device.path.clone()).collect();
        let uinput_devices: Vec<String> = devices
            .iter()
            .map(|device| default_uinput_name(seat_id, device.kind, &device.path))
            .collect();
        state.assignments.insert(
            seat_id.to_owned(),
            HelperSeatStatus {
                seat_id: seat_id.to_owned(),
                grabbed: grabbed.clone(),
                uinput_devices: uinput_devices.clone(),
            },
        );
        state
            .events
            .push(format!("grab {seat_id} {}", grabbed.join(",")));
        Ok(InputIsolationAssignment {
            seat_id: seat_id.to_owned(),
            grabbed,
            uinput_devices,
        })
    }

    fn release(&mut self, seat_id: &str) -> Result<InputIsolationRelease, MultiseatError> {
        let mut state = self.state.lock().expect("fake seat input lock");
        if let Some(message) = state.fail_release.take() {
            state.events.push(format!("release-failed {seat_id}"));
            return Err(MultiseatError::InputIsolation(message));
        }
        let released = state
            .assignments
            .remove(seat_id)
            .map(|seat| seat.grabbed)
            .unwrap_or_default();
        state.events.push(format!("release {seat_id}"));
        Ok(InputIsolationRelease {
            seat_id: seat_id.to_owned(),
            released,
        })
    }

    fn seat_status(&mut self, seat_id: &str) -> Result<Option<HelperSeatStatus>, MultiseatError> {
        let state = self.state.lock().expect("fake seat input lock");
        if !state.alive {
            return Ok(None);
        }
        Ok(state.assignments.get(seat_id).cloned())
    }

    fn shutdown(&mut self) {
        let mut state = self.state.lock().expect("fake seat input lock");
        state.alive = false;
        state.assignments.clear();
        state.events.push("shutdown".to_owned());
    }
}

#[derive(Debug, Default)]
struct FakeInputTargeterState {
    available: Option<Vec<String>>,
    requests: Vec<SeatInputTargetRequest>,
    applied: BTreeMap<String, SeatInputTargeting>,
    released: Vec<String>,
    cleared: Vec<String>,
    events: Vec<String>,
    fail_next: Option<String>,
    fail_seat: Option<String>,
    fail_next_release: Option<String>,
}

/// Test-only [`SeatInputTargeter`] with the same enable/disable classification
/// as the real `xinput` targeter. It never starts a process or opens a display.
#[derive(Clone, Debug, Default)]
pub struct FakeSeatInputTargeter {
    state: Arc<Mutex<FakeInputTargeterState>>,
}

impl FakeSeatInputTargeter {
    /// A targeter that treats every requested own device as present and finds
    /// no other-seat devices unless [`Self::with_available_devices`] is used.
    pub fn new() -> Self {
        Self::default()
    }

    /// Supply the simulated `xinput list` names for the display. Names starting
    /// with `ozma-seat-` that are not own devices are disabled.
    pub fn with_available_devices(self, devices: Vec<String>) -> Self {
        self.state.lock().expect("fake targeter lock").available = Some(devices);
        self
    }

    /// Fail the next apply with a typed message.
    pub fn fail_next(self, message: impl Into<String>) -> Self {
        self.state.lock().expect("fake targeter lock").fail_next = Some(message.into());
        self
    }

    /// Always fail applies for one seat.
    pub fn fail_seat(self, seat_id: impl Into<String>) -> Self {
        self.state.lock().expect("fake targeter lock").fail_seat = Some(seat_id.into());
        self
    }

    /// Fail the next release with a typed message.
    pub fn fail_next_release(self, message: impl Into<String>) -> Self {
        self.state
            .lock()
            .expect("fake targeter lock")
            .fail_next_release = Some(message.into());
        self
    }

    /// Every request recorded so far, in order.
    pub fn requests(&self) -> Vec<SeatInputTargetRequest> {
        self.state
            .lock()
            .expect("fake targeter lock")
            .requests
            .clone()
    }

    /// Number of apply calls.
    pub fn apply_count(&self) -> usize {
        self.state
            .lock()
            .expect("fake targeter lock")
            .requests
            .len()
    }

    /// Seat ids passed to `release`, in order.
    pub fn releases(&self) -> Vec<String> {
        self.state
            .lock()
            .expect("fake targeter lock")
            .released
            .clone()
    }

    /// Seat ids passed to `clear`, in order.
    pub fn clears(&self) -> Vec<String> {
        self.state
            .lock()
            .expect("fake targeter lock")
            .cleared
            .clone()
    }

    /// Recorded events, in order.
    pub fn events(&self) -> Vec<String> {
        self.state
            .lock()
            .expect("fake targeter lock")
            .events
            .clone()
    }

    /// Whether a seat currently has an applied targeting.
    pub fn has_targeting(&self, seat_id: &str) -> bool {
        self.state
            .lock()
            .expect("fake targeter lock")
            .applied
            .contains_key(seat_id)
    }
}

impl SeatInputTargeter for FakeSeatInputTargeter {
    fn apply(
        &mut self,
        request: &SeatInputTargetRequest,
    ) -> Result<SeatInputTargeting, MultiseatError> {
        let mut state = self.state.lock().expect("fake targeter lock");
        state.requests.push(request.clone());
        if let Some(message) = state.fail_next.take() {
            state
                .events
                .push(format!("target-failed {}", request.seat_id));
            return Err(MultiseatError::InputTargeting(message));
        }
        if state.fail_seat.as_deref() == Some(request.seat_id.as_str()) {
            state
                .events
                .push(format!("target-failed {}", request.seat_id));
            return Err(MultiseatError::InputTargeting(format!(
                "injected targeting failure for {}",
                request.seat_id
            )));
        }

        let available = state
            .available
            .clone()
            .unwrap_or_else(|| request.devices.clone());
        let own: BTreeSet<String> = request.devices.iter().cloned().collect();
        let missing: Vec<String> = own
            .iter()
            .filter(|name| !available.iter().any(|device| device == *name))
            .cloned()
            .collect();
        if !missing.is_empty() {
            state
                .events
                .push(format!("target-failed {}", request.seat_id));
            return Err(MultiseatError::InputTargeting(format!(
                "own uinput device(s) not visible: {}",
                missing.join(", ")
            )));
        }

        let mut enabled: Vec<String> = own.iter().cloned().collect();
        enabled.sort();
        let mut disabled: Vec<String> = available
            .iter()
            .filter(|name| is_uinput_name(name) && !own.contains(*name))
            .cloned()
            .collect();
        disabled.sort();
        disabled.dedup();

        let targeting = SeatInputTargeting {
            seat_id: request.seat_id.clone(),
            display: request.display.clone(),
            enabled,
            disabled,
        };
        state
            .events
            .push(format!("target {} {}", request.seat_id, request.display));
        state
            .applied
            .insert(request.seat_id.clone(), targeting.clone());
        Ok(targeting)
    }

    fn release(&mut self, seat_id: &str) -> Result<SeatInputTargetingRelease, MultiseatError> {
        let mut state = self.state.lock().expect("fake targeter lock");
        if let Some(message) = state.fail_next_release.take() {
            state
                .events
                .push(format!("target-release-failed {seat_id}"));
            return Err(MultiseatError::InputTargeting(message));
        }
        let reenabled = state
            .applied
            .remove(seat_id)
            .map(|targeting| targeting.disabled)
            .unwrap_or_default();
        state.released.push(seat_id.to_owned());
        state.events.push(format!("target-release {seat_id}"));
        Ok(SeatInputTargetingRelease {
            seat_id: seat_id.to_owned(),
            reenabled,
            warnings: Vec::new(),
        })
    }

    fn status_for(&self, seat_id: &str) -> Option<SeatInputTargeting> {
        self.state
            .lock()
            .expect("fake targeter lock")
            .applied
            .get(seat_id)
            .cloned()
    }

    fn clear(&mut self, seat_id: &str) {
        let mut state = self.state.lock().expect("fake targeter lock");
        state.applied.remove(seat_id);
        state.cleared.push(seat_id.to_owned());
        state.events.push(format!("target-clear {seat_id}"));
    }
}

/// One card configured in a [`FakeDrmLeaseBackend`].
#[derive(Clone, Debug)]
pub struct FakeDrmLeaseCard {
    /// Card summary returned by enumeration and inspection.
    pub card: crate::drm_lease::DrmCard,
    /// Connector name to object id.
    pub connector_ids: BTreeMap<String, u32>,
    /// CRTC object ids.
    pub crtc_ids: Vec<u32>,
    /// Plane object id to the CRTC ids it can drive.
    pub planes: BTreeMap<u32, Vec<u32>>,
}

/// One in-memory lease record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FakeLeaseRecord {
    /// Kernel lessee id.
    pub lessee_id: u32,
    /// Card path.
    pub card: std::path::PathBuf,
    /// Connector name.
    pub connector: String,
    /// Connector object id.
    pub connector_id: u32,
    /// CRTC object id.
    pub crtc_id: u32,
    /// Explicitly requested plane object id.
    pub plane_id: Option<u32>,
}

/// Injected one-shot lease failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FakeDrmLeaseFailure {
    /// No card is available.
    NoDrm,
    /// Opening the card is denied.
    NoPermission,
    /// Leasing is unsupported.
    Unsupported,
    /// A named object does not exist.
    UnknownObject,
    /// The object is already leased.
    AlreadyLeased,
    /// The caller is not master / lacks CAP_SYS_ADMIN.
    CapabilityRequired,
}

#[derive(Debug, Default)]
struct FakeDrmLeaseState {
    cards: Vec<FakeDrmLeaseCard>,
    leases: BTreeMap<u32, FakeLeaseRecord>,
    next_lessee: u32,
    failure: Option<FakeDrmLeaseFailure>,
    requests: Vec<crate::drm_lease::DrmLeaseRequest>,
    revoked: Vec<u32>,
    dropped: Vec<u32>,
    fd_enabled: bool,
}

/// Test-only DRM lease backend.
///
/// It validates connector/CRTC membership, rejects a duplicate object set, and
/// records create/revoke/drop events. It never opens `/dev/dri` or issues an
/// ioctl.
#[derive(Clone, Debug)]
pub struct FakeDrmLeaseBackend {
    state: Arc<Mutex<FakeDrmLeaseState>>,
}

impl Default for FakeDrmLeaseBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeDrmLeaseBackend {
    /// A fake with one virtual `vkms` card (`card9`), connector `Virtual-1`
    /// (object 101), and CRTC object 201.
    pub fn new() -> Self {
        let mut backend = Self {
            state: Arc::new(Mutex::new(FakeDrmLeaseState {
                next_lessee: 0,
                fd_enabled: true,
                ..FakeDrmLeaseState::default()
            })),
        };
        backend.add_card(
            "card9",
            "/dev/dri/card9",
            "vkms",
            true,
            "Virtual-1",
            101,
            &[201],
        );
        backend
    }

    /// Add or replace a card and its object ids.
    pub fn add_card(
        &mut self,
        name: &str,
        path: &str,
        driver: &str,
        virtual_card: bool,
        connector: &str,
        connector_id: u32,
        crtc_ids: &[u32],
    ) -> &mut Self {
        let connectors = vec![crate::drm_lease::DrmLeaseConnector {
            name: connector.to_owned(),
            status: crate::drm::DrmStatus::Connected,
            modes: vec!["1024x768".to_owned()],
            object_id: Some(connector_id),
            crtc_id: crtc_ids.first().copied(),
        }];
        let mut connector_ids = BTreeMap::new();
        connector_ids.insert(connector.to_owned(), connector_id);
        let mut state = self.state.lock().expect("fake drm lease lock");
        if let Some(existing) = state.cards.iter_mut().find(|card| card.card.name == name) {
            existing.card.driver = driver.to_owned();
            existing.card.virtual_card = virtual_card;
            existing.card.connectors.extend(connectors.iter().cloned());
            existing
                .connector_ids
                .extend(connector_ids.iter().map(|(name, id)| (name.clone(), *id)));
            existing.crtc_ids = crtc_ids.to_vec();
            drop(state);
            return self;
        }
        state.cards.push(FakeDrmLeaseCard {
            card: crate::drm_lease::DrmCard {
                name: name.to_owned(),
                path: std::path::PathBuf::from(path),
                sysfs_path: std::path::PathBuf::from(format!("/sys/class/drm/{name}")),
                driver: driver.to_owned(),
                virtual_card,
                connectors,
                crtcs: crtc_ids.to_vec(),
            },
            connector_ids,
            crtc_ids: crtc_ids.to_vec(),
            planes: BTreeMap::new(),
        });
        drop(state);
        self
    }

    /// Add a plane object that can drive the given CRTC ids.
    pub fn add_plane(&mut self, card_name: &str, plane_id: u32, crtc_ids: &[u32]) -> &mut Self {
        let mut state = self.state.lock().expect("fake drm lease lock");
        if let Some(card) = state
            .cards
            .iter_mut()
            .find(|card| card.card.name == card_name)
        {
            card.planes.insert(plane_id, crtc_ids.to_vec());
        }
        drop(state);
        self
    }

    /// Disable the lessee fd on subsequently acquired leases.
    pub fn without_fd(&mut self) -> &mut Self {
        self.state.lock().expect("fake drm lease lock").fd_enabled = false;
        self
    }

    /// Inject one typed failure on the next acquire.
    pub fn fail_next(&self, failure: FakeDrmLeaseFailure) {
        self.state.lock().expect("fake drm lease lock").failure = Some(failure);
    }

    /// Inject a no-DRM failure on the next acquire.
    pub fn fail_next_no_drm(&self) {
        self.fail_next(FakeDrmLeaseFailure::NoDrm);
    }

    /// Inject a no-permission failure on the next acquire.
    pub fn fail_next_no_permission(&self) {
        self.fail_next(FakeDrmLeaseFailure::NoPermission);
    }

    /// Inject an unsupported-lease failure on the next acquire.
    pub fn fail_next_unsupported(&self) {
        self.fail_next(FakeDrmLeaseFailure::Unsupported);
    }

    /// Inject an unknown-object failure on the next acquire.
    pub fn fail_next_unknown_object(&self) {
        self.fail_next(FakeDrmLeaseFailure::UnknownObject);
    }

    /// Inject an already-leased failure on the next acquire.
    pub fn fail_next_already_leased(&self) {
        self.fail_next(FakeDrmLeaseFailure::AlreadyLeased);
    }

    /// Inject a capability-required failure on the next acquire.
    pub fn fail_next_capability_required(&self) {
        self.fail_next(FakeDrmLeaseFailure::CapabilityRequired);
    }

    /// Acquire requests recorded so far.
    pub fn requests(&self) -> Vec<crate::drm_lease::DrmLeaseRequest> {
        self.state
            .lock()
            .expect("fake drm lease lock")
            .requests
            .clone()
    }

    /// Active leases.
    pub fn active_leases(&self) -> Vec<FakeLeaseRecord> {
        self.state
            .lock()
            .expect("fake drm lease lock")
            .leases
            .values()
            .cloned()
            .collect()
    }

    /// Number of active leases.
    pub fn active_count(&self) -> usize {
        self.state.lock().expect("fake drm lease lock").leases.len()
    }

    /// Lessee ids revoked explicitly, in order.
    pub fn revoked(&self) -> Vec<u32> {
        self.state
            .lock()
            .expect("fake drm lease lock")
            .revoked
            .clone()
    }

    /// Lessee ids released by dropping an active handle, in order.
    pub fn dropped(&self) -> Vec<u32> {
        self.state
            .lock()
            .expect("fake drm lease lock")
            .dropped
            .clone()
    }

    fn resolve_card(
        state: &FakeDrmLeaseState,
        selector: &crate::drm_lease::DrmCardSelector,
    ) -> Result<FakeDrmLeaseCard, crate::drm_lease::DrmLeaseError> {
        match selector {
            crate::drm_lease::DrmCardSelector::AutoVirtual => state
                .cards
                .iter()
                .find(|card| card.card.virtual_card)
                .cloned()
                .ok_or_else(|| crate::drm_lease::DrmLeaseError::NoDrm {
                    reason: "fake backend has no virtual card".to_owned(),
                }),
            crate::drm_lease::DrmCardSelector::Explicit(path) => state
                .cards
                .iter()
                .find(|card| card.card.path == *path)
                .cloned()
                .ok_or_else(|| {
                    crate::drm_lease::DrmLeaseError::UnknownCard(path.display().to_string())
                }),
        }
    }
}

impl crate::drm_lease::DrmLeaseBackend for FakeDrmLeaseBackend {
    fn enumerate(&self) -> Result<Vec<crate::drm_lease::DrmCard>, crate::drm_lease::DrmLeaseError> {
        Ok(self
            .state
            .lock()
            .expect("fake drm lease lock")
            .cards
            .iter()
            .map(|card| card.card.clone())
            .collect())
    }

    fn inspect(
        &self,
        card: &crate::drm_lease::DrmCard,
    ) -> Result<crate::drm_lease::DrmCard, crate::drm_lease::DrmLeaseError> {
        Ok(card.clone())
    }

    fn acquire(
        &self,
        request: &crate::drm_lease::DrmLeaseRequest,
    ) -> Result<Box<dyn crate::drm_lease::DrmLeaseHandle>, crate::drm_lease::DrmLeaseError> {
        use crate::drm_lease::{
            DrmCardSelector, DrmCrtcSelector, DrmLeaseError, DrmObjectKind, DrmPlaneSelector,
        };
        let mut state = self.state.lock().expect("fake drm lease lock");
        state.requests.push(request.clone());
        if let Some(failure) = state.failure.take() {
            let card = request.card.display_name();
            return Err(match failure {
                FakeDrmLeaseFailure::NoDrm => DrmLeaseError::NoDrm {
                    reason: "injected no-DRM failure".to_owned(),
                },
                FakeDrmLeaseFailure::NoPermission => DrmLeaseError::NoPermission {
                    path: card,
                    source: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
                },
                FakeDrmLeaseFailure::Unsupported => DrmLeaseError::Unsupported {
                    card,
                    reason: "injected unsupported lease".to_owned(),
                },
                FakeDrmLeaseFailure::UnknownObject => DrmLeaseError::UnknownObject {
                    card,
                    kind: DrmObjectKind::Connector,
                    id: 0,
                },
                FakeDrmLeaseFailure::AlreadyLeased => DrmLeaseError::AlreadyLeased {
                    card,
                    kind: DrmObjectKind::Connector,
                    id: 0,
                },
                FakeDrmLeaseFailure::CapabilityRequired => DrmLeaseError::CapabilityRequired {
                    card,
                    reason: "injected capability failure".to_owned(),
                },
            });
        }
        let card = Self::resolve_card(&state, &request.card)?;
        if matches!(request.card, DrmCardSelector::AutoVirtual) && !card.card.virtual_card {
            return Err(DrmLeaseError::ExplicitCardRequired {
                card: card.card.path.display().to_string(),
            });
        }
        let connector_id = card
            .connector_ids
            .get(&request.connector)
            .copied()
            .ok_or_else(|| DrmLeaseError::UnknownConnector {
                card: card.card.path.display().to_string(),
                connector: request.connector.clone(),
            })?;
        let crtc_id = match request.crtc {
            DrmCrtcSelector::Id(id) => {
                if !card.crtc_ids.contains(&id) {
                    return Err(DrmLeaseError::UnknownObject {
                        card: card.card.path.display().to_string(),
                        kind: DrmObjectKind::Crtc,
                        id,
                    });
                }
                id
            }
            DrmCrtcSelector::Auto => {
                *card
                    .crtc_ids
                    .first()
                    .ok_or_else(|| DrmLeaseError::UnknownObject {
                        card: card.card.path.display().to_string(),
                        kind: DrmObjectKind::Crtc,
                        id: 0,
                    })?
            }
        };
        let plane_id = match request.plane {
            DrmPlaneSelector::Auto => None,
            DrmPlaneSelector::Id(id) => {
                let Some(crtcs) = card.planes.get(&id) else {
                    return Err(DrmLeaseError::UnknownObject {
                        card: card.card.path.display().to_string(),
                        kind: DrmObjectKind::Plane,
                        id,
                    });
                };
                if !crtcs.contains(&crtc_id) {
                    return Err(DrmLeaseError::IncompatiblePlane {
                        card: card.card.path.display().to_string(),
                        plane: id,
                        crtc: crtc_id,
                    });
                }
                Some(id)
            }
        };
        let plane_ids: Vec<u32> = card
            .planes
            .iter()
            .filter(|(_, crtcs)| crtcs.contains(&crtc_id))
            .map(|(id, _)| *id)
            .collect();
        if state.leases.values().any(|lease| {
            lease.card == card.card.path
                && (lease.connector_id == connector_id
                    || lease.crtc_id == crtc_id
                    || plane_id.is_some_and(|id| lease.plane_id == Some(id)))
        }) {
            return Err(DrmLeaseError::AlreadyLeased {
                card: card.card.path.display().to_string(),
                kind: DrmObjectKind::Connector,
                id: connector_id,
            });
        }
        state.next_lessee = state.next_lessee.saturating_add(1);
        let lessee_id = state.next_lessee.max(1);
        let fd = if state.fd_enabled {
            nix::unistd::pipe().ok().map(|(read, _write)| read)
        } else {
            None
        };
        let record = FakeLeaseRecord {
            lessee_id,
            card: card.card.path.clone(),
            connector: request.connector.clone(),
            connector_id,
            crtc_id,
            plane_id,
        };
        state.leases.insert(lessee_id, record.clone());
        drop(state);
        Ok(Box::new(FakeDrmLeaseHandle {
            record,
            plane_ids,
            fd,
            active: true,
            state: Arc::clone(&self.state),
        }))
    }

    fn list_lease_ids(
        &self,
        card: &crate::drm_lease::DrmCardSelector,
    ) -> Result<Vec<u32>, crate::drm_lease::DrmLeaseError> {
        let state = self.state.lock().expect("fake drm lease lock");
        let card = Self::resolve_card(&state, card)?;
        Ok(state
            .leases
            .values()
            .filter(|lease| lease.card == card.card.path)
            .map(|lease| lease.lessee_id)
            .collect())
    }
}

struct FakeDrmLeaseHandle {
    record: FakeLeaseRecord,
    plane_ids: Vec<u32>,
    fd: Option<std::os::fd::OwnedFd>,
    active: bool,
    state: Arc<Mutex<FakeDrmLeaseState>>,
}

impl crate::drm_lease::DrmLeaseHandle for FakeDrmLeaseHandle {
    fn card(&self) -> &std::path::Path {
        &self.record.card
    }

    fn connector(&self) -> &str {
        &self.record.connector
    }

    fn lessee_id(&self) -> u32 {
        self.record.lessee_id
    }

    fn connector_id(&self) -> u32 {
        self.record.connector_id
    }

    fn crtc_id(&self) -> u32 {
        self.record.crtc_id
    }

    fn plane_id(&self) -> Option<u32> {
        self.record.plane_id
    }

    fn plane_ids(&self) -> &[u32] {
        &self.plane_ids
    }

    fn fd(&self) -> Option<std::os::fd::RawFd> {
        use std::os::fd::AsRawFd;
        self.fd.as_ref().map(AsRawFd::as_raw_fd)
    }

    fn as_fd(&self) -> Option<std::os::fd::BorrowedFd<'_>> {
        use std::os::fd::AsFd;
        self.fd.as_ref().map(AsFd::as_fd)
    }

    fn set_inheritable(&self, _inheritable: bool) -> Result<(), crate::drm_lease::DrmLeaseError> {
        Ok(())
    }

    fn validate(
        &self,
    ) -> Result<crate::drm_lease::DrmLeaseObjects, crate::drm_lease::DrmLeaseError> {
        if !self.active {
            return Err(crate::drm_lease::DrmLeaseError::Released(
                self.record.lessee_id,
            ));
        }
        Ok(crate::drm_lease::DrmLeaseObjects {
            connectors: vec![self.record.connector_id],
            crtcs: vec![self.record.crtc_id],
            planes: self.plane_ids.clone(),
        })
    }

    fn list_lease_ids(&self) -> Result<Vec<u32>, crate::drm_lease::DrmLeaseError> {
        if !self.active {
            return Err(crate::drm_lease::DrmLeaseError::Released(
                self.record.lessee_id,
            ));
        }
        let state = self.state.lock().expect("fake drm lease lock");
        Ok(state
            .leases
            .values()
            .filter(|lease| lease.card == self.record.card)
            .map(|lease| lease.lessee_id)
            .collect())
    }

    fn revoke(&mut self) -> Result<(), crate::drm_lease::DrmLeaseError> {
        if !self.active {
            return Ok(());
        }
        let mut state = self.state.lock().expect("fake drm lease lock");
        state.leases.remove(&self.record.lessee_id);
        state.revoked.push(self.record.lessee_id);
        drop(state);
        self.active = false;
        self.fd = None;
        Ok(())
    }

    fn is_active(&self) -> bool {
        self.active && self.fd.is_some()
    }
}

impl Drop for FakeDrmLeaseHandle {
    fn drop(&mut self) {
        if self.active {
            let mut state = self.state.lock().expect("fake drm lease lock");
            state.leases.remove(&self.record.lessee_id);
            state.dropped.push(self.record.lessee_id);
        }
    }
}

/// One in-memory card configured for [`FakeDrmScanoutBackend`].
#[derive(Clone, Debug)]
pub struct FakeDrmScanoutCard {
    /// Card name.
    pub name: String,
    /// Card path.
    pub path: std::path::PathBuf,
    /// Driver name.
    pub driver: String,
    /// Whether automatic selection may use this card.
    pub virtual_card: bool,
    /// Connector name.
    pub connector: String,
    /// Connector object id.
    pub connector_id: u32,
    /// CRTC object id.
    pub crtc_id: u32,
    /// Advertised mode names.
    pub modes: Vec<String>,
    /// Plane object ids that can drive this card's CRTC.
    pub plane_ids: Vec<u32>,
}

/// Injected one-shot scanout failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FakeDrmScanoutFailure {
    /// No card is available.
    NoDrm,
    /// Opening the card is denied.
    NoPermission,
    /// The caller is not master / lacks CAP_SYS_ADMIN.
    CapabilityRequired,
    /// The driver is not vkms.
    UnsupportedVkms,
    /// The connector is missing or has no modes.
    UnsupportedConnector,
    /// The requested mode does not exist / the modeset failed.
    ModeFailure,
    /// Dumb-buffer allocation failed.
    BufferFailure,
    /// Framebuffer creation failed.
    FramebufferFailure,
    /// Post-modeset verification fails.
    VerificationFailure,
    /// Page flip is rejected after a successful commit.
    PageFlipUnavailable,
    /// Atomic KMS is unsupported; the proof falls back to legacy.
    AtomicUnsupported,
    /// No compatible plane is available for the selected CRTC.
    UnsupportedPlane,
    /// The selected plane does not support XRGB8888.
    InvalidFormat,
    /// A required atomic property is missing.
    MissingAtomicProperty,
}

#[derive(Debug, Default)]
struct FakeDrmScanoutState {
    cards: Vec<FakeDrmScanoutCard>,
    failure: Option<FakeDrmScanoutFailure>,
    events: Vec<String>,
    active: bool,
    next_lessee: u32,
    next_framebuffer: u32,
    next_buffer: u32,
    verification_fails: bool,
}

/// Test-only KMS scanout backend.
///
/// It models lease creation, dumb-buffer allocation/fill, framebuffer creation,
/// `set_crtc`, page flip, verification, and teardown in memory. It never opens
/// `/dev/dri`, mmaps a real buffer, or changes any display.
#[derive(Clone, Debug)]
pub struct FakeDrmScanoutBackend {
    state: Arc<Mutex<FakeDrmScanoutState>>,
}

impl Default for FakeDrmScanoutBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeDrmScanoutBackend {
    /// A fake with one vkms card (`card9`), connector `Virtual-1` (101), CRTC
    /// 201, and modes `1024x768` and `800x600`.
    pub fn new() -> Self {
        let mut backend = Self {
            state: Arc::new(Mutex::new(FakeDrmScanoutState {
                next_lessee: 7,
                next_framebuffer: 51,
                next_buffer: 41,
                ..FakeDrmScanoutState::default()
            })),
        };
        backend.add_vkms_card(
            "card9",
            "/dev/dri/card9",
            "Virtual-1",
            101,
            201,
            &["1024x768", "800x600"],
        );
        backend
    }

    /// Add a virtual vkms card.
    pub fn add_vkms_card(
        &mut self,
        name: &str,
        path: &str,
        connector: &str,
        connector_id: u32,
        crtc_id: u32,
        modes: &[&str],
    ) -> &mut Self {
        self.add_card(
            name,
            path,
            "vkms",
            true,
            connector,
            connector_id,
            crtc_id,
            modes,
        )
    }

    /// Add a card with explicit virtual/real policy.
    #[allow(clippy::too_many_arguments)]
    pub fn add_card(
        &mut self,
        name: &str,
        path: &str,
        driver: &str,
        virtual_card: bool,
        connector: &str,
        connector_id: u32,
        crtc_id: u32,
        modes: &[&str],
    ) -> &mut Self {
        let card = FakeDrmScanoutCard {
            name: name.to_owned(),
            path: std::path::PathBuf::from(path),
            driver: driver.to_owned(),
            virtual_card,
            connector: connector.to_owned(),
            connector_id,
            crtc_id,
            modes: modes.iter().map(|mode| (*mode).to_owned()).collect(),
            plane_ids: vec![crtc_id.saturating_add(100)],
        };
        let mut state = self.state.lock().expect("fake scanout lock");
        state.cards.retain(|existing| existing.name != name);
        state.cards.push(card);
        drop(state);
        self
    }

    /// Add another plane id to a fake scanout card.
    pub fn add_plane(&mut self, name: &str, plane_id: u32) -> &mut Self {
        let mut state = self.state.lock().expect("fake scanout lock");
        if let Some(card) = state.cards.iter_mut().find(|card| card.name == name) {
            card.plane_ids.push(plane_id);
        }
        drop(state);
        self
    }

    /// Remove all cards (simulates a host with no DRM).
    pub fn without_cards(&mut self) -> &mut Self {
        self.state.lock().expect("fake scanout lock").cards.clear();
        self
    }

    /// Inject one typed failure on the next start.
    pub fn fail_next(&self, failure: FakeDrmScanoutFailure) {
        self.state.lock().expect("fake scanout lock").failure = Some(failure);
    }

    /// Events recorded so far, in order.
    pub fn events(&self) -> Vec<String> {
        self.state.lock().expect("fake scanout lock").events.clone()
    }

    /// Whether a scanout is currently active.
    pub fn active(&self) -> bool {
        self.state.lock().expect("fake scanout lock").active
    }
}

impl crate::drm_scanout::DrmScanoutBackend for FakeDrmScanoutBackend {
    fn start(
        &self,
        request: &crate::drm_scanout::DrmScanoutRequest,
    ) -> Result<Box<dyn crate::drm_scanout::DrmScanoutHandle>, crate::drm_scanout::DrmScanoutError>
    {
        use crate::drm_lease::DrmCardSelector;
        use crate::drm_scanout::{
            DrmScanoutCommit, DrmScanoutError, DrmScanoutNote, DrmScanoutNoteKind,
            DrmScanoutStatus, SCANOUT_PATTERN,
        };
        let mut state = self.state.lock().expect("fake scanout lock");
        state.events.push(format!(
            "start {}:{}",
            request.lease.card.display_name(),
            request.lease.connector
        ));
        let failure = state.failure.take();
        let mut page_flip_unavailable = false;
        let mut atomic_unsupported = false;
        if let Some(failure) = failure {
            let card = request.lease.card.display_name();
            match failure {
                FakeDrmScanoutFailure::PageFlipUnavailable => {
                    // Keep the commit: the real backend records this as a
                    // non-fatal note rather than a start failure.
                    page_flip_unavailable = true;
                }
                FakeDrmScanoutFailure::AtomicUnsupported => {
                    atomic_unsupported = true;
                }
                FakeDrmScanoutFailure::UnsupportedPlane => {
                    return Err(DrmScanoutError::UnsupportedPlane {
                        card,
                        crtc: 201,
                        reason: "injected missing plane".to_owned(),
                    })
                }
                FakeDrmScanoutFailure::InvalidFormat => {
                    return Err(DrmScanoutError::InvalidFormat {
                        card,
                        plane: 301,
                        format: "XR24".to_owned(),
                        reason: "injected format mismatch".to_owned(),
                    })
                }
                FakeDrmScanoutFailure::MissingAtomicProperty => {
                    return Err(DrmScanoutError::MissingAtomicProperty {
                        card,
                        object: "plane".to_owned(),
                        id: 301,
                        property: "FB_ID".to_owned(),
                    })
                }
                FakeDrmScanoutFailure::NoDrm => {
                    return Err(DrmScanoutError::NoDrm {
                        reason: "injected no-DRM failure".to_owned(),
                    })
                }
                FakeDrmScanoutFailure::NoPermission => {
                    return Err(DrmScanoutError::NoPermission {
                        path: card,
                        source: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
                    })
                }
                FakeDrmScanoutFailure::CapabilityRequired => {
                    return Err(DrmScanoutError::CapabilityRequired {
                        card,
                        reason: "injected missing master".to_owned(),
                    })
                }
                FakeDrmScanoutFailure::UnsupportedVkms => {
                    return Err(DrmScanoutError::UnsupportedVkms {
                        card,
                        driver: "xe".to_owned(),
                    })
                }
                FakeDrmScanoutFailure::UnsupportedConnector => {
                    return Err(DrmScanoutError::UnsupportedConnector {
                        card,
                        connector: request.lease.connector.clone(),
                        reason: "injected missing connector".to_owned(),
                    })
                }
                FakeDrmScanoutFailure::ModeFailure => {
                    return Err(DrmScanoutError::ModeFailed {
                        card,
                        connector: request.lease.connector.clone(),
                        mode: request.mode.clone().unwrap_or_else(|| "auto".to_owned()),
                        reason: "injected modeset failure".to_owned(),
                    })
                }
                FakeDrmScanoutFailure::BufferFailure => {
                    return Err(DrmScanoutError::BufferFailed {
                        card,
                        reason: "injected dumb-buffer failure".to_owned(),
                    })
                }
                FakeDrmScanoutFailure::FramebufferFailure => {
                    return Err(DrmScanoutError::FramebufferFailed {
                        card,
                        reason: "injected framebuffer failure".to_owned(),
                    })
                }
                FakeDrmScanoutFailure::VerificationFailure => {
                    state.verification_fails = true;
                }
            }
        }

        let card = match &request.lease.card {
            DrmCardSelector::AutoVirtual => state
                .cards
                .iter()
                .find(|card| card.virtual_card)
                .cloned()
                .ok_or_else(|| DrmScanoutError::NoDrm {
                    reason: "fake backend has no virtual card".to_owned(),
                })?,
            DrmCardSelector::Explicit(path) => state
                .cards
                .iter()
                .find(|card| card.path == *path)
                .cloned()
                .ok_or_else(|| DrmScanoutError::NoDrm {
                    reason: format!("unknown DRM card {}", path.display()),
                })?,
        };
        crate::drm_scanout::ensure_scanout_card(
            &crate::drm_lease::DrmCard {
                name: card.name.clone(),
                path: card.path.clone(),
                sysfs_path: std::path::PathBuf::from(format!("/sys/class/drm/{}", card.name)),
                driver: card.driver.clone(),
                virtual_card: card.virtual_card,
                connectors: Vec::new(),
                crtcs: vec![card.crtc_id],
            },
            &request.lease.card,
        )?;
        if card.connector != request.lease.connector {
            return Err(DrmScanoutError::UnsupportedConnector {
                card: card.path.display().to_string(),
                connector: request.lease.connector.clone(),
                reason: format!("fake card has connector {:?}", card.connector),
            });
        }
        let mode = match request.mode.as_deref() {
            Some(requested) => card
                .modes
                .iter()
                .find(|mode| mode.eq_ignore_ascii_case(requested.trim()))
                .cloned()
                .ok_or_else(|| DrmScanoutError::ModeFailed {
                    card: card.path.display().to_string(),
                    connector: card.connector.clone(),
                    mode: requested.to_owned(),
                    reason: "mode is not advertised".to_owned(),
                })?,
            None => card.modes.first().cloned().ok_or_else(|| {
                DrmScanoutError::UnsupportedConnector {
                    card: card.path.display().to_string(),
                    connector: card.connector.clone(),
                    reason: "connector advertises no modes".to_owned(),
                }
            })?,
        };
        let (width, height) =
            parse_mode_size(&mode).ok_or_else(|| DrmScanoutError::ModeFailed {
                card: card.path.display().to_string(),
                connector: card.connector.clone(),
                mode: mode.clone(),
                reason: "fake mode name is not WIDTHxHEIGHT".to_owned(),
            })?;
        let pitch = width * 4;
        let mut bytes = vec![0u8; (pitch * height) as usize];
        let checksum = crate::drm_scanout::fill_pattern(&mut bytes, width, height, pitch);

        let requested_plane = request.lease.plane_id();
        if let Some(id) = requested_plane {
            if !card.plane_ids.contains(&id) {
                return Err(DrmScanoutError::UnsupportedPlane {
                    card: card.path.display().to_string(),
                    crtc: card.crtc_id,
                    reason: format!("requested plane {id} is not leaseable on this card"),
                });
            }
        }
        let commit = if request.force_legacy || atomic_unsupported {
            DrmScanoutCommit::Legacy
        } else {
            DrmScanoutCommit::Atomic
        };
        let plane_id = if commit == DrmScanoutCommit::Atomic {
            Some(
                requested_plane
                    .or_else(|| card.plane_ids.first().copied())
                    .ok_or_else(|| DrmScanoutError::UnsupportedPlane {
                        card: card.path.display().to_string(),
                        crtc: card.crtc_id,
                        reason: "the fake card exposes no plane".to_owned(),
                    })?,
            )
        } else {
            requested_plane
        };

        state.next_lessee = state.next_lessee.saturating_add(1);
        state.next_framebuffer = state.next_framebuffer.saturating_add(1);
        state.next_buffer = state.next_buffer.saturating_add(1);
        let lessee_id = state.next_lessee;
        let framebuffer_id = state.next_framebuffer;
        let buffer_handle = state.next_buffer;
        state.events.push(format!("lease {lessee_id}"));
        state
            .events
            .push(format!("buffer {buffer_handle} {width}x{height}"));
        state.events.push(format!("fill {checksum}"));
        state.events.push(format!("framebuffer {framebuffer_id}"));
        match commit {
            DrmScanoutCommit::Atomic => {
                let plane = plane_id.unwrap_or(0);
                state.events.push(format!(
                    "atomic_property connector CRTC_ID {}",
                    card.crtc_id
                ));
                state
                    .events
                    .push("atomic_property crtc ACTIVE 1".to_owned());
                state.events.push(format!(
                    "atomic_property crtc MODE_ID {}",
                    framebuffer_id + 1000
                ));
                state
                    .events
                    .push(format!("atomic_property plane FB_ID {framebuffer_id}"));
                state
                    .events
                    .push(format!("atomic_property plane CRTC_ID {}", card.crtc_id));
                state.events.push(format!(
                    "atomic_property plane SRC_W {}",
                    u64::from(width) << 16
                ));
                state.events.push(format!(
                    "atomic_commit {} {plane} {framebuffer_id} {mode}",
                    card.crtc_id
                ));
                state.events.push(format!(
                    "atomic_page_flip {} {framebuffer_id}",
                    card.crtc_id
                ));
            }
            DrmScanoutCommit::Legacy => {
                state
                    .events
                    .push(format!("set_crtc {} {framebuffer_id} {mode}", card.crtc_id));
                state
                    .events
                    .push(format!("page_flip {} {framebuffer_id}", card.crtc_id));
            }
        }
        state.active = true;

        let mut status = DrmScanoutStatus {
            card: card.path.display().to_string(),
            connector: card.connector.clone(),
            connector_id: Some(card.connector_id),
            crtc_id: Some(card.crtc_id),
            plane_id,
            commit,
            lessee_id: Some(lessee_id),
            lessee_fd: Some(30 + i32::try_from(lessee_id).unwrap_or(0)),
            mode,
            width,
            height,
            pitch,
            framebuffer_id: Some(framebuffer_id),
            buffer_handle: Some(buffer_handle),
            pattern: SCANOUT_PATTERN.to_owned(),
            pattern_checksum: Some(checksum),
            page_flipped: !page_flip_unavailable,
            verification: None,
            applied: true,
            released: false,
            notes: Vec::new(),
            error: None,
        };
        status.push_proof_note();
        status.push_note(DrmScanoutNote::new(
            DrmScanoutNoteKind::LesseeFd,
            format!("fake scanout uses lessee fd {}", 30 + lessee_id),
        ));
        if let Some(plane_id) = plane_id {
            status.push_note(DrmScanoutNote::new(
                DrmScanoutNoteKind::PlaneLeased,
                format!(
                    "fake plane {plane_id} was committed through {} KMS",
                    commit.as_str()
                ),
            ));
        }
        if request.force_legacy {
            status.push_note(DrmScanoutNote::new(
                DrmScanoutNoteKind::LegacyCommitForced,
                "fake scanout forced the legacy commit path",
            ));
        } else if atomic_unsupported {
            status.push_note(DrmScanoutNote::new(
                DrmScanoutNoteKind::LegacyCommitFallback,
                "fake atomic KMS was unsupported; fell back to set_crtc/page_flip",
            ));
        }
        if !card.virtual_card {
            status.push_note(DrmScanoutNote::new(
                DrmScanoutNoteKind::RealGpuNamed,
                format!(
                    "fake real card {} was named explicitly",
                    card.path.display()
                ),
            ));
        }
        let verification_fails = state.verification_fails;
        state.verification_fails = false;
        drop(state);
        let mode_for_record = status.mode.clone();

        Ok(Box::new(FakeDrmScanoutHandle {
            status,
            record: FakeScanoutRecord {
                card: card.path.clone(),
                connector: card.connector.clone(),
                crtc_id: card.crtc_id,
                plane_id,
                commit,
                mode: mode_for_record,
                width,
                height,
                pitch,
                framebuffer_id,
                buffer_handle,
                lessee_id,
                checksum,
            },
            verification_fails,
            active: true,
            state: Arc::clone(&self.state),
        }))
    }
}

#[derive(Clone, Debug)]
struct FakeScanoutRecord {
    card: std::path::PathBuf,
    connector: String,
    crtc_id: u32,
    plane_id: Option<u32>,
    commit: crate::drm_scanout::DrmScanoutCommit,
    mode: String,
    width: u32,
    height: u32,
    pitch: u32,
    framebuffer_id: u32,
    buffer_handle: u32,
    lessee_id: u32,
    checksum: u32,
}

struct FakeDrmScanoutHandle {
    status: crate::drm_scanout::DrmScanoutStatus,
    record: FakeScanoutRecord,
    verification_fails: bool,
    active: bool,
    state: Arc<Mutex<FakeDrmScanoutState>>,
}

impl crate::drm_scanout::DrmScanoutHandle for FakeDrmScanoutHandle {
    fn status(&self) -> &crate::drm_scanout::DrmScanoutStatus {
        &self.status
    }

    fn verify(
        &mut self,
    ) -> Result<crate::drm_scanout::DrmScanoutVerification, crate::drm_scanout::DrmScanoutError>
    {
        use crate::drm_scanout::{DrmScanoutError, DrmScanoutVerification};
        if !self.active {
            return Err(DrmScanoutError::Released(
                self.record.card.display().to_string(),
            ));
        }
        if self.verification_fails {
            let error = DrmScanoutError::VerificationFailed {
                card: self.record.card.display().to_string(),
                connector: self.record.connector.clone(),
                message: "injected verification mismatch".to_owned(),
            };
            self.status.mark_error(&error);
            return Err(error);
        }
        let verification = DrmScanoutVerification {
            crtc_id: self.record.crtc_id,
            crtc_mode: Some(self.record.mode.clone()),
            crtc_framebuffer: Some(self.record.framebuffer_id),
            crtc_position: (0, 0),
            connector_state: "connected".to_owned(),
            connector_crtc: Some(self.record.crtc_id),
            plane_id: self.record.plane_id,
            plane_crtc: self.record.plane_id.map(|_| self.record.crtc_id),
            plane_framebuffer: self.record.plane_id.map(|_| self.record.framebuffer_id),
            framebuffer_id: self.record.framebuffer_id,
            framebuffer_size: (self.record.width, self.record.height),
            framebuffer_pitch: self.record.pitch,
            framebuffer_bpp: 32,
            framebuffer_depth: 24,
            framebuffer_buffer_handle: Some(self.record.buffer_handle),
            buffer_size: (self.record.width, self.record.height),
            buffer_pitch: self.record.pitch,
            buffer_handle: self.record.buffer_handle,
            pattern_checksum: self.record.checksum,
            ok: true,
            mismatch: None,
        };
        self.status.verification = Some(verification.clone());
        Ok(verification)
    }

    fn teardown(&mut self) -> Result<(), crate::drm_scanout::DrmScanoutError> {
        if !self.active {
            return Ok(());
        }
        let mut state = self.state.lock().expect("fake scanout lock");
        match self.record.commit {
            crate::drm_scanout::DrmScanoutCommit::Atomic => {
                state.events.push(format!(
                    "atomic_disable {} {}",
                    self.record.crtc_id,
                    self.record.plane_id.unwrap_or(0)
                ));
            }
            crate::drm_scanout::DrmScanoutCommit::Legacy => {
                state
                    .events
                    .push(format!("clear_crtc {}", self.record.crtc_id));
            }
        }
        state.events.push(format!(
            "destroy_framebuffer {}",
            self.record.framebuffer_id
        ));
        state
            .events
            .push(format!("destroy_buffer {}", self.record.buffer_handle));
        state
            .events
            .push(format!("revoke {}", self.record.lessee_id));
        state.active = false;
        drop(state);
        self.active = false;
        self.status.mark_released();
        Ok(())
    }

    fn is_active(&self) -> bool {
        self.active
    }
}

impl Drop for FakeDrmScanoutHandle {
    fn drop(&mut self) {
        let _ = crate::drm_scanout::DrmScanoutHandle::teardown(self);
    }
}

fn parse_mode_size(mode: &str) -> Option<(u32, u32)> {
    let (width, height) = mode.split_once('x')?;
    Some((width.parse().ok()?, height.parse().ok()?))
}
