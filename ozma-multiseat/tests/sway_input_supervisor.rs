// SPDX-License-Identifier: AGPL-3.0-only
//!
//! Fake-backed per-seat sway synthetic-input supervisor tests.
//!
//! No compositor, node, socket, or device is touched. `FakeSeatDisplayLauncher`
//! produces the same private Wayland environment as a real sway spawn, and
//! `FakeSeatInputInjector` records events by seat.

use std::sync::Arc;
use std::time::Duration;

use ozma_multiseat::{
    DisplayOutput, FakeDisplayBackend, FakeInputBackend, FakeNodeLauncher, FakeReadinessProbe,
    FakeSeatDisplayLauncher, FakeSeatInputInjector, HostTopology, InputDevice, InputDeviceKind,
    MultiseatPlan, MultiseatPlanner, PlannerOptions, ProcessSeatSupervisor, ResourceKind,
    SeatDisplayBackend, SeatDisplayNoteKind, SeatDisplayOptions, SeatDisplaySize, SeatInputError,
    SeatInputInjector, SeatInputMode, SeatInputRequest, SeatKeyState, SeatResourceStatus,
    SeatState, SeatSupervisor, SupervisorOptions,
};

const SIZE: SeatDisplaySize = SeatDisplaySize {
    width: 1280,
    height: 800,
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
    HostTopology::enumerate(
        &FakeDisplayBackend::new(vec![display(0, "HDMI-1", 0), display(1, "DP-2", 1920)]),
        &FakeInputBackend::new(inputs),
    )
    .expect("topology")
    .plan(&MultiseatPlanner::new(PlannerOptions {
        machine_name: "host".to_owned(),
        x11_display: Some(":0".to_owned()),
        ..PlannerOptions::default()
    }))
    .expect("plan")
}

fn display_options(backend: SeatDisplayBackend) -> SeatDisplayOptions {
    SeatDisplayOptions {
        backend,
        size: SIZE,
        number_start: 90,
        number_end: 99,
        ready_timeout: Duration::from_millis(40),
        ready_poll: Duration::from_millis(1),
        parent_display: None,
        xorg_config: None,
    }
}

fn options(backend: SeatDisplayBackend, input: SeatInputMode) -> SupervisorOptions {
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
        seat_display: display_options(backend),
        seat_input: input,
        drm_leases: Default::default(),
    }
}

fn supervisor(
    options: SupervisorOptions,
    displays: &FakeSeatDisplayLauncher,
    injector: &FakeSeatInputInjector,
) -> ProcessSeatSupervisor {
    ProcessSeatSupervisor::with_parts(
        options,
        Arc::new(FakeNodeLauncher::new()),
        Arc::new(FakeReadinessProbe::always_ready()),
    )
    .with_display_launcher(Box::new(displays.clone()))
    .with_seat_input_injector(Box::new(injector.clone()))
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

fn has_note(status: &ozma_multiseat::SeatRuntimeStatus, kind: SeatDisplayNoteKind) -> bool {
    status
        .display
        .display_notes
        .iter()
        .any(|note| note.kind == kind)
}

#[test]
fn two_sway_seats_get_only_their_own_injector_socket() {
    let plan = plan();
    let displays = FakeSeatDisplayLauncher::new();
    let injector = FakeSeatInputInjector::new();
    let mut supervisor = supervisor(
        options(SeatDisplayBackend::Sway, SeatInputMode::Wayland),
        &displays,
        &injector,
    );

    let first = supervisor.start(&plan.seats[0]).expect("seat 0 start");
    let second = supervisor.start(&plan.seats[1]).expect("seat 1 start");
    assert_eq!(first.state, SeatState::Running);
    assert_eq!(second.state, SeatState::Running);
    assert_eq!(first.display.display_backend, SeatDisplayBackend::Sway);
    assert_eq!(first.display.display.as_deref(), Some("ozma-sway-90"));
    assert_eq!(second.display.display.as_deref(), Some("ozma-sway-91"));
    assert!(first.resources.input_injection_available);
    assert!(first.resources.input_injection_applied);
    assert!(second.resources.input_injection_available);
    assert!(second.resources.input_injection_applied);

    let requests = injector.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].seat_id, "host-seat-0");
    assert_eq!(requests[0].wayland.wayland_display, "ozma-sway-90");
    assert!(requests[0]
        .wayland
        .socket
        .to_string_lossy()
        .contains("ozma-fake-sway"));
    assert_eq!(requests[1].seat_id, "host-seat-1");
    assert_eq!(requests[1].wayland.wayland_display, "ozma-sway-91");
    assert_ne!(
        requests[0].wayland.xdg_runtime_dir,
        requests[1].wayland.xdg_runtime_dir
    );

    let stopped = supervisor.stop("host-seat-0").expect("stop");
    assert_eq!(stopped.state, SeatState::Stopped);
    assert!(!stopped.resources.input_injection_applied);
    assert!(!stopped.resources.input_injection_available);
    assert!(stopped.resources.input_injection_released);
    assert_eq!(injector.disconnects(), vec!["host-seat-0".to_owned()]);

    let status = status_for(&supervisor, "host-seat-1");
    assert!(status.resources.input_injection_applied);
    assert!(!injector.disconnects().contains(&"host-seat-1".to_owned()));
}

