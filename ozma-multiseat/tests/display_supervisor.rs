// SPDX-License-Identifier: AGPL-3.0-only
//!
//! Per-seat display backend behavior in `ProcessSeatSupervisor`.
//!
//! Every test uses fake display spawners, fake node launchers, fake readiness
//! probes, fake command runners, and fake helpers. No Xvfb, Xephyr, node,
//! evdev node, or real display is touched.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ozma_multiseat::{
    ChildHandle, DisplayExit, DisplayNumberAllocator, DisplayOutput, FakeCommandRunner,
    FakeDeviceNameResolver, FakeDisplayBackend, FakeInputBackend, FakeNodeLauncher,
    FakeReadinessProbe, FakeSeatDisplayLauncher, FakeSeatInputHelper, HostTopology, InputDevice,
    InputDeviceKind, MultiseatError, MultiseatPlan, MultiseatPlanner, NodeLaunchSpec, NodeLauncher,
    PlannerOptions, ProcessSeatDisplayLauncher, ProcessSeatSupervisor, SeatDisplayBackend,
    SeatDisplayHandle, SeatDisplayLauncher, SeatDisplayNoteKind, SeatDisplayOptions,
    SeatDisplayPrograms, SeatDisplayRequest, SeatDisplaySize, SeatDisplayStatus,
    SeatResourceStatus, SeatRuntimeStatus, SeatState, SeatSupervisor, SupervisorOptions,
    XInputRouter, XrandrOwnershipController,
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
    }
}

fn options(backend: SeatDisplayBackend) -> SupervisorOptions {
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
        input_isolation: false,
        input_targeting: Default::default(),
        seat_display: seat_display_options(backend),
        drm_leases: Default::default(),
    }
}

fn supervisor_with(
    options: SupervisorOptions,
    node: FakeNodeLauncher,
    displays: FakeSeatDisplayLauncher,
) -> ProcessSeatSupervisor {
    ProcessSeatSupervisor::with_parts(
        options,
        Arc::new(node),
        Arc::new(FakeReadinessProbe::always_ready()),
    )
    .with_display_launcher(Box::new(displays))
}

fn status_for(supervisor: &ProcessSeatSupervisor, seat_id: &str) -> SeatRuntimeStatus {
    supervisor
        .status()
        .into_iter()
        .find(|status| status.seat_id == seat_id)
        .expect("seat status")
}

#[test]
fn default_shared_backend_is_a_no_op_and_status_omits_private_fields() {
    let plan = plan();
    let node = FakeNodeLauncher::new();
    let displays = FakeSeatDisplayLauncher::new();
    let mut supervisor = supervisor_with(
        options(SeatDisplayBackend::Shared),
        node.clone(),
        displays.clone(),
    );

    let status = supervisor.start(&plan.seats[0]).expect("start");
    assert_eq!(status.state, SeatState::Running);
    assert_eq!(status.display, SeatDisplayStatus::shared());
    assert_eq!(displays.spawn_count(), 0);
    assert_eq!(displays.stops(), Vec::<String>::new());
    assert_eq!(
        node.spawns()[0].env.get("DISPLAY").map(String::as_str),
        Some(":0")
    );

    let value = serde_json::to_value(&status).expect("status json");
    assert!(value.get("display_backend").is_none());
    assert!(value.get("display").is_none());
    assert!(value.get("display_pid").is_none());
    assert!(value.get("display_ready").is_none());

    let stopped = supervisor.stop("host-seat-0").expect("stop");
    assert_eq!(stopped.state, SeatState::Stopped);
    assert_eq!(displays.stops(), Vec::<String>::new());
}

