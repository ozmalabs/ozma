// SPDX-License-Identifier: AGPL-3.0-only

use std::collections::BTreeMap;

use ozma_multiseat::{
    allocate_ports, CapturePreference, DiagnosticCode, DisplayOutput, FakeDisplayBackend,
    FakeInputBackend, InputDevice, InputDeviceKind, MultiseatError, MultiseatPlanner,
    PlannerOptions, SeatProfile,
};

fn display(index: u32, name: &str, x: i32) -> DisplayOutput {
    DisplayOutput::new(index, name, 1920, 1080, x, 0, ":0")
}

fn device(path: &str, kind: InputDeviceKind, hub: Option<&str>) -> InputDevice {
    InputDevice {
        path: path.to_owned(),
        name: path.to_owned(),
        kind,
        usb_hub: hub.map(str::to_owned),
        sysfs_path: None,
    }
}

fn planner() -> MultiseatPlanner {
    MultiseatPlanner::new(PlannerOptions {
        machine_name: "host".to_owned(),
        ..PlannerOptions::default()
    })
}

fn full_group(hub: &str, base: u32) -> Vec<InputDevice> {
    vec![
        device(
            &format!("/dev/input/event{}", base),
            InputDeviceKind::Keyboard,
            Some(hub),
        ),
        device(
            &format!("/dev/input/event{}", base + 1),
            InputDeviceKind::Mouse,
            Some(hub),
        ),
    ]
}

#[test]
fn plans_one_seat_per_display_with_sequential_ports_and_exact_argv() {
    let displays = vec![display(0, "HDMI-1", 0), display(1, "DP-2", 1920)];
    let mut inputs = full_group("1-1", 0);
    inputs.extend(full_group("2-1", 2));

    let plan = planner().plan(&displays, &inputs).expect("plan");
    assert_eq!(plan.requested_seats, 2);
    assert_eq!(plan.seat_count(), 2);
    assert_eq!(plan.node_count(), 2);
    assert_eq!(plan.capture_providers(), vec!["x11"]);
    assert!(plan.diagnostics.is_empty());

    assert_eq!(plan.seats[0].config.id, "host-seat-0");
    assert_eq!(plan.seats[0].config.udp_port, 7331);
    assert_eq!(plan.seats[0].config.http_port, 7382);
    assert_eq!(plan.seats[0].config.audio_sink, "ozma-seat-0");
    assert_eq!(
        plan.seats[0].config.input_devices,
        vec!["/dev/input/event0", "/dev/input/event1"]
    );
    assert_eq!(plan.seats[0].config.input_hub.as_deref(), Some("1-1"));
    assert_eq!(plan.seats[1].config.udp_port, 7332);
    assert_eq!(plan.seats[1].config.http_port, 7383);
    assert_eq!(plan.seats[1].config.audio_sink, "ozma-seat-1");
    assert_eq!(
        plan.seats[1].config.input_devices,
        vec!["/dev/input/event2", "/dev/input/event3"]
    );

    assert_eq!(
        plan.seats[0].node_argv(),
        vec![
            "ozma-node",
            "--profile",
            "desktop",
            "--name",
            "host-seat-0",
            "--hid-udp-port",
            "7331",
            "--http-port",
            "7382",
            "--input-provider",
            "uinput",
            "--capture-provider",
            "x11",
            "--x11-display",
            ":0",
            "--x11-offset",
            "0,0",
            "--x11-width",
            "1920",
            "--x11-height",
            "1080",
            "--x11-fps",
            "30",
            "--audio-sink",
            "ozma-seat-0",
            "--audio-capture-target",
            "monitor:ozma-seat-0",
        ]
    );
    assert_eq!(
        plan.seats[0].command.env.get("DISPLAY").map(String::as_str),
        Some(":0")
    );
    assert_eq!(
        plan.seats[0]
            .command
            .env
            .get("PULSE_SINK")
            .map(String::as_str),
        Some("ozma-seat-0")
    );
    assert_eq!(
        plan.seats[0]
            .command
            .env
            .get("OZMA_SEAT_ID")
            .map(String::as_str),
        Some("host-seat-0")
    );
}

#[test]
fn planning_is_deterministic() {
    let displays = vec![display(1, "DP-2", 1920), display(0, "HDMI-1", 0)];
    let inputs = full_group("1-1", 0);
    let first = planner().plan(&displays, &inputs).expect("first");
    let second = planner().plan(&displays, &inputs).expect("second");
    assert_eq!(first, second);
    assert_eq!(first.seats[0].config.display_output, "HDMI-1");
    assert_eq!(first.seats[1].config.display_output, "DP-2");
}