#[test]
fn missing_protocol_is_typed_non_fatal_and_never_available() {
    let plan = plan();
    let displays = FakeSeatDisplayLauncher::new();
    let injector = FakeSeatInputInjector::new();
    injector.fail_connect(
        "host-seat-0",
        SeatInputError::ProtocolUnavailable {
            display: "ozma-sway-90".to_owned(),
            protocol: "zwlr_virtual_pointer_manager_v1".to_owned(),
        },
    );
    let mut supervisor = supervisor(
        options(SeatDisplayBackend::Sway, SeatInputMode::Wayland),
        &displays,
        &injector,
    );

    let status = supervisor.start(&plan.seats[0]).expect("start");
    assert_eq!(status.state, SeatState::Running);
    assert!(!status.resources.input_injection_available);
    assert!(!status.resources.input_injection_applied);
    assert!(has_note(
        &status,
        SeatDisplayNoteKind::InputInjectionUnavailable
    ));
    assert!(status.resources.errors.iter().any(|error| {
        error.kind == ResourceKind::SeatInput && error.message.contains("protocol_unavailable")
    }));

    let value = serde_json::to_value(&status).expect("status json");
    assert_eq!(value["resources"]["input_injection_available"], false);
    assert_eq!(value["resources"]["input_injection_applied"], false);
}

#[test]
fn shared_display_never_claims_wayland_input() {
    let plan = plan();
    let displays = FakeSeatDisplayLauncher::new();
    let injector = FakeSeatInputInjector::new();
    let mut supervisor = supervisor(
        options(SeatDisplayBackend::Shared, SeatInputMode::Wayland),
        &displays,
        &injector,
    );

    let status = supervisor.start(&plan.seats[0]).expect("start");
    assert_eq!(status.state, SeatState::Running);
    assert!(!status.resources.input_injection_applied);
    assert!(!status.resources.input_injection_available);
    assert!(has_note(
        &status,
        SeatDisplayNoteKind::InputInjectionUnavailable
    ));
    assert!(injector.requests().is_empty());
}

#[test]
fn fake_injector_keeps_events_per_seat() {
    let plan = plan();
    let mut injector = FakeSeatInputInjector::new();
    let first = SeatInputRequest {
        seat_id: plan.seats[0].config.id.clone(),
        wayland: ozma_multiseat::SeatWaylandEnv::new("ozma-sway-90", "/tmp/seat-a"),
    };
    let second = SeatInputRequest {
        seat_id: plan.seats[1].config.id.clone(),
        wayland: ozma_multiseat::SeatWaylandEnv::new("ozma-sway-91", "/tmp/seat-b"),
    };
    let caps = injector.connect(&first).expect("seat 0 connect");
    assert!(caps.is_complete());
    injector.connect(&second).expect("seat 1 connect");

    injector.inject_text(&first.seat_id, "a").expect("type a");
    injector.inject_text(&second.seat_id, "b").expect("type b");
    injector
        .pointer_motion(&first.seat_id, 10.0, -5.0)
        .expect("motion");
    injector
        .pointer_button(
            &first.seat_id,
            ozma_multiseat::BTN_LEFT,
            SeatKeyState::Pressed,
        )
        .expect("button");

    assert_eq!(injector.text(&first.seat_id), "a");
    assert_eq!(injector.text(&second.seat_id), "b");
    assert_eq!(injector.keys(&first.seat_id)[0].keycode, 38);
    assert_eq!(injector.keys(&second.seat_id)[0].keycode, 56);
    assert!(!injector
        .keys(&first.seat_id)
        .iter()
        .any(|key| key.keycode == 56));
    assert!(!injector
        .keys(&second.seat_id)
        .iter()
        .any(|key| key.keycode == 38));
    assert_eq!(injector.motions(&first.seat_id), vec![(10.0, -5.0)]);
    assert!(injector.motions(&second.seat_id).is_empty());
    assert_eq!(
        injector.buttons(&first.seat_id),
        vec![(ozma_multiseat::BTN_LEFT, SeatKeyState::Pressed)]
    );
    assert!(injector.buttons(&second.seat_id).is_empty());
    assert_eq!(injector.connected_seats().len(), 2);
}

#[test]
fn default_options_leave_synthetic_input_off() {
    let options = SupervisorOptions::default();
    assert_eq!(options.seat_input, SeatInputMode::Off);
    assert!(!options.seat_input.is_wayland());
    assert_eq!(SeatResourceStatus::default().input_injection_events, 0);
}