#[test]
fn xvfb_starts_before_node_exports_display_and_stops_after_node() {
    let plan = plan();
    let log = EventLog::default();
    let node = FakeNodeLauncher::new();
    let displays = FakeSeatDisplayLauncher::new();
    let mut supervisor = ProcessSeatSupervisor::with_parts(
        options(SeatDisplayBackend::Xvfb),
        Arc::new(LoggingNodeLauncher {
            inner: node.clone(),
            log: log.clone(),
        }),
        Arc::new(FakeReadinessProbe::always_ready()),
    )
    .with_display_launcher(Box::new(LoggingDisplayLauncher {
        inner: displays.clone(),
        log: log.clone(),
    }));

    let status = supervisor.start(&plan.seats[0]).expect("start");
    assert_eq!(status.state, SeatState::Running);
    assert_eq!(status.display.display_backend, SeatDisplayBackend::Xvfb);
    assert_eq!(status.display.display.as_deref(), Some(":90"));
    assert_eq!(status.display.display_pid, Some(1));
    assert!(status.display.display_ready);
    assert!(status.display.display_error.is_none());
    assert_eq!(displays.spawn_count(), 1);

    let spawn = &node.spawns()[0];
    assert_eq!(spawn.env.get("DISPLAY").map(String::as_str), Some(":90"));
    assert!(!spawn.env.contains_key("XAUTHORITY"));
    assert!(spawn
        .args
        .windows(2)
        .any(|pair| pair == ["--x11-display", ":90"]));
    assert!(spawn
        .args
        .windows(2)
        .any(|pair| pair == ["--x11-offset", "0,0"]));
    assert!(spawn
        .args
        .windows(2)
        .any(|pair| pair == ["--x11-width", "1280"]));
    assert!(spawn
        .args
        .windows(2)
        .any(|pair| pair == ["--x11-height", "800"]));

    let value = serde_json::to_value(&status).expect("status json");
    assert_eq!(value["display_backend"], "xvfb");
    assert_eq!(value["display"], ":90");
    assert_eq!(value["display_pid"], 1);
    assert_eq!(value["display_ready"], true);

    let stopped = supervisor.stop("host-seat-0").expect("stop");
    assert_eq!(stopped.state, SeatState::Stopped);
    assert_eq!(stopped.display.display_backend, SeatDisplayBackend::Xvfb);
    assert_eq!(stopped.display.display, None);
    assert_eq!(stopped.display.display_pid, None);
    assert!(!stopped.display.display_ready);
    assert_eq!(displays.stops(), vec![":90".to_owned()]);
    assert_eq!(displays.released_numbers(), vec![90]);

    assert_eq!(
        log.snapshot(),
        vec![
            "display start :90".to_owned(),
            "node spawn host-seat-0".to_owned(),
            "display stop :90".to_owned(),
        ]
    );
}

#[test]
fn display_readiness_can_take_more_than_one_probe() {
    let plan = plan();
    let node = FakeNodeLauncher::new();
    let displays = FakeSeatDisplayLauncher::new();
    displays.ready_after("host-seat-0", 2);
    let mut supervisor = supervisor_with(
        options(SeatDisplayBackend::Xvfb),
        node.clone(),
        displays.clone(),
    );

    let status = supervisor.start(&plan.seats[0]).expect("start");
    assert_eq!(status.state, SeatState::Running);
    assert!(status.display.display_ready);
    assert_eq!(node.spawn_count(), 1);
}

#[test]
fn startup_timeout_is_typed_and_never_claims_a_display_or_spawns_the_node() {
    let plan = plan();
    let node = FakeNodeLauncher::new();
    let displays = FakeSeatDisplayLauncher::new();
    displays.never_ready("host-seat-0");
    let mut supervisor = supervisor_with(
        options(SeatDisplayBackend::Xvfb),
        node.clone(),
        displays.clone(),
    );

    let error = supervisor.start(&plan.seats[0]).expect_err("timeout");
    assert!(matches!(
        error,
        MultiseatError::SeatDisplayStartupTimeout { .. }
    ));
    assert_eq!(node.spawn_count(), 0);
    assert_eq!(displays.stops(), vec![":90".to_owned()]);
    assert_eq!(displays.released_numbers(), vec![90]);

    let status = status_for(&supervisor, "host-seat-0");
    assert_eq!(status.state, SeatState::Failed);
    assert_eq!(status.display.display, None);
    assert!(!status.display.display_ready);
    assert_eq!(
        status
            .display
            .display_error
            .as_ref()
            .map(|error| error.kind.as_str()),
        Some("startup_timeout")
    );
}