#[test]
fn uses_configured_x11_display_when_output_has_no_screen() {
    let mut output = display(0, "HDMI-1", 0);
    output.x_screen.clear();

    let error = planner()
        .plan(&[output.clone()], &[])
        .expect_err("missing X display");
    assert!(matches!(error, MultiseatError::MissingX11Display(name) if name == "HDMI-1"));

    let planner = MultiseatPlanner::new(PlannerOptions {
        machine_name: "host".to_owned(),
        x11_display: Some(":1".to_owned()),
        ..PlannerOptions::default()
    });
    let plan = planner.plan(&[output], &[]).expect("fallback display");
    if let ozma_multiseat::CapturePlan::X11 { display, .. } = &plan.seats[0].config.capture {
        assert_eq!(display, ":1");
    } else {
        panic!("expected an X11 capture plan");
    }
    assert_eq!(
        plan.seats[0].command.env.get("DISPLAY").map(String::as_str),
        Some(":1")
    );
}

#[test]
fn pipewire_capture_requires_an_explicit_target() {
    let displays = vec![display(0, "HDMI-1", 0), display(1, "DP-2", 1920)];
    let mut targets = BTreeMap::new();
    targets.insert("DP-2".to_owned(), "node:42".to_owned());
    let planner = MultiseatPlanner::new(PlannerOptions {
        machine_name: "host".to_owned(),
        capture: CapturePreference::PipeWire { targets },
        ..PlannerOptions::default()
    });

    let error = planner.plan(&displays, &[]).expect_err("missing target");
    assert!(matches!(error, MultiseatError::MissingPipeWireTarget(name) if name == "HDMI-1"));
}

#[test]
fn pipewire_targets_become_desktop_capture_argv() {
    let displays = vec![display(0, "HDMI-1", 0)];
    let mut targets = BTreeMap::new();
    targets.insert("HDMI-1".to_owned(), "name:ozma-screen-0".to_owned());
    let planner = MultiseatPlanner::new(PlannerOptions {
        machine_name: "host".to_owned(),
        profile: SeatProfile::Gaming,
        capture: CapturePreference::PipeWire { targets },
        ..PlannerOptions::default()
    });

    let plan = planner.plan(&displays, &[]).expect("plan");
    let argv = plan.seats[0].node_argv();
    assert!(argv
        .windows(2)
        .any(|pair| pair == ["--capture-provider", "pipewire"]));
    assert!(argv
        .windows(2)
        .any(|pair| pair == ["--desktop-capture-target", "name:ozma-screen-0"]));
    assert!(argv
        .windows(2)
        .any(|pair| pair == ["--desktop-capture-fps", "60"]));
    assert!(argv
        .windows(2)
        .any(|pair| pair == ["--desktop-capture-width", "1920"]));
    assert_eq!(plan.seats[0].config.profile, SeatProfile::Gaming);
    assert!(!plan.seats[0].command.env.contains_key("DISPLAY"));
}

#[test]
fn no_displays_is_an_error_not_a_fabricated_seat() {
    let error = planner().plan(&[], &[]).expect_err("no displays");
    assert!(matches!(error, MultiseatError::NoDisplays));

    let planner = MultiseatPlanner::new(PlannerOptions {
        seat_count: Some(0),
        ..PlannerOptions::default()
    });
    let error = planner.plan(&[], &[]).expect_err("zero seats");
    assert!(matches!(error, MultiseatError::ZeroSeats));
}

#[test]
fn requested_seats_are_capped_when_virtual_displays_are_unavailable() {
    let displays = vec![display(0, "HDMI-1", 0), display(1, "DP-2", 1920)];
    let planner = MultiseatPlanner::new(PlannerOptions {
        machine_name: "host".to_owned(),
        seat_count: Some(4),
        ..PlannerOptions::default()
    });

    let plan = planner.plan(&displays, &[]).expect("capped plan");
    assert_eq!(plan.requested_seats, 4);
    assert_eq!(plan.seat_count(), 2);
    assert!(plan
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == DiagnosticCode::RequestedSeatsCapped));
}

#[test]
fn virtual_displays_fill_requested_seats_through_the_fake_backend() {
    let displays = vec![display(0, "HDMI-1", 0), display(1, "DP-2", 1920)];
    let display_backend = FakeDisplayBackend::new(displays).with_virtual_displays();
    let input_backend = FakeInputBackend::empty();
    let planner = MultiseatPlanner::new(PlannerOptions {
        machine_name: "host".to_owned(),
        seat_count: Some(4),
        ..PlannerOptions::default()
    });

    let plan = planner
        .plan_with_backends(&display_backend, &input_backend)
        .expect("plan with virtual displays");
    assert_eq!(plan.seat_count(), 4);
    assert_eq!(plan.display_backend, "fake-display");
    assert_eq!(plan.input_backend, "fake-input");
    assert_eq!(plan.seats[0].config.udp_port, 7331);
    assert_eq!(plan.seats[3].config.udp_port, 7334);
    assert_eq!(plan.seats[3].config.http_port, 7385);
    assert!(plan
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == DiagnosticCode::VirtualDisplayCreated));
    assert_eq!(display_backend.created_virtual().len(), 2);

    let mut displays: Vec<String> = plan
        .seats
        .iter()
        .map(|seat| seat.config.display_output.clone())
        .collect();
    displays.sort();
    displays.dedup();
    assert_eq!(displays.len(), 4);
}

