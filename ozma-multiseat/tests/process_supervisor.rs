// SPDX-License-Identifier: AGPL-3.0-only

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ozma_multiseat::{
    allocate_ports, render_metrics, restart_delay, DiagnosticCode, DisplayOutput,
    FakeDisplayBackend, FakeInputBackend, FakeNodeLauncher, FakeReadinessProbe, HostTopology,
    HttpHealthProbe, MultiseatError, MultiseatPlan, MultiseatPlanner, MultiseatStatus,
    NodeLaunchSpec, PlannerOptions, ProcessSeatSupervisor, ReadinessProbe, SeatState,
    SeatSupervisor, SupervisorOptions,
};

fn display(index: u32, name: &str, x: i32) -> DisplayOutput {
    DisplayOutput::new(index, name, 1920, 1080, x, 0, ":0")
}

fn plan_with(displays: Vec<DisplayOutput>, base_udp: u16, base_http: u16) -> MultiseatPlan {
    let topology = HostTopology::enumerate(
        &FakeDisplayBackend::new(displays),
        &FakeInputBackend::empty(),
    )
    .expect("topology");
    let planner = MultiseatPlanner::new(PlannerOptions {
        machine_name: "host".to_owned(),
        base_udp_port: base_udp,
        base_http_port: base_http,
        ..PlannerOptions::default()
    });
    topology.plan(&planner).expect("plan")
}

fn two_seat_plan() -> MultiseatPlan {
    plan_with(
        vec![display(0, "HDMI-1", 0), display(1, "DP-2", 1920)],
        7331,
        7382,
    )
}

fn options() -> SupervisorOptions {
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
    }
}

fn supervisor(launcher: &FakeNodeLauncher, probe: FakeReadinessProbe) -> ProcessSeatSupervisor {
    ProcessSeatSupervisor::with_parts(options(), Arc::new(launcher.clone()), Arc::new(probe))
}

#[test]
fn plan_to_spawn_uses_exact_argv_environment_and_ports() {
    let plan = two_seat_plan();
    let launcher = FakeNodeLauncher::new();
    let mut supervisor = supervisor(&launcher, FakeReadinessProbe::new());

    let first = supervisor.start(&plan.seats[0]).expect("start seat 0");
    assert_eq!(first.state, SeatState::Running);
    assert_eq!(first.pid, Some(1));
    assert_eq!(first.udp_port, Some(7331));
    assert_eq!(first.http_port, Some(7382));
    assert_eq!(first.restarts, 0);

    let spawns = launcher.spawns();
    assert_eq!(spawns.len(), 1);
    let spec = &spawns[0];
    assert_eq!(spec.seat_id, "host-seat-0");
    assert_eq!(spec.program, "ozma-node");
    assert_eq!(spec.args, plan.seats[0].node_argv()[1..].to_vec());
    assert_eq!(spec.env, plan.seats[0].command.env);
    assert_eq!(spec.env.get("DISPLAY").map(String::as_str), Some(":0"));
    assert_eq!(
        spec.env.get("PULSE_SINK").map(String::as_str),
        Some("ozma-seat-0")
    );
    assert_eq!(
        spec.env.get("OZMA_SEAT_ID").map(String::as_str),
        Some("host-seat-0")
    );

    let second = supervisor.start(&plan.seats[1]).expect("start seat 1");
    assert_eq!(second.state, SeatState::Running);
    assert_eq!(second.pid, Some(2));
    assert_eq!(second.udp_port, Some(7332));
    assert_eq!(second.http_port, Some(7383));

    assert_eq!(supervisor.status().len(), 2);
    assert_eq!(launcher.spawn_count(), 2);
    assert!(launcher.active("host-seat-0"));
    assert!(launcher.active("host-seat-1"));

    let error = supervisor.start(&plan.seats[0]).expect_err("duplicate");
    assert!(matches!(
        error,
        MultiseatError::SeatAlreadyRunning(id) if id == "host-seat-0"
    ));
}

#[test]
fn readiness_waits_until_the_probe_succeeds() {
    let plan = two_seat_plan();
    let launcher = FakeNodeLauncher::new();
    let probe = FakeReadinessProbe::ready_after(2);
    let mut supervisor = supervisor(&launcher, probe.clone());

    let status = supervisor.start(&plan.seats[0]).expect("ready");
    assert_eq!(status.state, SeatState::Running);
    assert!(probe.calls() >= 3, "probe calls: {}", probe.calls());
    assert!(launcher.active("host-seat-0"));
}