#[test]
fn crash_before_readiness_is_typed_and_releases_the_number() {
    let plan = plan();
    let node = FakeNodeLauncher::new();
    let displays = FakeSeatDisplayLauncher::new();
    displays.crash_on_start("host-seat-0");
    let mut supervisor = supervisor_with(
        options(SeatDisplayBackend::Xvfb),
        node.clone(),
        displays.clone(),
    );

    let error = supervisor.start(&plan.seats[0]).expect_err("crash");
    assert!(matches!(error, MultiseatError::SeatDisplayCrashed { .. }));
    assert_eq!(node.spawn_count(), 0);
    assert_eq!(displays.released_numbers(), vec![90]);
    let status = status_for(&supervisor, "host-seat-0");
    assert_eq!(
        status
            .display
            .display_error
            .as_ref()
            .map(|error| error.kind.as_str()),
        Some("crashed")
    );
}

#[test]
fn running_display_crash_stops_the_node_and_restarts_both() {
    let plan = plan();
    let node = FakeNodeLauncher::new();
    let displays = FakeSeatDisplayLauncher::new();
    let mut supervisor = supervisor_with(
        options(SeatDisplayBackend::Xvfb),
        node.clone(),
        displays.clone(),
    );

    supervisor.start(&plan.seats[0]).expect("start");
    assert_eq!(node.spawn_count(), 1);
    assert_eq!(displays.display_for("host-seat-0").as_deref(), Some(":90"));

    displays.crash("host-seat-0");
    let now = Instant::now();
    supervisor.tick_at(now + Duration::from_secs(1));
    let crashed = status_for(&supervisor, "host-seat-0");
    assert_eq!(crashed.state, SeatState::Failed);
    assert!(!crashed.display.display_ready);
    assert_eq!(crashed.display.display, None);
    assert_eq!(displays.released_numbers(), vec![90]);
    assert!(!node.active("host-seat-0"));

    supervisor.tick_at(now + Duration::from_secs(2));
    let restarted = status_for(&supervisor, "host-seat-0");
    assert_eq!(restarted.state, SeatState::Running);
    assert_eq!(restarted.display.display.as_deref(), Some(":90"));
    assert!(restarted.display.display_ready);
    assert_eq!(node.spawn_count(), 2);
    assert_eq!(
        node.spawns()[1].env.get("DISPLAY").map(String::as_str),
        Some(":90")
    );
}

#[test]
fn missing_xvfb_binary_is_a_typed_status_error_without_a_node_spawn() {
    let plan = plan();
    let node = FakeNodeLauncher::new();
    let supervisor_options = options(SeatDisplayBackend::Xvfb);
    let display_options = supervisor_options.seat_display.clone();
    let launcher =
        ProcessSeatDisplayLauncher::new(display_options).with_programs(SeatDisplayPrograms {
            xvfb: PathBuf::from("/nonexistent/ozma-Xvfb"),
            ..SeatDisplayPrograms::default()
        });
    let mut supervisor = ProcessSeatSupervisor::with_parts(
        supervisor_options.clone(),
        Arc::new(node.clone()),
        Arc::new(FakeReadinessProbe::always_ready()),
    )
    .with_display_launcher(Box::new(launcher));

    let error = supervisor.start(&plan.seats[0]).expect_err("missing Xvfb");
    assert!(matches!(error, MultiseatError::SeatDisplayTool { .. }));
    assert_eq!(node.spawn_count(), 0);
    let status = status_for(&supervisor, "host-seat-0");
    assert_eq!(
        status
            .display
            .display_error
            .as_ref()
            .map(|error| error.kind.as_str()),
        Some("missing_binary")
    );
    assert_eq!(status.display.display, None);

    // The supervisor refused before spawning anything.
    assert!(supervisor.status().iter().all(|seat| seat.pid.is_none()));
}

