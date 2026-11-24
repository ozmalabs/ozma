// SPDX-License-Identifier: AGPL-3.0-only
//!
//! Opt-in ownership/routing execution in `ProcessSeatSupervisor`.
//!
//! Every test uses a fake command runner, fake device-name resolver, fake
//! launcher, and fake readiness probe. No `xrandr`, `xinput`, node, evdev node,
//! or display is touched.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ozma_multiseat::{
    ChildHandle, CommandOutput, CommandRunner, DisplayOutput, FakeCommandRunner,
    FakeDeviceNameResolver, FakeDisplayBackend, FakeInputBackend, FakeNodeLauncher,
    FakeReadinessProbe, HostTopology, InputDevice, InputDeviceKind, MultiseatError, MultiseatPlan,
    MultiseatPlanner, NodeLaunchSpec, NodeLauncher, PlannerOptions, ProcessSeatSupervisor,
    RouteCommand, SeatRuntimeStatus, SeatState, SeatSupervisor, SupervisorOptions, XInputRouter,
    XrandrOwnershipController,
};

const XINPUT_LIST: &str = "\
⎡ Virtual core pointer                     id=2    [master pointer  (3)]
⎜   ↳ Mouse A                              id=12   [slave  pointer  (2)]
⎜   ↳ Mouse B                              id=13   [slave  pointer  (2)]
";

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
struct RecordingRunner {
    inner: FakeCommandRunner,
    log: EventLog,
}

impl CommandRunner for RecordingRunner {
    fn run(&self, command: &RouteCommand) -> Result<CommandOutput, MultiseatError> {
        self.log.push(format!(
            "cmd {} {}",
            command.program,
            command.args.join(" ")
        ));
        self.inner.run(command)
    }
}

#[derive(Clone)]
struct RecordingLauncher {
    inner: FakeNodeLauncher,
    log: EventLog,
}

impl RecordingLauncher {
    fn new(log: EventLog) -> Self {
        Self {
            inner: FakeNodeLauncher::new(),
            log,
        }
    }

    fn spawn_count(&self) -> usize {
        self.inner.spawn_count()
    }
}

