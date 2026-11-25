// SPDX-License-Identifier: AGPL-3.0-only
//!
//! Supervisor lifecycle for private-display input targeting. Every test uses
//! fake displays, helper, targeter, node launcher, and readiness probe; no X
//! server, `xinput`, evdev node, or uinput device is touched.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ozma_multiseat::{
    ChildHandle, DisplayExit, DisplayOutput, FakeDisplayBackend, FakeInputBackend,
    FakeNodeLauncher, FakeReadinessProbe, FakeSeatDisplayLauncher, FakeSeatInputHelper,
    FakeSeatInputTargeter, HostTopology, InputDevice, InputDeviceKind, InputTargetingMode,
    MultiseatError, MultiseatPlan, MultiseatPlanner, NodeLaunchSpec, NodeLauncher, PlannerOptions,
    ProcessSeatSupervisor, ResourceAction, ResourceKind, SeatDisplayBackend, SeatDisplayHandle,
    SeatDisplayLauncher, SeatDisplayNoteKind, SeatDisplayOptions, SeatDisplayRequest,
    SeatDisplaySize, SeatInputTargetRequest, SeatInputTargeter, SeatInputTargeting,
    SeatInputTargetingRelease, SeatState, SeatSupervisor, SupervisorOptions,
};

const SEAT_DISPLAY_SIZE: SeatDisplaySize = SeatDisplaySize {
    width: 1280,
    height: 800,
};

#[derive(Clone, Debug, Default)]
struct EventLog {
    events: Arc<Mutex<Vec<String>>>,
}

impl EventLog {
    fn push(&self, event: impl Into<String>) {
        self.events
            .lock()
            .expect("event log lock")
            .push(event.into());
    }

    fn snapshot(&self) -> Vec<String> {
        self.events.lock().expect("event log lock").clone()
    }
}

#[derive(Clone)]
struct LoggingNodeLauncher {
    inner: FakeNodeLauncher,
    log: EventLog,
}

impl NodeLauncher for LoggingNodeLauncher {
    fn spawn(&self, spec: &NodeLaunchSpec) -> Result<Box<dyn ChildHandle>, MultiseatError> {
        self.log.push(format!("node spawn {}", spec.seat_id));
        self.inner.spawn(spec)
    }
}

#[derive(Clone)]
struct LoggingDisplayLauncher {
    inner: FakeSeatDisplayLauncher,
    log: EventLog,
}

impl SeatDisplayLauncher for LoggingDisplayLauncher {
    fn spawn(
        &self,
        request: &SeatDisplayRequest,
    ) -> Result<Box<dyn SeatDisplayHandle>, MultiseatError> {
        let handle = self.inner.spawn(request)?;
        self.log.push(format!("display start {}", handle.display()));
        Ok(Box::new(LoggingDisplayHandle {
            inner: handle,
            log: self.log.clone(),
        }))
    }
}

struct LoggingDisplayHandle {
    inner: Box<dyn SeatDisplayHandle>,
    log: EventLog,
}

impl SeatDisplayHandle for LoggingDisplayHandle {
    fn backend(&self) -> SeatDisplayBackend {
        self.inner.backend()
    }

    fn display(&self) -> &str {
        self.inner.display()
    }

    fn display_number(&self) -> u32 {
        self.inner.display_number()
    }

    fn pid(&self) -> Option<u32> {
        self.inner.pid()
    }

    fn xauthority(&self) -> Option<&Path> {
        self.inner.xauthority()
    }

    fn auth_disabled(&self) -> bool {
        self.inner.auth_disabled()
    }

    fn is_ready(&self) -> Result<bool, MultiseatError> {
        self.inner.is_ready()
    }

    fn try_wait(&mut self) -> std::io::Result<Option<DisplayExit>> {
        self.inner.try_wait()
    }

    fn terminate(&mut self) -> Result<(), MultiseatError> {
        self.log
            .push(format!("display stop {}", self.inner.display()));
        self.inner.terminate()
    }
}

#[derive(Clone)]
struct LoggingTargeter {
    inner: FakeSeatInputTargeter,
    log: EventLog,
}

impl SeatInputTargeter for LoggingTargeter {
    fn apply(
        &mut self,
        request: &SeatInputTargetRequest,
    ) -> Result<SeatInputTargeting, MultiseatError> {
        self.log
            .push(format!("target {} {}", request.seat_id, request.display));
        self.inner.apply(request)
    }

