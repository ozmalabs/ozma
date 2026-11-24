// SPDX-License-Identifier: AGPL-3.0-only

use ozma_multiseat::{
    DisplayOutput, FakeSeatSupervisor, MultiseatPlan, MultiseatPlanner, MultiseatStatus,
    PlannerOptions, SeatState, SeatSupervisor,
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

#[test]
fn fake_supervisor_starts_stops_and_reports_each_seat() {
    let plan = plan();
    let mut supervisor = FakeSeatSupervisor::new();

    let status = supervisor.start(&plan.seats[0]).expect("start 0");
    assert_eq!(status.state, SeatState::Running);
    assert!(matches!(
        supervisor.start(&plan.seats[0]).expect_err("duplicate start"),
        ozma_multiseat::MultiseatError::SeatAlreadyRunning(id) if id == "host-seat-0"
    ));

    supervisor.start(&plan.seats[1]).expect("start 1");
    assert_eq!(supervisor.start_count("host-seat-0"), 1);
    assert_eq!(supervisor.status().len(), 2);

    let status = supervisor.stop("host-seat-0").expect("stop");
    assert_eq!(status.state, SeatState::Stopped);
    assert_eq!(supervisor.state("host-seat-0"), Some(SeatState::Stopped));
    assert_eq!(supervisor.stop_count("host-seat-0"), 1);
    assert!(matches!(
        supervisor.stop("missing").expect_err("unknown"),
        ozma_multiseat::MultiseatError::UnknownSeat(id) if id == "missing"
    ));
}

#[test]
fn injected_start_failure_is_reported_and_can_be_retried() {
    let plan = plan();
    let mut supervisor = FakeSeatSupervisor::new().fail_next_start("host-seat-0");

    let error = supervisor.start(&plan.seats[0]).expect_err("injected");
    assert!(matches!(
        error,
        ozma_multiseat::MultiseatError::Supervisor(_)
    ));
    assert_eq!(supervisor.state("host-seat-0"), Some(SeatState::Failed));

    let status = supervisor.start(&plan.seats[0]).expect("retry");
    assert_eq!(status.state, SeatState::Running);
}

#[test]
fn supervisor_status_merges_into_the_controller_document() {
    let plan = plan();
    let mut supervisor = FakeSeatSupervisor::new();
    supervisor.start(&plan.seats[0]).expect("start");
    supervisor.stop("host-seat-0").expect("stop");

    let status = MultiseatStatus::from_plan_with_runtime(&plan, &supervisor.status());
    assert_eq!(status.capability.node_count, 2);
    assert_eq!(status.seats[0].state, SeatState::Stopped);
    assert_eq!(status.seats[1].state, SeatState::Planned);
}