#[test]
fn readiness_timeout_and_probe_errors_are_failures_not_successes() {
    let plan = two_seat_plan();

    let launcher = FakeNodeLauncher::new();
    let mut timeout_supervisor = ProcessSeatSupervisor::with_parts(
        SupervisorOptions {
            readiness_timeout: Duration::from_millis(5),
            readiness_poll: Duration::from_millis(1),
            ..options()
        },
        Arc::new(launcher.clone()),
        Arc::new(FakeReadinessProbe::not_ready()),
    );
    let error = timeout_supervisor
        .start(&plan.seats[0])
        .expect_err("readiness timeout");
    assert!(error.to_string().contains("not ready"));
    let status = timeout_supervisor.status().pop().expect("status");
    assert_eq!(status.state, SeatState::Failed);
    assert!(status
        .last_error
        .as_deref()
        .is_some_and(|error| error.contains("not ready")));
    assert!(status.next_restart_in_ms.is_some());
    assert!(!launcher.active("host-seat-0"));
    assert_eq!(launcher.stopped(), vec!["host-seat-0".to_owned()]);

    let launcher = FakeNodeLauncher::new();
    let mut error_supervisor = supervisor(&launcher, FakeReadinessProbe::failing("probe exploded"));
    let error = error_supervisor
        .start(&plan.seats[0])
        .expect_err("probe error");
    assert!(error.to_string().contains("probe exploded"));
    let status = error_supervisor.status().pop().expect("status");
    assert_eq!(status.state, SeatState::Failed);
    assert!(status
        .last_error
        .as_deref()
        .is_some_and(|error| error.contains("probe exploded")));
}

#[test]
fn crash_restarts_with_bounded_backoff_and_counts_restarts() {
    let plan = two_seat_plan();
    let launcher = FakeNodeLauncher::new();
    let mut supervisor = supervisor(&launcher, FakeReadinessProbe::new());

    supervisor.start(&plan.seats[0]).expect("start");
    assert_eq!(launcher.spawn_count(), 1);

    launcher.exit("host-seat-0");
    let crashed_at = Instant::now();
    supervisor.tick_at(crashed_at);
    let status = supervisor.status().pop().expect("status");
    assert_eq!(status.state, SeatState::Failed);
    assert_eq!(status.restarts, 0);
    assert!(status
        .last_error
        .as_deref()
        .is_some_and(|error| error.contains("exited")));
    assert!(status.next_restart_in_ms.is_some());
    assert!(!launcher.active("host-seat-0"));

    // Before the backoff expires, no restart is attempted.
    supervisor.tick_at(crashed_at + Duration::from_millis(5));
    assert_eq!(launcher.spawn_count(), 1);

    supervisor.tick_at(crashed_at + Duration::from_millis(15));
    assert_eq!(launcher.spawn_count(), 2);
    let status = supervisor.status().pop().expect("status");
    assert_eq!(status.state, SeatState::Running);
    assert_eq!(status.restarts, 1);
    assert!(launcher.active("host-seat-0"));
}

#[test]
fn spawn_failure_is_reported_and_retried() {
    let plan = two_seat_plan();
    let launcher = FakeNodeLauncher::new();
    launcher.fail_spawn_once("host-seat-0");
    let mut supervisor = supervisor(&launcher, FakeReadinessProbe::new());

    let error = supervisor.start(&plan.seats[0]).expect_err("spawn failed");
    assert!(error.to_string().contains("injected spawn failure"));
    let status = supervisor.status().pop().expect("status");
    assert_eq!(status.state, SeatState::Failed);
    assert!(status
        .last_error
        .as_deref()
        .is_some_and(|error| error.contains("injected spawn failure")));
    assert!(status.next_restart_in_ms.is_some());
    assert_eq!(launcher.spawn_count(), 0);

    let retry_at = Instant::now() + Duration::from_millis(20);
    supervisor.tick_at(retry_at);
    assert_eq!(launcher.spawn_count(), 1);
    let status = supervisor.status().pop().expect("status");
    assert_eq!(status.state, SeatState::Running);
    assert_eq!(status.restarts, 0);
    assert!(launcher.active("host-seat-0"));
}