#[test]
fn missing_input_still_plans_seats_with_diagnostics() {
    let displays = vec![
        display(0, "HDMI-1", 0),
        display(1, "DP-2", 1920),
        display(2, "DP-3", 3840),
    ];
    let inputs = full_group("1-1", 0);

    let plan = planner().plan(&displays, &inputs).expect("plan");
    assert_eq!(plan.seat_count(), 3);
    assert_eq!(plan.seats[0].config.input_devices.len(), 2);
    assert!(plan.seats[1].input.is_none());
    assert!(plan.seats[2].input.is_none());
    assert_eq!(
        plan.diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.code == DiagnosticCode::SeatWithoutInput)
            .count(),
        2
    );

    let plan = planner().plan(&displays, &[]).expect("no input plan");
    assert_eq!(plan.seat_count(), 3);
    assert!(plan
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == DiagnosticCode::NoInputDevices));
    assert!(plan
        .seats
        .iter()
        .all(|seat| seat.config.input_devices.is_empty()));
}

#[test]
fn internal_group_takes_the_first_seat() {
    let displays = vec![display(0, "HDMI-1", 0), display(1, "DP-2", 1920)];
    let inputs = vec![
        device("/dev/input/event9", InputDeviceKind::Keyboard, None),
        device("/dev/input/event2", InputDeviceKind::Mouse, Some("1-1")),
    ];

    let plan = planner().plan(&displays, &inputs).expect("plan");
    assert_eq!(
        plan.seats[0].config.input_devices,
        vec!["/dev/input/event9"]
    );
    assert_eq!(plan.seats[0].config.input_hub.as_deref(), Some("internal"));
}

#[test]
fn gamepad_only_group_attaches_to_the_next_seat() {
    let displays = vec![display(0, "HDMI-1", 0), display(1, "DP-2", 1920)];
    let mut inputs = full_group("1-1", 0);
    inputs.push(device(
        "/dev/input/event7",
        InputDeviceKind::Gamepad,
        Some("3-1"),
    ));

    let plan = planner().plan(&displays, &inputs).expect("plan");
    assert_eq!(plan.seats[0].config.input_devices.len(), 3);
    assert!(plan.seats[0]
        .config
        .input_devices
        .contains(&"/dev/input/event7".to_owned()));
}

#[test]
fn extra_input_groups_are_reported_as_unassigned() {
    let displays = vec![display(0, "HDMI-1", 0)];
    let mut inputs = full_group("1-1", 0);
    inputs.extend(full_group("2-1", 2));

    let plan = planner().plan(&displays, &inputs).expect("plan");
    assert_eq!(plan.seat_count(), 1);
    assert!(plan
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == DiagnosticCode::UnassignedInputGroup));
}

#[test]
fn duplicate_display_identity_is_rejected() {
    let duplicate_name = vec![display(0, "HDMI-1", 0), display(1, "HDMI-1", 1920)];
    let error = planner().plan(&duplicate_name, &[]).expect_err("name");
    assert!(matches!(error, MultiseatError::DuplicateDisplay { .. }));

    let duplicate_index = vec![display(0, "HDMI-1", 0), display(0, "DP-2", 1920)];
    let error = planner().plan(&duplicate_index, &[]).expect_err("index");
    assert!(matches!(error, MultiseatError::DuplicateDisplay { .. }));
}

#[test]
fn invalid_display_geometry_is_rejected() {
    let output = DisplayOutput::new(0, "HDMI-1", 0, 1080, 0, 0, ":0");
    let error = planner().plan(&[output], &[]).expect_err("geometry");
    assert!(matches!(error, MultiseatError::InvalidDisplay { .. }));
}

#[test]
fn backend_failures_are_propagated() {
    let planner = planner();

    let display_error = planner
        .plan_with_backends(
            &FakeDisplayBackend::new(Vec::new()).failing("xrandr exploded"),
            &FakeInputBackend::empty(),
        )
        .expect_err("display failure");
    assert!(matches!(display_error, MultiseatError::DisplayBackend(_)));

    let input_error = planner
        .plan_with_backends(
            &FakeDisplayBackend::new(vec![display(0, "HDMI-1", 0)]),
            &FakeInputBackend::empty().failing("sysfs exploded"),
        )
        .expect_err("input failure");
    assert!(matches!(input_error, MultiseatError::InputBackend(_)));
}

#[test]
fn ports_are_sequential_and_overflow_is_typed() {
    let ports = allocate_ports(3, 7331, 7382).expect("ports");
    assert_eq!(
        ports,
        vec![
            ozma_multiseat::SeatPorts {
                udp: 7331,
                http: 7382
            },
            ozma_multiseat::SeatPorts {
                udp: 7332,
                http: 7383
            },
            ozma_multiseat::SeatPorts {
                udp: 7333,
                http: 7384
            },
        ]
    );
    assert!(allocate_ports(0, 7331, 7382).expect("empty").is_empty());
    assert!(matches!(
        allocate_ports(2, u16::MAX, 7382).expect_err("udp overflow"),
        MultiseatError::PortOverflow { .. }
    ));
    assert!(matches!(
        allocate_ports(2, 7331, u16::MAX).expect_err("http overflow"),
        MultiseatError::PortOverflow { .. }
    ));
}