#[test]
fn missing_xephyr_is_typed_unsupported() {
    let plan = plan();
    let node = FakeNodeLauncher::new();
    let supervisor_options = options(SeatDisplayBackend::Xephyr);
    let launcher = ProcessSeatDisplayLauncher::new(supervisor_options.seat_display.clone())
        .with_programs(SeatDisplayPrograms {
            xephyr: PathBuf::from("/nonexistent/ozma-Xephyr"),
            ..SeatDisplayPrograms::default()
        });
    let mut supervisor = ProcessSeatSupervisor::with_parts(
        supervisor_options,
        Arc::new(node.clone()),
        Arc::new(FakeReadinessProbe::always_ready()),
    )
    .with_display_launcher(Box::new(launcher));

    let error = supervisor
        .start(&plan.seats[0])
        .expect_err("missing Xephyr");
    assert!(matches!(error, MultiseatError::Unsupported(_)));
    assert_eq!(node.spawn_count(), 0);
    let status = status_for(&supervisor, "host-seat-0");
    assert_eq!(
        status
            .display
            .display_error
            .as_ref()
            .map(|error| error.kind.as_str()),
        Some("unsupported")
    );
}

#[test]
fn nested_ownership_and_xinput_requests_become_typed_notes_not_actions() {
    let plan = plan();
    let node = FakeNodeLauncher::new();
    let displays = FakeSeatDisplayLauncher::new();
    let runner = FakeCommandRunner::new();
    let mut supervisor_options = options(SeatDisplayBackend::Xvfb);
    supervisor_options.apply_ownership = true;
    supervisor_options.apply_routing = true;
    let mut supervisor = ProcessSeatSupervisor::with_parts(
        supervisor_options,
        Arc::new(node.clone()),
        Arc::new(FakeReadinessProbe::always_ready()),
    )
    .with_display_launcher(Box::new(displays))
    .with_ownership_controller(Box::new(XrandrOwnershipController::new(Box::new(
        runner.clone(),
    ))))
    .with_routing_controller(Box::new(XInputRouter::with_resolver(
        Box::new(runner.clone()),
        Box::new(FakeDeviceNameResolver::new()),
    )));

    let status = supervisor.start(&plan.seats[0]).expect("start");
    assert_eq!(status.state, SeatState::Running);
    assert!(!status.resources.ownership_applied);
    assert!(!status.resources.routing_applied);
    assert!(runner.commands().is_empty());
    assert!(status
        .display
        .display_notes
        .iter()
        .any(|note| note.kind == SeatDisplayNoteKind::OwnershipUnsupported));
    assert!(status
        .display
        .display_notes
        .iter()
        .any(|note| note.kind == SeatDisplayNoteKind::XinputRoutingUnsupported));
}

#[test]
fn helper_input_isolation_still_applies_with_a_private_display() {
    let plan = plan();
    let node = FakeNodeLauncher::new();
    let displays = FakeSeatDisplayLauncher::new();
    let helper = FakeSeatInputHelper::new();
    let mut supervisor_options = options(SeatDisplayBackend::Xvfb);
    supervisor_options.input_isolation = true;
    let mut supervisor = ProcessSeatSupervisor::with_parts(
        supervisor_options,
        Arc::new(node.clone()),
        Arc::new(FakeReadinessProbe::always_ready()),
    )
    .with_display_launcher(Box::new(displays))
    .with_input_isolation_helper(Box::new(helper.clone()));

    let status = supervisor.start(&plan.seats[0]).expect("start");
    assert_eq!(status.state, SeatState::Running);
    assert!(status.resources.input_isolation_applied);
    assert_eq!(status.resources.grabbed_device_count, 2);
    assert_eq!(helper.grabbed("host-seat-0").len(), 2);
    assert_eq!(
        node.spawns()[0].env.get("DISPLAY").map(String::as_str),
        Some(":90")
    );

    let stopped = supervisor.stop("host-seat-0").expect("stop");
    assert!(!stopped.resources.input_isolation_applied);
    assert!(stopped.resources.input_isolation_released);
    assert!(!helper.has_assignment("host-seat-0"));
}