#[test]
fn stop_reaps_and_remove_forgets_a_seat() {
    let plan = two_seat_plan();
    let launcher = FakeNodeLauncher::new();
    let mut supervisor = supervisor(&launcher, FakeReadinessProbe::new());

    supervisor.start(&plan.seats[0]).expect("start");
    let stopped = supervisor.stop("host-seat-0").expect("stop");
    assert_eq!(stopped.state, SeatState::Stopped);
    assert_eq!(stopped.pid, None);
    assert_eq!(launcher.stopped(), vec!["host-seat-0".to_owned()]);
    assert!(!launcher.active("host-seat-0"));

    // A stopped seat may be started again; it is no longer "already running".
    let restarted = supervisor.start(&plan.seats[0]).expect("restart");
    assert_eq!(restarted.state, SeatState::Running);
    assert_eq!(launcher.spawn_count(), 2);

    let removed = supervisor.remove("host-seat-0").expect("remove");
    assert_eq!(removed.state, SeatState::Stopped);
    assert!(supervisor.status().is_empty());
    assert!(matches!(
        supervisor.stop("host-seat-0").expect_err("unknown"),
        MultiseatError::UnknownSeat(id) if id == "host-seat-0"
    ));
    assert!(matches!(
        supervisor.stop("missing").expect_err("unknown"),
        MultiseatError::UnknownSeat(id) if id == "missing"
    ));
}

#[test]
fn dry_run_records_plans_without_spawning_or_probing() {
    let plan = two_seat_plan();
    let launcher = FakeNodeLauncher::new();
    let probe = FakeReadinessProbe::new();
    let mut supervisor = ProcessSeatSupervisor::with_parts(
        SupervisorOptions {
            dry_run: true,
            ..options()
        },
        Arc::new(launcher.clone()),
        Arc::new(probe.clone()),
    );

    let status = supervisor.start(&plan.seats[0]).expect("dry run");
    assert_eq!(status.state, SeatState::Planned);
    assert_eq!(status.pid, None);
    assert!(status
        .message
        .as_deref()
        .is_some_and(|message| message.contains("dry-run")));
    assert_eq!(launcher.spawn_count(), 0);
    assert_eq!(probe.calls(), 0);

    supervisor.tick_at(Instant::now() + Duration::from_secs(3600));
    assert_eq!(launcher.spawn_count(), 0);
    assert_eq!(supervisor.status().len(), 1);
}

#[test]
fn status_json_and_metrics_are_bounded_and_have_one_node_per_seat() {
    let plan = two_seat_plan();
    let launcher = FakeNodeLauncher::new();
    let mut supervisor = supervisor(&launcher, FakeReadinessProbe::new());
    supervisor.start(&plan.seats[0]).expect("seat 0");
    supervisor.start(&plan.seats[1]).expect("seat 1");

    launcher.exit("host-seat-1");
    supervisor.tick_at(Instant::now());
    let runtime = supervisor.status();
    let status = MultiseatStatus::from_plan_with_runtime(&plan, &runtime);

    assert_eq!(status.capability.seat_count, 2);
    assert_eq!(status.capability.node_count, 2);
    assert_eq!(status.capability.node_count, status.seats.len());
    assert_eq!(status.seats[0].state, SeatState::Running);
    assert_eq!(status.seats[0].pid, Some(1));
    assert_eq!(status.seats[0].udp_port, 7331);
    assert_eq!(status.seats[0].http_port, 7382);
    assert_eq!(status.seats[1].state, SeatState::Failed);
    assert!(status.seats[1].last_error.is_some());

    let value = serde_json::to_value(&status).expect("json");
    assert_eq!(value["capability"]["seat_count"], 2);
    assert_eq!(value["capability"]["node_count"], 2);
    assert_eq!(value["seats"][0]["state"], "running");
    assert_eq!(value["seats"][0]["pid"], 1);
    assert_eq!(value["seats"][0]["udp_port"], 7331);
    assert_eq!(value["seats"][0]["http_port"], 7382);
    assert_eq!(value["seats"][0]["restarts"], 0);
    assert!(value["seats"][1]["last_error"].is_string());
    assert!(value["seats"][1]["next_restart_in_ms"].is_number());

    let metrics = render_metrics(&status);
    assert!(metrics.contains("ozma_multiseat_nodes 2\n"));
    assert!(metrics.contains("ozma_multiseat_seats{state=\"running\"} 1\n"));
    assert!(metrics.contains("ozma_multiseat_seats{state=\"failed\"} 1\n"));
    assert!(metrics.contains("ozma_multiseat_seat_up{seat=\"host-seat-0\"} 1\n"));
    assert!(metrics.contains("ozma_multiseat_seat_up{seat=\"host-seat-1\"} 0\n"));
    assert!(metrics.contains("ozma_multiseat_seat_port{seat=\"host-seat-0\",kind=\"udp\"} 7331\n"));
    assert!(metrics.contains("ozma_multiseat_seat_port{seat=\"host-seat-1\",kind=\"http\"} 7383\n"));
    // Error text and input/argv details stay out of metrics.
    let error = status.seats[1].last_error.as_deref().expect("error");
    assert!(!metrics.contains(error));
    assert!(!metrics.contains("/dev/input"));
    assert!(!metrics.contains("OZMA_SEAT_ID"));
}

