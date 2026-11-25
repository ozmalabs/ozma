// SPDX-License-Identifier: AGPL-3.0-only
//!
//! Supervisor integration for opt-in helper input isolation. Every test uses
//! fake launchers, readiness probes, and a fake helper; no node process,
//! evdev node, uinput device, or X server is touched.

use std::sync::Arc;
use std::time::{Duration, Instant};

use ozma_multiseat::{
    DisplayOutput, FakeDisplayBackend, FakeInputBackend, FakeNodeLauncher, FakeReadinessProbe,
    FakeSeatInputHelper, HostTopology, InputDevice, InputDeviceKind, MultiseatError, MultiseatPlan,
    MultiseatPlanner, PlannerOptions, ProcessSeatSupervisor, ResourceKind, SeatState,
    SeatSupervisor, SupervisorOptions,
};

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

fn options(dry_run: bool) -> SupervisorOptions {
    SupervisorOptions {
        node_bin: None,
        readiness_timeout: Duration::from_millis(100),
        readiness_poll: Duration::from_millis(1),
        restart_backoff: Duration::from_millis(10),
        restart_backoff_max: Duration::from_millis(40),
        healthy_uptime: Duration::from_secs(30),
        dry_run,
        apply_ownership: false,
        apply_routing: false,
        input_isolation: true,
    }
}

fn supervisor(
    helper: FakeSeatInputHelper,
    dry_run: bool,
) -> (ProcessSeatSupervisor, FakeNodeLauncher) {
    let launcher = FakeNodeLauncher::new();
    let supervisor = ProcessSeatSupervisor::with_parts(
        options(dry_run),
        Arc::new(launcher.clone()),
        Arc::new(FakeReadinessProbe::always_ready()),
    )
    .with_input_isolation_helper(Box::new(helper));
    (supervisor, launcher)
}

#[test]
fn start_confirms_grab_and_status_reports_devices_and_uinput_names() {
    let plan = plan();
    let helper = FakeSeatInputHelper::new();
    let (mut supervisor, launcher) = supervisor(helper.clone(), false);

    let status = supervisor.start(&plan.seats[0]).expect("start seat 0");
    assert_eq!(status.state, SeatState::Running);
    assert!(status.resources.input_isolation_applied);
    assert_eq!(status.resources.grabbed_device_count, 2);
    assert_eq!(status.resources.uinput_devices.len(), 2);
    assert_eq!(
        helper.grabbed("host-seat-0"),
        vec!["/dev/input/event0", "/dev/input/event1"]
    );
    assert!(helper
        .uinput_devices("host-seat-0")
        .iter()
        .all(|name| name.starts_with("ozma-seat-host-seat-0")));
    assert_eq!(launcher.spawn_count(), 1);
    assert_eq!(helper.start_count(), 1);

    let stopped = supervisor.stop("host-seat-0").expect("stop");
    assert_eq!(stopped.state, SeatState::Stopped);
    assert!(!stopped.resources.input_isolation_applied);
    assert!(stopped.resources.input_isolation_released);
    assert_eq!(stopped.resources.grabbed_device_count, 0);
    assert!(stopped.resources.uinput_devices.is_empty());
    assert!(!helper.has_assignment("host-seat-0"));
    assert!(launcher.stopped().contains(&"host-seat-0".to_owned()));
}

#[test]
fn grab_failure_prevents_spawn_and_rolls_back_earlier_seats() {
    let plan = plan();
    let helper = FakeSeatInputHelper::new().fail_seat("host-seat-1");
    let (mut supervisor, launcher) = supervisor(helper.clone(), false);

    supervisor.start(&plan.seats[0]).expect("start seat 0");
    assert!(helper.has_assignment("host-seat-0"));

    let error = supervisor.start(&plan.seats[1]).expect_err("seat 1 grab");
    assert!(error.to_string().contains("injected grab failure"));
    assert_eq!(launcher.spawn_count(), 1, "seat 1 must not spawn");

    let statuses = supervisor.status();
    let seat0 = statuses
        .iter()
        .find(|status| status.seat_id == "host-seat-0")
        .expect("seat 0 status");
    assert!(!seat0.resources.input_isolation_applied);
    assert!(seat0.resources.input_isolation_released);
    assert!(!helper.has_assignment("host-seat-0"));

    let seat1 = statuses
        .iter()
        .find(|status| status.seat_id == "host-seat-1")
        .expect("seat 1 status");
    assert_eq!(seat1.state, SeatState::Failed);
    assert!(seat1.resources.errors.iter().any(|error| {
        error.kind == ResourceKind::InputIsolation
            && error.action == ozma_multiseat::ResourceAction::Apply
    }));
}

#[test]
fn isolation_without_a_helper_is_unsupported_and_does_not_spawn() {
    let plan = plan();
    let launcher = FakeNodeLauncher::new();
    let mut supervisor = ProcessSeatSupervisor::with_parts(
        options(false),
        Arc::new(launcher.clone()),
        Arc::new(FakeReadinessProbe::always_ready()),
    );

    let error = supervisor.start(&plan.seats[0]).expect_err("no helper");
    assert!(matches!(error, MultiseatError::Unsupported(_)));
    assert_eq!(launcher.spawn_count(), 0);
    let status = supervisor
        .status()
        .into_iter()
        .find(|status| status.seat_id == "host-seat-0")
        .expect("seat status");
    assert_eq!(status.state, SeatState::Failed);
    assert!(!status.resources.input_isolation_applied);
}

