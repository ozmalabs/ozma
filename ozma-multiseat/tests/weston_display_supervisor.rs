// SPDX-License-Identifier: AGPL-3.0-only
//!
//! Headless-Weston seat display behavior in `ProcessSeatSupervisor`.
//!
//! Every test uses fake display spawners, fake node launchers, fake readiness
//! probes, and fake helpers/targeters. No Weston compositor, X server, node, or
//! real display is touched.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ozma_multiseat::{
    ChildHandle, DisplayExit, DisplayOutput, FakeDisplayBackend, FakeInputBackend,
    FakeNodeLauncher, FakeReadinessProbe, FakeSeatDisplayLauncher, FakeSeatInputHelper,
    FakeSeatInputTargeter, HostTopology, InputDevice, InputDeviceKind, MultiseatError,
    MultiseatPlan, MultiseatPlanner, NodeLaunchSpec, NodeLauncher, PlannerOptions,
    ProcessSeatDisplayLauncher, ProcessSeatSupervisor, SeatDisplayBackend, SeatDisplayHandle,
    SeatDisplayLauncher, SeatDisplayNoteKind, SeatDisplayOptions, SeatDisplayPrograms,
    SeatDisplayRequest, SeatDisplaySize, SeatDisplayStatus, SeatRuntimeStatus, SeatState,
    SeatSupervisor, SupervisorOptions,
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

    fn wayland(&self) -> Option<&ozma_multiseat::SeatWaylandEnv> {
        self.inner.wayland()
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
        xorg_config: None,
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
        seat_input: Default::default(),
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
fn default_shared_backend_starts_no_weston_and_omits_private_status() {
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
    assert_eq!(
        node.spawns()[0].env.get("DISPLAY").map(String::as_str),
        Some(":0")
    );
    assert!(!node.spawns()[0].env.contains_key("WAYLAND_DISPLAY"));

    let value = serde_json::to_value(&status).expect("status json");
    assert!(value.get("display_backend").is_none());
    assert!(value.get("display").is_none());

    let stopped = supervisor.stop("host-seat-0").expect("stop");
    assert_eq!(stopped.state, SeatState::Stopped);
    assert_eq!(displays.stops(), Vec::<String>::new());
}

#[test]
fn weston_starts_before_node_exports_wayland_env_and_stops_after_node() {
    let plan = plan();
    let log = EventLog::default();
    let node = FakeNodeLauncher::new();
    let displays = FakeSeatDisplayLauncher::new();
    let mut supervisor = ProcessSeatSupervisor::with_parts(
        options(SeatDisplayBackend::Weston),
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
    assert_eq!(status.display.display_backend, SeatDisplayBackend::Weston);
    assert_eq!(status.display.display.as_deref(), Some("ozma-weston-90"));
    assert_eq!(status.display.display_pid, Some(1));
    assert!(status.display.display_ready);
    assert!(status.display.display_error.is_none());
    assert!(status
        .display
        .display_notes
        .iter()
        .any(|note| note.kind == SeatDisplayNoteKind::InputTargetingUnavailable));
    assert_eq!(displays.spawn_count(), 1);

    let spawn = &node.spawns()[0];
    assert_eq!(
        spawn.env.get("WAYLAND_DISPLAY").map(String::as_str),
        Some("ozma-weston-90")
    );
    let runtime = spawn
        .env
        .get("XDG_RUNTIME_DIR")
        .expect("XDG_RUNTIME_DIR must be exported");
    assert!(
        runtime.contains("ozma-fake-weston"),
        "unexpected runtime dir: {runtime}"
    );
    assert!(!spawn.env.contains_key("DISPLAY"));
    assert!(!spawn.env.contains_key("XAUTHORITY"));

    let value = serde_json::to_value(&status).expect("status json");
    assert_eq!(value["display_backend"], "weston");
    assert_eq!(value["display"], "ozma-weston-90");
    assert_eq!(value["display_pid"], 1);
    assert_eq!(value["display_ready"], true);
    assert!(value["display_notes"]
        .as_array()
        .expect("notes array")
        .iter()
        .any(|note| note["kind"] == "input_targeting_unavailable"));

    let stopped = supervisor.stop("host-seat-0").expect("stop");
    assert_eq!(stopped.state, SeatState::Stopped);
    assert_eq!(stopped.display.display, None);
    assert_eq!(stopped.display.display_pid, None);
    assert!(!stopped.display.display_ready);
    assert_eq!(displays.stops(), vec!["ozma-weston-90".to_owned()]);
    assert_eq!(displays.released_numbers(), vec![90]);

    assert_eq!(
        log.snapshot(),
        vec![
            "display start ozma-weston-90".to_owned(),
            "node spawn host-seat-0".to_owned(),
            "display stop ozma-weston-90".to_owned(),
        ]
    );
}

#[test]
fn weston_readiness_can_take_more_than_one_probe() {
    let plan = plan();
    let node = FakeNodeLauncher::new();
    let displays = FakeSeatDisplayLauncher::new();
    displays.ready_after("host-seat-0", 2);
    let mut supervisor = supervisor_with(
        options(SeatDisplayBackend::Weston),
        node.clone(),
        displays.clone(),
    );

    let status = supervisor.start(&plan.seats[0]).expect("start");
    assert_eq!(status.state, SeatState::Running);
    assert!(status.display.display_ready);
    assert_eq!(node.spawn_count(), 1);
}

#[test]
fn weston_startup_timeout_is_typed_and_never_claims_a_display_or_spawns_the_node() {
    let plan = plan();
    let node = FakeNodeLauncher::new();
    let displays = FakeSeatDisplayLauncher::new();
    displays.never_ready("host-seat-0");
    let mut supervisor = supervisor_with(
        options(SeatDisplayBackend::Weston),
        node.clone(),
        displays.clone(),
    );

    let error = supervisor.start(&plan.seats[0]).expect_err("timeout");
    assert!(matches!(
        error,
        MultiseatError::SeatDisplayStartupTimeout { .. }
    ));
    assert_eq!(node.spawn_count(), 0);
    assert_eq!(displays.stops(), vec!["ozma-weston-90".to_owned()]);
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
fn weston_crash_before_readiness_is_typed_and_releases_the_number() {
    let plan = plan();
    let node = FakeNodeLauncher::new();
    let displays = FakeSeatDisplayLauncher::new();
    displays.crash_on_start("host-seat-0");
    let mut supervisor = supervisor_with(
        options(SeatDisplayBackend::Weston),
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
fn running_weston_crash_stops_the_node_and_restarts_both_with_the_same_socket() {
    let plan = plan();
    let node = FakeNodeLauncher::new();
    let displays = FakeSeatDisplayLauncher::new();
    let mut supervisor = supervisor_with(
        options(SeatDisplayBackend::Weston),
        node.clone(),
        displays.clone(),
    );

    supervisor.start(&plan.seats[0]).expect("start");
    assert_eq!(node.spawn_count(), 1);
    assert_eq!(
        displays.display_for("host-seat-0").as_deref(),
        Some("ozma-weston-90")
    );

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
    assert_eq!(restarted.display.display.as_deref(), Some("ozma-weston-90"));
    assert!(restarted.display.display_ready);
    assert_eq!(node.spawn_count(), 2);
    assert_eq!(
        node.spawns()[1]
            .env
            .get("WAYLAND_DISPLAY")
            .map(String::as_str),
        Some("ozma-weston-90")
    );
}

#[test]
fn missing_weston_binary_is_a_typed_status_error_without_a_node_spawn() {
    let plan = plan();
    let node = FakeNodeLauncher::new();
    let supervisor_options = options(SeatDisplayBackend::Weston);
    let launcher = ProcessSeatDisplayLauncher::new(supervisor_options.seat_display.clone())
        .with_programs(SeatDisplayPrograms {
            weston: PathBuf::from("/nonexistent/ozma-weston"),
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
        .expect_err("missing weston");
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
    assert!(supervisor.status().iter().all(|seat| seat.pid.is_none()));
}

#[test]
fn weston_rollback_releases_a_started_compositor_when_resource_apply_fails() {
    let plan = plan();
    let node = FakeNodeLauncher::new();
    let displays = FakeSeatDisplayLauncher::new();
    let mut supervisor_options = options(SeatDisplayBackend::Weston);
    supervisor_options.input_isolation = true;
    // No helper installed: the compositor starts, then isolation fails. The
    // failed seat must not keep the compositor or spawn a node.
    let mut supervisor = supervisor_with(supervisor_options, node.clone(), displays.clone());

    let error = supervisor.start(&plan.seats[0]).expect_err("no helper");
    assert!(matches!(error, MultiseatError::Unsupported(_)));
    assert_eq!(node.spawn_count(), 0);
    assert_eq!(displays.spawn_count(), 1);
    assert_eq!(displays.stops(), vec!["ozma-weston-90".to_owned()]);
    assert_eq!(displays.released_numbers(), vec![90]);
    let status = status_for(&supervisor, "host-seat-0");
    assert_eq!(status.state, SeatState::Failed);
    assert_eq!(status.display.display, None);
    assert!(!status.display.display_ready);
}

#[test]
fn weston_input_targeting_is_unavailable_and_never_claimed() {
    let plan = plan();
    let node = FakeNodeLauncher::new();
    let displays = FakeSeatDisplayLauncher::new();
    let helper = FakeSeatInputHelper::new();
    let targeter = FakeSeatInputTargeter::new();
    let mut supervisor_options = options(SeatDisplayBackend::Weston);
    supervisor_options.input_isolation = true;
    let mut supervisor = ProcessSeatSupervisor::with_parts(
        supervisor_options,
        Arc::new(node.clone()),
        Arc::new(FakeReadinessProbe::always_ready()),
    )
    .with_display_launcher(Box::new(displays))
    .with_input_isolation_helper(Box::new(helper.clone()))
    .with_input_targeter(Box::new(targeter.clone()));

    let status = supervisor.start(&plan.seats[0]).expect("start");
    assert_eq!(status.state, SeatState::Running);
    assert!(status.resources.input_isolation_applied);
    assert_eq!(status.resources.grabbed_device_count, 2);
    assert!(!status.resources.input_targeting_applied);
    assert!(status.resources.targeted_devices.is_empty());
    assert!(status.resources.disabled_devices.is_empty());
    assert_eq!(targeter.apply_count(), 0);
    assert!(targeter.clears().iter().any(|seat| seat == "host-seat-0"));
    assert!(status
        .display
        .display_notes
        .iter()
        .any(|note| note.kind == SeatDisplayNoteKind::InputTargetingUnavailable));

    let stopped = supervisor.stop("host-seat-0").expect("stop");
    assert!(!stopped.resources.input_isolation_applied);
    assert!(stopped.resources.input_isolation_released);
    assert!(!helper.has_assignment("host-seat-0"));
}