    fn release(&mut self, seat_id: &str) -> Result<SeatInputTargetingRelease, MultiseatError> {
        self.log.push(format!("target release {seat_id}"));
        self.inner.release(seat_id)
    }

    fn status_for(&self, seat_id: &str) -> Option<SeatInputTargeting> {
        self.inner.status_for(seat_id)
    }

    fn clear(&mut self, seat_id: &str) {
        self.inner.clear(seat_id);
    }
}

fn display(index: u32, name: &str, x: i32) -> DisplayOutput {
    DisplayOutput::new(index, name, 1920, 1080, x, 0, ":0")
}

fn device(path: &str, kind: InputDeviceKind, hub: &str) -> InputDevice {
    InputDevice {
        path: path.to_owned(),
        name: path.to_owned(),
        kind,
        usb_hub: Some(hub.to_owned()),
        sysfs_path: None,
    }
}

fn plan() -> MultiseatPlan {
    let inputs = vec![
        device("/dev/input/event0", InputDeviceKind::Keyboard, "1-1"),
        device("/dev/input/event1", InputDeviceKind::Mouse, "1-1"),
        device("/dev/input/event2", InputDeviceKind::Keyboard, "2-1"),
        device("/dev/input/event3", InputDeviceKind::Mouse, "2-1"),
    ];
    let topology = HostTopology::enumerate(
        &FakeDisplayBackend::new(vec![display(0, "HDMI-1", 0), display(1, "DP-2", 1920)]),
        &FakeInputBackend::new(inputs),
    )
    .expect("topology");
    topology
        .plan(&MultiseatPlanner::new(PlannerOptions {
            machine_name: "host".to_owned(),
            x11_display: Some(":0".to_owned()),
            ..PlannerOptions::default()
        }))
        .expect("plan")
}

fn seat_display_options(backend: SeatDisplayBackend) -> SeatDisplayOptions {
    SeatDisplayOptions {
        backend,
        size: SEAT_DISPLAY_SIZE,
        number_start: 90,
        number_end: 99,
        ready_timeout: Duration::from_millis(40),
        ready_poll: Duration::from_millis(1),
        parent_display: None,
        xorg_config: None,
    }
}

fn options(backend: SeatDisplayBackend, mode: InputTargetingMode) -> SupervisorOptions {
    SupervisorOptions {
        node_bin: None,
        readiness_timeout: Duration::from_millis(100),
        readiness_poll: Duration::from_millis(1),
        restart_backoff: Duration::from_millis(10),
        restart_backoff_max: Duration::from_millis(40),
        healthy_uptime: Duration::from_secs(30),
        dry_run: false,
        apply_ownership: false,
        apply_routing: false,
        input_isolation: true,
        input_targeting: mode,
        seat_display: seat_display_options(backend),
        drm_leases: Default::default(),
    }
}

fn own_devices() -> Vec<String> {
    vec![
        "ozma-seat-host-seat-0-kbd-event0".to_owned(),
        "ozma-seat-host-seat-0-mouse-event1".to_owned(),
    ]
}

fn available_devices() -> Vec<String> {
    vec![
        "Virtual core pointer".to_owned(),
        "Virtual core keyboard".to_owned(),
        "ozma-seat-host-seat-0-kbd-event0".to_owned(),
        "ozma-seat-host-seat-0-mouse-event1".to_owned(),
        "ozma-seat-host-seat-1-kbd-event2".to_owned(),
        "ozma-seat-host-seat-1-mouse-event3".to_owned(),
    ]
}

fn targeter() -> FakeSeatInputTargeter {
    FakeSeatInputTargeter::new().with_available_devices(available_devices())
}

fn supervisor(
    options: SupervisorOptions,
    helper: FakeSeatInputHelper,
    targeter: FakeSeatInputTargeter,
    displays: FakeSeatDisplayLauncher,
    node: FakeNodeLauncher,
) -> ProcessSeatSupervisor {
    ProcessSeatSupervisor::with_parts(
        options,
        Arc::new(node),
        Arc::new(FakeReadinessProbe::always_ready()),
    )
    .with_display_launcher(Box::new(displays))
    .with_input_isolation_helper(Box::new(helper))
    .with_input_targeter(Box::new(targeter))
}