#[test]
fn helper_readiness_failure_is_a_typed_start_failure_without_spawn() {
    let plan = plan();
    let helper = FakeSeatInputHelper::new().fail_ready("helper cannot start");
    let (mut supervisor, launcher) = supervisor(helper.clone(), false);

    let error = supervisor.start(&plan.seats[0]).expect_err("helper ready");
    assert!(error.to_string().contains("helper cannot start"));
    assert_eq!(launcher.spawn_count(), 0);
    assert!(!helper.has_assignment("host-seat-0"));

    let status = supervisor
        .status()
        .into_iter()
        .find(|status| status.seat_id == "host-seat-0")
        .expect("seat status");
    assert_eq!(status.state, SeatState::Failed);
    assert!(status.resources.errors.iter().any(|error| {
        error.kind == ResourceKind::InputIsolation
            && error.action == ozma_multiseat::ResourceAction::Apply
    }));
}

#[test]
fn release_failure_keeps_the_assignment_and_seat_for_retry() {
    let plan = plan();
    let helper = FakeSeatInputHelper::new().fail_next_release("injected release failure");
    let (mut supervisor, _launcher) = supervisor(helper.clone(), false);

    supervisor.start(&plan.seats[0]).expect("start");
    let stopped = supervisor.stop("host-seat-0").expect("stop");
    assert_eq!(stopped.state, SeatState::Stopped);
    assert!(stopped.resources.input_isolation_applied);
    assert!(stopped.resources.errors.iter().any(|error| {
        error.kind == ResourceKind::InputIsolation
            && error.action == ozma_multiseat::ResourceAction::Release
    }));
    assert!(helper.has_assignment("host-seat-0"));

    // A second remove retries the release now that the injected failure is
    // consumed, then forgets the seat.
    let removed = supervisor.remove("host-seat-0").expect("retry remove");
    assert!(!removed.resources.input_isolation_applied);
    assert!(removed.resources.input_isolation_released);
    assert!(!helper.has_assignment("host-seat-0"));
    assert_eq!(supervisor.seat_count(), 0);
}

#[test]
fn helper_crash_is_detected_and_the_grab_is_restored_without_respawning_the_node() {
    let plan = plan();
    let helper = FakeSeatInputHelper::new();
    let (mut supervisor, launcher) = supervisor(helper.clone(), false);

    supervisor.start(&plan.seats[0]).expect("start");
    assert_eq!(helper.start_count(), 1);

    helper.crash();
    assert!(!helper.has_assignment("host-seat-0"));

    supervisor.tick_at(Instant::now());

    let status = supervisor
        .status()
        .into_iter()
        .find(|status| status.seat_id == "host-seat-0")
        .expect("status");
    assert!(status.resources.input_isolation_applied);
    assert_eq!(status.resources.grabbed_device_count, 2);
    assert!(helper.has_assignment("host-seat-0"));
    assert_eq!(helper.start_count(), 2);
    assert_eq!(launcher.spawn_count(), 1, "node must not respawn");
    assert!(status.resources.errors.iter().any(|error| {
        error.kind == ResourceKind::InputIsolation
            && (error.message.contains("no longer reports")
                || error.message.contains("lost the seat's grab"))
    }));
}

#[test]
fn cross_seat_device_conflict_is_a_typed_start_failure() {
    let mut plan = plan();
    // Force a conflict the planner would normally avoid: both seats claim the
    // same physical group.
    plan.seats[1].input = plan.seats[0].input.clone();
    plan.seats[1].config.input_devices = plan.seats[0].config.input_devices.clone();

    let helper = FakeSeatInputHelper::new();
    let (mut supervisor, launcher) = supervisor(helper.clone(), false);

    supervisor.start(&plan.seats[0]).expect("start seat 0");
    let error = supervisor.start(&plan.seats[1]).expect_err("conflict");
    assert!(error.to_string().contains("already grabbed"));
    assert_eq!(launcher.spawn_count(), 1);
    assert!(!helper.has_assignment("host-seat-0"));
    assert!(!helper.has_assignment("host-seat-1"));

    let seat1 = supervisor
        .status()
        .into_iter()
        .find(|status| status.seat_id == "host-seat-1")
        .expect("seat 1");
    assert_eq!(seat1.state, SeatState::Failed);
}

#[test]
fn dry_run_never_starts_the_helper_or_grabs() {
    let plan = plan();
    let helper = FakeSeatInputHelper::new();
    let (mut supervisor, launcher) = supervisor(helper.clone(), true);

    let status = supervisor.start(&plan.seats[0]).expect("dry run");
    assert_eq!(status.state, SeatState::Planned);
    assert!(status
        .message
        .as_deref()
        .is_some_and(|message| message.contains("helper")));
    assert_eq!(helper.start_count(), 0);
    assert_eq!(launcher.spawn_count(), 0);
}

#[test]
fn default_supervisor_options_leave_input_isolation_off() {
    assert!(!SupervisorOptions::default().input_isolation);
    assert!(!SupervisorOptions::default().apply_routing);
}
