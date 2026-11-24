// SPDX-License-Identifier: AGPL-3.0-only

use std::sync::Arc;
use std::time::Duration;

use ozma_multiseat::{
    DisplayOutput, FakeNodeLauncher, FakeReadinessProbe, FakeSeatAudioRouter, MultiseatPlan,
    MultiseatPlanner, PlannerOptions, ProcessSeatSupervisor, ResourceAction, ResourceKind,
    SeatState, SeatSupervisor, SupervisorOptions,
};

fn plan() -> MultiseatPlan {
    let displays = vec![
        DisplayOutput::new(0, "HDMI-1", 1920, 1080, 0, 0, ":0"),
        DisplayOutput::new(1, "DP-2", 1920, 1080, 1920, 0, ":0"),
    ];
    MultiseatPlanner::new(PlannerOptions {
        machine_name: "host".to_owned(),
        ..PlannerOptions::default()
    })
    .plan(&displays, &[])
    .expect("plan")
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
    }
}

fn supervisor(dry_run: bool, audio: &FakeSeatAudioRouter) -> ProcessSeatSupervisor {
    ProcessSeatSupervisor::with_parts(
        options(dry_run),
        Arc::new(FakeNodeLauncher::new()),
        Arc::new(FakeReadinessProbe::new()),
    )
    .with_audio_router(Box::new(audio.clone()))
}

#[test]
fn audio_sink_is_ensured_before_spawn_and_removed_after_stop() {
    let plan = plan();
    let audio = FakeSeatAudioRouter::new();
    let mut supervisor = supervisor(false, &audio);

    let status = supervisor.start(&plan.seats[0]).expect("start");
    assert_eq!(status.state, SeatState::Running);
    assert!(status.resources.audio_applied);
    assert!(!status.resources.audio_released);
    assert!(audio.is_present("ozma-seat-0"));

    let ensured = audio.ensured();
    assert_eq!(ensured.len(), 1);
    assert_eq!(ensured[0].name, "ozma-seat-0");
    assert_eq!(ensured[0].monitor_target(), "ozma-seat-0.monitor");
    assert!(ensured[0].monitor);

    let status = supervisor.stop("host-seat-0").expect("stop");
    assert_eq!(status.state, SeatState::Stopped);
    assert!(!status.resources.audio_applied);
    assert!(status.resources.audio_released);
    assert!(!audio.is_present("ozma-seat-0"));
    assert_eq!(audio.removed(), vec!["ozma-seat-0".to_owned()]);
}

#[test]
fn audio_ensure_failure_is_typed_non_fatal_and_reported() {
    let plan = plan();
    let audio = FakeSeatAudioRouter::new().fail_next_ensure("pactl is not installed");
    let mut supervisor = supervisor(false, &audio);

    let status = supervisor
        .start(&plan.seats[0])
        .expect("start despite audio");
    assert_eq!(status.state, SeatState::Running);
    assert!(!status.resources.audio_applied);
    assert!(!audio.is_present("ozma-seat-0"));
    assert_eq!(audio.ensured().len(), 0);
    assert!(status.resources.errors.iter().any(|error| {
        error.kind == ResourceKind::Audio
            && error.action == ResourceAction::Apply
            && error.message.contains("pactl is not installed")
    }));

    // No sink was applied, so stop does not pretend to remove one.
    let status = supervisor.stop("host-seat-0").expect("stop");
    assert_eq!(status.state, SeatState::Stopped);
    assert!(audio.removed().is_empty());
}

#[test]
fn audio_remove_failure_is_best_effort_and_keeps_the_seat_for_a_retry() {
    let plan = plan();
    let audio = FakeSeatAudioRouter::new().fail_next_remove("audio server busy");
    let mut supervisor = supervisor(false, &audio);

    supervisor.start(&plan.seats[0]).expect("start");
    // `remove` stops the child once; the failing sink release keeps the seat
    // (and its applied audio flag) for a later retry.
    let status = supervisor.remove("host-seat-0").expect("retained");
    assert_eq!(status.state, SeatState::Stopped);
    assert!(status.resources.audio_applied);
    assert!(audio.is_present("ozma-seat-0"));
    assert!(status.resources.errors.iter().any(|error| {
        error.kind == ResourceKind::Audio
            && error.action == ResourceAction::Release
            && error.message.contains("audio server busy")
    }));
    assert_eq!(supervisor.seat_count(), 1);
    let status = supervisor.remove("host-seat-0").expect("retry");
    assert!(!status.resources.audio_applied);
    assert!(status.resources.audio_released);
    assert_eq!(supervisor.seat_count(), 0);
    assert_eq!(audio.removed(), vec!["ozma-seat-0".to_owned()]);
}

#[test]
fn dry_run_never_touches_the_audio_router() {
    let plan = plan();
    let audio = FakeSeatAudioRouter::new();
    let mut supervisor = supervisor(true, &audio);

    let status = supervisor.start(&plan.seats[0]).expect("dry run");
    assert_eq!(status.state, SeatState::Planned);
    assert!(audio.ensured().is_empty());
    assert!(audio.removed().is_empty());
    assert!(status.resources.is_empty());
}

#[test]
fn every_seat_gets_its_own_sink_and_monitor_target() {
    let plan = plan();
    let audio = FakeSeatAudioRouter::new();
    let mut supervisor = supervisor(false, &audio);

    supervisor.start(&plan.seats[0]).expect("seat 0");
    supervisor.start(&plan.seats[1]).expect("seat 1");

    let ensured = audio.ensured();
    assert_eq!(ensured.len(), 2);
    assert_eq!(ensured[0].name, "ozma-seat-0");
    assert_eq!(ensured[0].monitor_target(), "ozma-seat-0.monitor");
    assert_eq!(ensured[1].name, "ozma-seat-1");
    assert_eq!(ensured[1].monitor_target(), "ozma-seat-1.monitor");
    assert!(audio.is_present("ozma-seat-0"));
    assert!(audio.is_present("ozma-seat-1"));

    supervisor.stop_all();
    assert!(!audio.is_present("ozma-seat-0"));
    assert!(!audio.is_present("ozma-seat-1"));
    assert_eq!(
        audio.removed(),
        vec!["ozma-seat-0".to_owned(), "ozma-seat-1".to_owned()]
    );
}