fn status_for(
    supervisor: &ProcessSeatSupervisor,
    seat_id: &str,
) -> ozma_multiseat::SeatRuntimeStatus {
    supervisor
        .status()
        .into_iter()
        .find(|status| status.seat_id == seat_id)
        .expect("seat status")
}

fn targeting_error(status: &ozma_multiseat::SeatRuntimeStatus, action: ResourceAction) -> bool {
    status
        .resources
        .errors
        .iter()
        .any(|error| error.kind == ResourceKind::InputTargeting && error.action == action)
}

#[test]
fn apply_order_is_display_then_target_then_node_and_only_own_devices_stay_enabled() {
    let plan = plan();
    let log = EventLog::default();
    let helper = FakeSeatInputHelper::new();
    let displays = FakeSeatDisplayLauncher::new();
    let node = FakeNodeLauncher::new();
    let targeter = targeter();
    let mut supervisor = ProcessSeatSupervisor::with_parts(
        options(SeatDisplayBackend::Xvfb, InputTargetingMode::Warn),
        Arc::new(LoggingNodeLauncher {
            inner: node.clone(),
            log: log.clone(),
        }),
        Arc::new(FakeReadinessProbe::always_ready()),
    )
    .with_display_launcher(Box::new(LoggingDisplayLauncher {
        inner: displays.clone(),
        log: log.clone(),
    }))
    .with_input_isolation_helper(Box::new(helper.clone()))
    .with_input_targeter(Box::new(LoggingTargeter {
        inner: targeter.clone(),
        log: log.clone(),
    }));

    let status = supervisor.start(&plan.seats[0]).expect("start");
    assert_eq!(status.state, SeatState::Running);
    assert!(status.resources.input_isolation_applied);
    assert!(status.resources.input_targeting_applied);
    assert_eq!(status.resources.uinput_devices, own_devices());
    assert_eq!(status.resources.targeted_devices, own_devices());
    assert_eq!(
        status.resources.disabled_devices,
        vec![
            "ozma-seat-host-seat-1-kbd-event2".to_owned(),
            "ozma-seat-host-seat-1-mouse-event3".to_owned(),
        ]
    );
    assert_eq!(
        log.snapshot(),
        vec![
            "display start :90".to_owned(),
            "target host-seat-0 :90".to_owned(),
            "node spawn host-seat-0".to_owned(),
        ]
    );

    let requests = targeter.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].display, ":90");
    assert_eq!(requests[0].devices, own_devices());

    let value = serde_json::to_value(&status).expect("status json");
    assert_eq!(value["resources"]["input_targeting_applied"], true);
    assert_eq!(
        value["resources"]["targeted_devices"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        value["resources"]["disabled_devices"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn strict_targeting_failure_prevents_spawn_and_rolls_back_helper_and_display() {
    let plan = plan();
    let helper = FakeSeatInputHelper::new();
    let displays = FakeSeatDisplayLauncher::new();
    let node = FakeNodeLauncher::new();
    let targeter = FakeSeatInputTargeter::new().fail_seat("host-seat-0");
    let mut supervisor = supervisor(
        options(SeatDisplayBackend::Xvfb, InputTargetingMode::Strict),
        helper.clone(),
        targeter.clone(),
        displays.clone(),
        node.clone(),
    );

    let error = supervisor
        .start(&plan.seats[0])
        .expect_err("strict targeting");
    assert!(matches!(error, MultiseatError::InputTargeting(_)));
    assert_eq!(node.spawn_count(), 0);
    assert_eq!(displays.stops(), vec![":90".to_owned()]);
    assert_eq!(displays.released_numbers(), vec![90]);
    assert!(!helper.has_assignment("host-seat-0"));

    let status = status_for(&supervisor, "host-seat-0");
    assert_eq!(status.state, SeatState::Failed);
    assert!(!status.resources.input_isolation_applied);
    assert!(!status.resources.input_targeting_applied);
    assert!(status.resources.targeted_devices.is_empty());
    assert!(targeting_error(&status, ResourceAction::Apply));
    assert!(status
        .display
        .display_notes
        .iter()
        .any(|note| note.kind == SeatDisplayNoteKind::InputTargetingFailed));
}

#[test]
fn warn_targeting_failure_still_spawns_but_never_claims_targeting() {
    let plan = plan();
    let helper = FakeSeatInputHelper::new();
    let displays = FakeSeatDisplayLauncher::new();
    let node = FakeNodeLauncher::new();
    let targeter = FakeSeatInputTargeter::new().fail_seat("host-seat-0");
    let mut supervisor = supervisor(
        options(SeatDisplayBackend::Xvfb, InputTargetingMode::Warn),
        helper.clone(),
        targeter.clone(),
        displays.clone(),
        node.clone(),
    );

    let status = supervisor.start(&plan.seats[0]).expect("warn and continue");
    assert_eq!(status.state, SeatState::Running);
    assert_eq!(node.spawn_count(), 1);
    assert!(helper.has_assignment("host-seat-0"));
    assert!(status.resources.input_isolation_applied);
    assert!(!status.resources.input_targeting_applied);
    assert!(status.resources.targeted_devices.is_empty());
    assert!(status.resources.disabled_devices.is_empty());
    assert!(targeting_error(&status, ResourceAction::Apply));
    assert!(status
        .display
        .display_notes
        .iter()
        .any(|note| note.kind == SeatDisplayNoteKind::InputTargetingFailed));
}

#[test]
fn shared_display_is_a_targeting_no_op() {
    let plan = plan();
    let helper = FakeSeatInputHelper::new();
    let displays = FakeSeatDisplayLauncher::new();
    let node = FakeNodeLauncher::new();
    let targeter = targeter();
    let mut supervisor = supervisor(
        options(SeatDisplayBackend::Shared, InputTargetingMode::Warn),
        helper.clone(),
        targeter.clone(),
        displays.clone(),
        node.clone(),
    );

    let status = supervisor.start(&plan.seats[0]).expect("start");
    assert_eq!(status.state, SeatState::Running);
    assert!(status.resources.input_isolation_applied);
    assert!(!status.resources.input_targeting_applied);
    assert_eq!(targeter.apply_count(), 0);
    assert!(targeter.requests().is_empty());
    assert_eq!(displays.spawn_count(), 0);
    assert!(!status.display.display_notes.iter().any(|note| matches!(
        note.kind,
        SeatDisplayNoteKind::InputTargetingFailed | SeatDisplayNoteKind::InputTargetingUnavailable
    )));
}

#[test]
fn stop_releases_targeting_before_display_and_clears_status() {
    let plan = plan();
    let helper = FakeSeatInputHelper::new();
    let displays = FakeSeatDisplayLauncher::new();
    let node = FakeNodeLauncher::new();
    let targeter = targeter();
    let mut supervisor = supervisor(
        options(SeatDisplayBackend::Xvfb, InputTargetingMode::Warn),
        helper.clone(),
        targeter.clone(),
        displays.clone(),
        node,
    );

    supervisor.start(&plan.seats[0]).expect("start");
    let stopped = supervisor.stop("host-seat-0").expect("stop");
    assert_eq!(stopped.state, SeatState::Stopped);
    assert!(!stopped.resources.input_isolation_applied);
    assert!(!stopped.resources.input_targeting_applied);
    assert!(stopped.resources.input_targeting_released);
    assert!(stopped.resources.targeted_devices.is_empty());
    assert!(stopped.resources.disabled_devices.is_empty());
    assert_eq!(targeter.releases(), vec!["host-seat-0".to_owned()]);
    assert!(!targeter.has_targeting("host-seat-0"));
    assert!(!helper.has_assignment("host-seat-0"));
    assert_eq!(displays.stops(), vec![":90".to_owned()]);
}

#[test]
fn helper_restart_reapplies_targeting_without_respawning_the_node() {
    let plan = plan();
    let helper = FakeSeatInputHelper::new();
    let displays = FakeSeatDisplayLauncher::new();
    let node = FakeNodeLauncher::new();
    let targeter = targeter();
    let mut supervisor = supervisor(
        options(SeatDisplayBackend::Xvfb, InputTargetingMode::Warn),
        helper.clone(),
        targeter.clone(),
        displays.clone(),
        node.clone(),
    );

    supervisor.start(&plan.seats[0]).expect("start");
    assert_eq!(targeter.apply_count(), 1);

    helper.crash();
    assert!(!helper.has_assignment("host-seat-0"));

    supervisor.tick_at(Instant::now());

    assert_eq!(helper.start_count(), 2);
    assert_eq!(node.spawn_count(), 1, "node must not respawn");
    assert_eq!(targeter.apply_count(), 2, "targeting must be re-applied");
    assert!(targeter.has_targeting("host-seat-0"));
    let status = status_for(&supervisor, "host-seat-0");
    assert!(status.resources.input_isolation_applied);
    assert!(status.resources.input_targeting_applied);
    assert_eq!(status.resources.targeted_devices, own_devices());
}

#[test]
fn display_restart_reapplies_targeting_to_the_new_display() {
    let plan = plan();
    let helper = FakeSeatInputHelper::new();
    let displays = FakeSeatDisplayLauncher::new();
    let node = FakeNodeLauncher::new();
    let targeter = targeter();
    let mut supervisor = supervisor(
        options(SeatDisplayBackend::Xvfb, InputTargetingMode::Warn),
        helper.clone(),
        targeter.clone(),
        displays.clone(),
        node.clone(),
    );

    supervisor.start(&plan.seats[0]).expect("start");
    assert_eq!(targeter.apply_count(), 1);

    displays.crash("host-seat-0");
    let now = Instant::now();
    supervisor.tick_at(now);
    assert!(!targeter.has_targeting("host-seat-0"));
    assert!(
        !status_for(&supervisor, "host-seat-0")
            .resources
            .input_targeting_applied
    );

    supervisor.tick_at(now + Duration::from_secs(1));
    assert_eq!(displays.spawn_count(), 2);
    assert_eq!(node.spawn_count(), 2, "node restarts after its display");
    assert_eq!(targeter.apply_count(), 2);
    assert!(targeter.has_targeting("host-seat-0"));
    let status = status_for(&supervisor, "host-seat-0");
    assert_eq!(status.state, SeatState::Running);
    assert!(status.resources.input_targeting_applied);
}

#[test]
fn later_seat_targeting_failure_releases_earlier_seat_targeting() {
    let plan = plan();
    let helper = FakeSeatInputHelper::new();
    let displays = FakeSeatDisplayLauncher::new();
    let node = FakeNodeLauncher::new();
    let targeter = FakeSeatInputTargeter::new()
        .with_available_devices(available_devices())
        .fail_seat("host-seat-1");
    let mut supervisor = supervisor(
        options(SeatDisplayBackend::Xvfb, InputTargetingMode::Strict),
        helper.clone(),
        targeter.clone(),
        displays.clone(),
        node.clone(),
    );

    supervisor.start(&plan.seats[0]).expect("seat 0");
    assert!(targeter.has_targeting("host-seat-0"));

    let error = supervisor
        .start(&plan.seats[1])
        .expect_err("seat 1 targeting");
    assert!(matches!(error, MultiseatError::InputTargeting(_)));
    assert_eq!(node.spawn_count(), 1, "only seat 0 spawned");

    assert!(!targeter.has_targeting("host-seat-0"));
    assert!(!helper.has_assignment("host-seat-0"));
    assert!(!helper.has_assignment("host-seat-1"));
    let seat0 = status_for(&supervisor, "host-seat-0");
    assert!(!seat0.resources.input_isolation_applied);
    assert!(!seat0.resources.input_targeting_applied);
    assert!(targeter.releases().contains(&"host-seat-0".to_owned()));
}

#[test]
fn targeting_release_failure_is_typed_and_does_not_leave_a_false_claim() {
    let plan = plan();
    let helper = FakeSeatInputHelper::new();
    let displays = FakeSeatDisplayLauncher::new();
    let node = FakeNodeLauncher::new();
    let targeter = targeter().fail_next_release("injected release failure");
    let mut supervisor = supervisor(
        options(SeatDisplayBackend::Xvfb, InputTargetingMode::Warn),
        helper,
        targeter,
        displays,
        node,
    );

    supervisor.start(&plan.seats[0]).expect("start");
    let stopped = supervisor.stop("host-seat-0").expect("stop");
    assert_eq!(stopped.state, SeatState::Stopped);
    assert!(!stopped.resources.input_targeting_applied);
    assert!(targeting_error(&stopped, ResourceAction::Release));
}