#[test]
fn port_allocation_is_sequential_and_reported_by_the_supervisor() {
    let plan = plan_with(
        vec![
            display(0, "HDMI-1", 0),
            display(1, "DP-2", 1920),
            display(2, "DP-3", 3840),
        ],
        7500,
        7600,
    );
    let expected = allocate_ports(3, 7500, 7600).expect("ports");
    assert_eq!(
        expected.iter().map(|ports| ports.udp).collect::<Vec<_>>(),
        vec![7500, 7501, 7502]
    );

    let launcher = FakeNodeLauncher::new();
    let mut supervisor = supervisor(&launcher, FakeReadinessProbe::new());
    for seat in &plan.seats {
        supervisor.start(seat).expect("start");
    }

    let statuses = supervisor.status();
    assert_eq!(statuses.len(), 3);
    for (status, ports) in statuses.iter().zip(expected.iter()) {
        assert_eq!(status.udp_port, Some(ports.udp));
        assert_eq!(status.http_port, Some(ports.http));
    }

    let spawns = launcher.spawns();
    let udp: Vec<u16> = spawns.iter().map(|spec| spec.udp_port).collect();
    let http: Vec<u16> = spawns.iter().map(|spec| spec.http_port).collect();
    assert_eq!(udp, vec![7500, 7501, 7502]);
    assert_eq!(http, vec![7600, 7601, 7602]);
}

#[test]
fn host_topology_refuses_missing_displays_and_backend_failures() {
    let empty = HostTopology::enumerate(
        &FakeDisplayBackend::new(Vec::new()),
        &FakeInputBackend::empty(),
    )
    .expect_err("no displays");
    assert!(matches!(empty, MultiseatError::NoDisplays));

    let display_failure = HostTopology::enumerate(
        &FakeDisplayBackend::new(Vec::new()).failing("xrandr exploded"),
        &FakeInputBackend::empty(),
    )
    .expect_err("display failure");
    assert!(matches!(display_failure, MultiseatError::DisplayBackend(_)));

    let input_failure = HostTopology::enumerate(
        &FakeDisplayBackend::new(vec![display(0, "HDMI-1", 0)]),
        &FakeInputBackend::empty().failing("sysfs exploded"),
    )
    .expect_err("input failure");
    assert!(matches!(input_failure, MultiseatError::InputBackend(_)));

    let topology = HostTopology::enumerate(
        &FakeDisplayBackend::new(vec![display(0, "HDMI-1", 0)]),
        &FakeInputBackend::empty(),
    )
    .expect("empty input is not an enumeration failure");
    let plan = topology
        .plan(&MultiseatPlanner::new(PlannerOptions {
            machine_name: "host".to_owned(),
            ..PlannerOptions::default()
        }))
        .expect("plan");
    assert_eq!(plan.seat_count(), 1);
    assert_eq!(plan.display_backend, "fake-display");
    assert_eq!(plan.input_backend, "fake-input");
    assert!(plan.seats[0].config.input_devices.is_empty());
    assert!(plan
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == DiagnosticCode::NoInputDevices));
}

#[test]
fn restart_delay_doubles_and_caps() {
    let initial = Duration::from_millis(100);
    let max = Duration::from_millis(250);
    assert_eq!(restart_delay(initial, max, 0), Duration::from_millis(100));
    assert_eq!(restart_delay(initial, max, 1), Duration::from_millis(200));
    assert_eq!(restart_delay(initial, max, 2), Duration::from_millis(250));
    assert_eq!(restart_delay(initial, max, 10), max);
    assert_eq!(restart_delay(Duration::ZERO, max, 3), Duration::ZERO);
}

#[test]
fn http_health_probe_checks_a_real_200_and_treats_a_closed_port_as_not_ready() {
    fn spec(port: u16) -> NodeLaunchSpec {
        NodeLaunchSpec {
            seat_id: "host-seat-0".to_owned(),
            node_name: "host-seat-0".to_owned(),
            program: "ozma-node".to_owned(),
            args: Vec::new(),
            env: BTreeMap::new(),
            udp_port: 7331,
            http_port: port,
        }
    }

    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener");
    let port = listener.local_addr().expect("addr").port();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut request = [0u8; 1024];
        let _ = stream.read(&mut request);
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
            .expect("write response");
    });
    let probe = HttpHealthProbe::new("/health").with_timeout(Duration::from_millis(500));
    assert!(probe.probe(&spec(port)).expect("probe"));
    server.join().expect("server");

    // A bound-then-closed port is "not ready", not an error.
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener");
    let closed_port = listener.local_addr().expect("addr").port();
    drop(listener);
    assert!(!probe.probe(&spec(closed_port)).expect("closed port probe"));
}