#[test]
fn resource_rollback_releases_a_display_that_already_started() {
    let plan = plan();
    let node = FakeNodeLauncher::new();
    let displays = FakeSeatDisplayLauncher::new();
    let mut supervisor_options = options(SeatDisplayBackend::Xvfb);
    supervisor_options.input_isolation = true;
    // No helper installed: the display starts, then isolation fails. The
    // failed seat must not keep the display or spawn a node.
    let mut supervisor = supervisor_with(supervisor_options, node.clone(), displays.clone());

    let error = supervisor.start(&plan.seats[0]).expect_err("no helper");
    assert!(matches!(error, MultiseatError::Unsupported(_)));
    assert_eq!(node.spawn_count(), 0);
    assert_eq!(displays.spawn_count(), 1);
    assert_eq!(displays.stops(), vec![":90".to_owned()]);
    assert_eq!(displays.released_numbers(), vec![90]);
    let status = status_for(&supervisor, "host-seat-0");
    assert_eq!(status.state, SeatState::Failed);
    assert_eq!(status.display.display, None);
    assert!(!status.display.display_ready);
}

#[test]
fn cleanup_order_is_display_first_on_start_and_last_on_stop() {
    let plan = plan();
    let log = EventLog::default();
    let node = FakeNodeLauncher::new();
    let displays = FakeSeatDisplayLauncher::new();
    let mut supervisor = ProcessSeatSupervisor::with_parts(
        options(SeatDisplayBackend::Xvfb),
        Arc::new(LoggingNodeLauncher {
            inner: node,
            log: log.clone(),
        }),
        Arc::new(FakeReadinessProbe::always_ready()),
    )
    .with_display_launcher(Box::new(LoggingDisplayLauncher {
        inner: displays,
        log: log.clone(),
    }));

    supervisor.start(&plan.seats[0]).expect("start");
    supervisor.stop("host-seat-0").expect("stop");
    assert_eq!(
        log.snapshot(),
        vec![
            "display start :90".to_owned(),
            "node spawn host-seat-0".to_owned(),
            "display stop :90".to_owned(),
        ]
    );
}

#[test]
fn display_number_allocator_reuses_released_numbers_and_reports_exhaustion() {
    let mut allocator = DisplayNumberAllocator::without_host_check(90, 92);
    assert_eq!(allocator.allocate().expect("first"), 90);
    assert_eq!(allocator.allocate().expect("second"), 91);
    assert_eq!(allocator.in_use(), vec![90, 91]);
    allocator.release(90);
    assert_eq!(allocator.allocate().expect("reused"), 90);
    assert_eq!(allocator.allocate().expect("third"), 92);
    assert!(matches!(
        allocator.allocate(),
        Err(MultiseatError::SeatDisplayNumberExhausted { start: 90, end: 92 })
    ));

    let mut busy = DisplayNumberAllocator::without_host_check(90, 91).with_busy([90]);
    assert_eq!(busy.allocate().expect("skips busy"), 91);
}

#[test]
fn dry_run_reports_the_display_plan_without_spawning_it() {
    let plan = plan();
    let node = FakeNodeLauncher::new();
    let displays = FakeSeatDisplayLauncher::new();
    let mut supervisor_options = options(SeatDisplayBackend::Xvfb);
    supervisor_options.dry_run = true;
    let mut supervisor = supervisor_with(supervisor_options, node.clone(), displays.clone());

    let status = supervisor.start(&plan.seats[0]).expect("dry run");
    assert_eq!(status.state, SeatState::Planned);
    assert_eq!(displays.spawn_count(), 0);
    assert_eq!(node.spawn_count(), 0);
    assert!(status
        .message
        .as_deref()
        .is_some_and(|message| message.contains("would start xvfb display 1280x800")));
}

#[test]
fn empty_resource_status_still_round_trips_with_display_fields() {
    let mut status = SeatRuntimeStatus::new("host-seat-0", SeatState::Running);
    status.resources = SeatResourceStatus::default();
    status.display = SeatDisplayStatus {
        display_backend: SeatDisplayBackend::Xvfb,
        display: Some(":90".to_owned()),
        display_pid: Some(42),
        display_ready: true,
        display_notes: vec![],
        display_error: None,
    };
    let value = serde_json::to_value(&status).expect("json");
    assert_eq!(value["display_backend"], "xvfb");
    assert_eq!(value["display"], ":90");
    assert_eq!(value["display_pid"], 42);
    assert_eq!(value["display_ready"], true);
    let decoded: SeatRuntimeStatus = serde_json::from_value(value).expect("roundtrip");
    assert_eq!(decoded, status);
}
