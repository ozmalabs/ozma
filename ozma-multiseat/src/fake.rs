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
    default_uinput_name, HelperBackend, HelperBackendKind, HelperDevice, HelperError,
    HelperErrorCode, HelperResponse, HelperSeatStatus, InputIsolationAssignment,
    InputIsolationRelease, SeatInputHelper, HELPER_PROTOCOL_VERSION,
};
use crate::routing::{
    CommandOutput, CommandRunner, DeviceNameResolver, EvdevGrabBackend, EvdevGrabRequest,
    RouteCommand,
};
use crate::seat::{SeatPlan, SeatRuntimeStatus, SeatState};
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
            default_uinput_name(seat_id, &device.path)
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
            .map(|device| default_uinput_name(seat_id, &device.path))
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