impl NodeLauncher for RecordingLauncher {
    fn spawn(&self, spec: &NodeLaunchSpec) -> Result<Box<dyn ChildHandle>, MultiseatError> {
        self.log.push(format!("spawn {}", spec.seat_id));
        self.inner.spawn(spec)
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
    let planner = MultiseatPlanner::new(PlannerOptions {
        machine_name: "host".to_owned(),
        ..PlannerOptions::default()
    });
    topology.plan(&planner).expect("plan")
}

fn resolver() -> FakeDeviceNameResolver {
    FakeDeviceNameResolver::new()
        .with_name("/dev/input/event1", "Mouse A")
        .with_name("/dev/input/event3", "Mouse B")
}

fn scripted_runner() -> FakeCommandRunner {
    FakeCommandRunner::new()
        .respond_stdout("xinput", &["list", "--short"], XINPUT_LIST)
        .respond(
            "xinput",
            &["map-to-output", "12", "HDMI-1"],
            CommandOutput::success(),
        )
        .respond(
            "xinput",
            &["map-to-output", "13", "DP-2"],
            CommandOutput::success(),
        )
        .respond(
            "xinput",
            &["map-to-output", "12", "all"],
            CommandOutput::success(),
        )
        .respond(
            "xinput",
            &["map-to-output", "13", "all"],
            CommandOutput::success(),
        )
        .respond(
            "xrandr",
            &["--output", "HDMI-1", "--mode", "1920x1080", "--pos", "0x0"],
            CommandOutput::success(),
        )
        .respond(
            "xrandr",
            &["--output", "DP-2", "--mode", "1920x1080", "--pos", "1920x0"],
            CommandOutput::success(),
        )
        .respond(
            "xrandr",
            &["--output", "HDMI-1", "--auto"],
            CommandOutput::success(),
        )
        .respond(
            "xrandr",
            &["--output", "DP-2", "--auto"],
            CommandOutput::success(),
        )
}

fn options(dry_run: bool, apply_ownership: bool, apply_routing: bool) -> SupervisorOptions {
    SupervisorOptions {
        node_bin: None,
        readiness_timeout: Duration::from_millis(100),
        readiness_poll: Duration::from_millis(1),
        restart_backoff: Duration::from_millis(10),
        restart_backoff_max: Duration::from_millis(40),
        healthy_uptime: Duration::from_secs(30),
        dry_run,
        apply_ownership,
        apply_routing,
    }
}

fn supervisor(
    options: SupervisorOptions,
    launcher: RecordingLauncher,
    runner: FakeCommandRunner,
) -> ProcessSeatSupervisor {
    let runner = RecordingRunner {
        inner: runner,
        log: launcher.log.clone(),
    };
    ProcessSeatSupervisor::with_parts(
        options,
        Arc::new(launcher),
        Arc::new(FakeReadinessProbe::new()),
    )
    .with_ownership_controller(Box::new(XrandrOwnershipController::new(Box::new(
        runner.clone(),
    ))))
    .with_routing_controller(Box::new(XInputRouter::with_resolver(
        Box::new(runner),
        Box::new(resolver()),
    )))
}

fn status_for(supervisor: &ProcessSeatSupervisor, seat_id: &str) -> SeatRuntimeStatus {
    supervisor
        .status()
        .into_iter()
        .find(|status| status.seat_id == seat_id)
        .expect("seat status")
}

#[test]
fn ownership_then_routing_is_applied_before_spawn_and_reported() {
    let plan = plan();
    let log = EventLog::default();
    let launcher = RecordingLauncher::new(log.clone());
    let mut supervisor = supervisor(
        options(false, true, true),
        launcher.clone(),
        scripted_runner(),
    );

    let status = supervisor.start(&plan.seats[0]).expect("start seat 0");
    assert_eq!(status.state, SeatState::Running);
    assert!(status.resources.ownership_applied);
    assert!(status.resources.routing_applied);
    assert!(!status.resources.ownership_released);
    assert!(!status.resources.routing_released);
    assert!(status.resources.errors.is_empty());
    let json = serde_json::to_value(&status).expect("status json");
    assert_eq!(json["resources"]["ownership_applied"], true);
    assert_eq!(json["resources"]["routing_applied"], true);

    let events = log.snapshot();
    assert_eq!(
        events,
        vec![
            "cmd xrandr --output HDMI-1 --mode 1920x1080 --pos 0x0",
            "cmd xinput list --short",
            "cmd xinput map-to-output 12 HDMI-1",
            "spawn host-seat-0",
        ],
        "events: {events:?}"
    );

    let status = supervisor.start(&plan.seats[1]).expect("start seat 1");
    assert_eq!(status.state, SeatState::Running);
    assert!(status.resources.ownership_applied);
    assert!(status.resources.routing_applied);

    let events = log.snapshot();
    assert_eq!(
        &events[4..],
        [
            "cmd xrandr --output DP-2 --mode 1920x1080 --pos 1920x0",
            "cmd xinput list --short",
            "cmd xinput map-to-output 13 DP-2",
            "spawn host-seat-1",
        ],
        "events: {events:?}"
    );
}

#[test]
fn later_seat_apply_failure_does_not_spawn_and_releases_earlier_resources() {
    let plan = plan();
    let log = EventLog::default();
    let launcher = RecordingLauncher::new(log.clone());
    let runner = FakeCommandRunner::new()
        .respond_stdout("xinput", &["list", "--short"], XINPUT_LIST)
        .respond(
            "xinput",
            &["map-to-output", "12", "HDMI-1"],
            CommandOutput::success(),
        )
        .respond(
            "xinput",
            &["map-to-output", "12", "all"],
            CommandOutput::success(),
        )
        .respond(
            "xrandr",
            &["--output", "HDMI-1", "--mode", "1920x1080", "--pos", "0x0"],
            CommandOutput::success(),
        )
        .respond(
            "xrandr",
            &["--output", "HDMI-1", "--auto"],
            CommandOutput::success(),
        )
        .respond_failure(
            "xrandr",
            &["--output", "DP-2", "--mode", "1920x1080", "--pos", "1920x0"],
            1,
            "cannot find output DP-2",
        );
    let mut supervisor = supervisor(options(false, true, true), launcher.clone(), runner);

    supervisor.start(&plan.seats[0]).expect("start seat 0");
    assert_eq!(launcher.spawn_count(), 1);

    let error = supervisor.start(&plan.seats[1]).expect_err("seat 1 apply");
    assert!(
        matches!(
            &error,
            MultiseatError::CommandFailed { program, status, .. }
                if program == "xrandr" && *status == 1
        ),
        "error: {error:?}"
    );
    assert_eq!(launcher.spawn_count(), 1, "failed apply must not spawn");
    assert!(!launcher.inner.active("host-seat-1"));

    let failed = status_for(&supervisor, "host-seat-1");
    assert_eq!(failed.state, SeatState::Failed);
    assert!(!failed.resources.ownership_applied);
    assert!(!failed.resources.routing_applied);
    assert!(failed
        .last_error
        .as_deref()
        .is_some_and(|error| error.contains("cannot find output DP-2")));
    assert!(failed
        .resources
        .errors
        .iter()
        .any(|error| error.kind.as_str() == "ownership"
            && error.action.as_str() == "apply"
            && error.message.contains("cannot find output DP-2")));

    let earlier = status_for(&supervisor, "host-seat-0");
    assert!(!earlier.resources.ownership_applied);
    assert!(!earlier.resources.routing_applied);
    assert!(earlier.resources.ownership_released);
    assert!(earlier.resources.routing_released);

    let events = log.snapshot();
    assert!(events.contains(&"cmd xinput map-to-output 12 all".to_owned()));
    assert!(events.contains(&"cmd xrandr --output HDMI-1 --auto".to_owned()));
}

#[test]
fn stop_and_remove_release_routing_before_ownership() {
    let plan = plan();
    let log = EventLog::default();
    let launcher = RecordingLauncher::new(log.clone());
    let mut supervisor = supervisor(
        options(false, true, true),
        launcher.clone(),
        scripted_runner(),
    );

    supervisor.start(&plan.seats[0]).expect("start");
    let stopped = supervisor.stop("host-seat-0").expect("stop");
    assert_eq!(stopped.state, SeatState::Stopped);
    assert!(!stopped.resources.ownership_applied);
    assert!(!stopped.resources.routing_applied);
    assert!(stopped.resources.ownership_released);
    assert!(stopped.resources.routing_released);

    let events = log.snapshot();
    let release: Vec<&String> = events
        .iter()
        .filter(|event| event.contains(" all") || event.contains("--auto"))
        .collect();
    assert_eq!(
        release,
        vec![
            &"cmd xinput map-to-output 12 all".to_owned(),
            &"cmd xrandr --output HDMI-1 --auto".to_owned(),
        ],
        "events: {events:?}"
    );

    // A restarted seat re-applies, and remove releases again.
    supervisor.start(&plan.seats[0]).expect("restart");
    let removed = supervisor.remove("host-seat-0").expect("remove");
    assert_eq!(removed.state, SeatState::Stopped);
    assert!(removed.resources.ownership_released);
    assert!(removed.resources.routing_released);
    assert!(supervisor.status().is_empty());
}

#[test]
fn release_failures_are_best_effort_and_stay_in_status() {
    let plan = plan();
    let log = EventLog::default();
    let launcher = RecordingLauncher::new(log.clone());
    // Apply commands succeed; both release commands fail.
    let runner = FakeCommandRunner::new()
        .respond_stdout("xinput", &["list", "--short"], XINPUT_LIST)
        .respond(
            "xinput",
            &["map-to-output", "12", "HDMI-1"],
            CommandOutput::success(),
        )
        .respond(
            "xrandr",
            &["--output", "HDMI-1", "--mode", "1920x1080", "--pos", "0x0"],
            CommandOutput::success(),
        )
        .respond_failure(
            "xrandr",
            &["--output", "HDMI-1", "--auto"],
            1,
            "xrandr refused release",
        );
    let mut supervisor = supervisor(options(false, true, true), launcher, runner);

    supervisor.start(&plan.seats[0]).expect("start");
    let stopped = supervisor.stop("host-seat-0").expect("best-effort stop");
    assert_eq!(stopped.state, SeatState::Stopped);
    // Failed releases keep the applied flag so a later stop can retry.
    assert!(stopped.resources.ownership_applied);
    assert!(stopped.resources.routing_applied);
    assert!(!stopped.resources.ownership_released);
    assert!(!stopped.resources.routing_released);
    assert_eq!(stopped.resources.errors.len(), 2);
    assert!(stopped
        .resources
        .errors
        .iter()
        .any(|error| error.kind.as_str() == "ownership"
            && error.action.as_str() == "release"
            && error.message.contains("refused release")));
    assert!(stopped
        .resources
        .errors
        .iter()
        .any(|error| error.kind.as_str() == "routing" && error.action.as_str() == "release"));

    // A failed release keeps the seat so a later remove/stop can retry rather
    // than dropping the only handle to the still-applied resource.
    let retained = supervisor.remove("host-seat-0").expect("retry remove");
    assert_eq!(retained.state, SeatState::Stopped);
    assert!(retained.resources.ownership_applied || retained.resources.routing_applied);
    assert_eq!(supervisor.status().len(), 1);
}

#[test]
fn dry_run_with_apply_flags_performs_no_actions() {
    let plan = plan();
    let log = EventLog::default();
    let launcher = RecordingLauncher::new(log.clone());
    let runner = FakeCommandRunner::new();
    let mut supervisor = supervisor(options(true, true, true), launcher.clone(), runner.clone());

    let status = supervisor.start(&plan.seats[0]).expect("dry run");
    assert_eq!(status.state, SeatState::Planned);
    assert_eq!(status.pid, None);
    assert!(status.resources.is_empty());
    assert_eq!(launcher.spawn_count(), 0);
    assert_eq!(runner.run_count(), 0);
    assert!(log.snapshot().is_empty());
    let message = status.message.as_deref().expect("dry-run message");
    assert!(message.contains("would apply xrandr ownership"));
    assert!(message.contains("would apply input routing"));
}

#[test]
fn flags_off_never_invoke_controllers() {
    let plan = plan();
    let log = EventLog::default();
    let launcher = RecordingLauncher::new(log.clone());
    let runner = FakeCommandRunner::new();
    let mut supervisor = supervisor(
        options(false, false, false),
        launcher.clone(),
        runner.clone(),
    );

    let status = supervisor.start(&plan.seats[0]).expect("start");
    assert_eq!(status.state, SeatState::Running);
    assert!(status.resources.is_empty());
    assert_eq!(runner.run_count(), 0);
    let json = serde_json::to_value(&status).expect("status json");
    assert!(json.get("resources").is_none());
    assert_equals_spawn_only(&log.snapshot());
}

fn assert_equals_spawn_only(events: &[String]) {
    assert_eq!(events, ["spawn host-seat-0"], "events: {events:?}");
}

#[test]
fn missing_tools_are_typed_and_never_spawn() {
    let plan = plan();

    let log = EventLog::default();
    let launcher = RecordingLauncher::new(log.clone());
    let runner = scripted_runner().with_unavailable("xrandr");
    let mut ownership_supervisor = supervisor(options(false, true, true), launcher.clone(), runner);
    let error = ownership_supervisor
        .start(&plan.seats[0])
        .expect_err("xrandr missing");
    assert!(
        matches!(&error, MultiseatError::CommandUnavailable { program, .. } if program == "xrandr"),
        "error: {error:?}"
    );
    assert_eq!(launcher.spawn_count(), 0);
    let status = status_for(&ownership_supervisor, "host-seat-0");
    assert_eq!(status.state, SeatState::Failed);
    assert!(status
        .resources
        .errors
        .iter()
        .any(|error| error.message.contains("unavailable")));

    let log = EventLog::default();
    let launcher = RecordingLauncher::new(log.clone());
    let runner = scripted_runner().with_unavailable("xinput");
    let mut routing_supervisor = supervisor(options(false, false, true), launcher.clone(), runner);
    let error = routing_supervisor
        .start(&plan.seats[0])
        .expect_err("xinput missing");
    assert!(
        matches!(&error, MultiseatError::CommandUnavailable { program, .. } if program == "xinput"),
        "error: {error:?}"
    );
    assert_eq!(launcher.spawn_count(), 0);
}

#[test]
fn evdev_strategy_without_a_backend_is_typed_unsupported() {
    let mut plan = plan();
    plan.seats[0].input_route = plan.seats[0]
        .input_route
        .clone()
        .map(|route| route.with_exclusive_grab());

    let log = EventLog::default();
    let launcher = RecordingLauncher::new(log.clone());
    let runner = scripted_runner();
    // Only the xinput router is installed; no evdev backend is configured.
    let mut supervisor = ProcessSeatSupervisor::with_parts(
        options(false, false, true),
        Arc::new(launcher.clone()),
        Arc::new(FakeReadinessProbe::new()),
    )
    .with_routing_controller(Box::new(XInputRouter::with_resolver(
        Box::new(RecordingRunner {
            inner: runner,
            log: log.clone(),
        }),
        Box::new(resolver()),
    )));

    let error = supervisor
        .start(&plan.seats[0])
        .expect_err("evdev unsupported");
    assert!(
        matches!(&error, MultiseatError::Unsupported(message) if message.contains("evdev")),
        "error: {error:?}"
    );
    assert_eq!(launcher.spawn_count(), 0);
    let status = status_for(&supervisor, "host-seat-0");
    assert!(!status.resources.routing_applied);
    assert!(status
        .resources
        .errors
        .iter()
        .any(|error| error.kind.as_str() == "routing" && error.message.contains("unsupported")));
}
