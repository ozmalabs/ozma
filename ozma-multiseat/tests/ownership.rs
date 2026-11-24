// SPDX-License-Identifier: AGPL-3.0-only

use ozma_multiseat::{
    apply_ownership, CommandOutput, DisplayOutput, DisplayOwnership, DisplayOwnershipController,
    DisplayOwnershipKind, FakeCommandRunner, FakeDisplayBackend, FakeInputBackend, InputDevice,
    InputDeviceKind, MultiseatError, MultiseatPlanner, PlannerOptions, XrandrOwnershipController,
    OZMA_DISPLAY_OWNERSHIP_ENV, OZMA_INPUT_ROUTER_ENV, OZMA_UINPUT_SEAT_ENV, OZMA_X11_MODE_ENV,
    OZMA_X11_OUTPUT_ENV,
};

fn ownership(seat: &str, output: &str, x: i32) -> DisplayOwnership {
    DisplayOwnership {
        seat_id: seat.to_owned(),
        kind: DisplayOwnershipKind::XrandrOutput,
        x_screen: ":0".to_owned(),
        output: output.to_owned(),
        mode: "1920x1080".to_owned(),
        width: 1920,
        height: 1080,
        x,
        y: 0,
        primary: false,
        virtual_display: false,
    }
}

#[test]
fn xrandr_ownership_apply_and_release_use_exact_argv() {
    let runner = FakeCommandRunner::new()
        .respond(
            "xrandr",
            &[
                "--output",
                "HDMI-1",
                "--mode",
                "1920x1080",
                "--pos",
                "1920x0",
            ],
            CommandOutput::success(),
        )
        .respond(
            "xrandr",
            &["--output", "HDMI-1", "--auto"],
            CommandOutput::success(),
        );
    let mut controller = XrandrOwnershipController::new(Box::new(runner.clone()));

    controller
        .apply(&ownership("host-seat-0", "HDMI-1", 1920))
        .expect("apply");
    assert_eq!(controller.status().len(), 1);
    assert_eq!(
        runner.commands()[0].env.get("DISPLAY").map(String::as_str),
        Some(":0")
    );

    controller.release("host-seat-0").expect("release");
    assert!(controller.status().is_empty());
    assert_eq!(
        runner.last_command().expect("release").argv(),
        vec!["xrandr", "--output", "HDMI-1", "--auto"]
    );
}

#[test]
fn absent_xrandr_is_a_typed_error_and_nothing_is_recorded() {
    let runner = FakeCommandRunner::new().with_unavailable("xrandr");
    let mut controller = XrandrOwnershipController::new(Box::new(runner));

    let error = controller
        .apply(&ownership("host-seat-0", "HDMI-1", 0))
        .expect_err("missing xrandr");
    assert!(matches!(
        error,
        MultiseatError::CommandUnavailable { program, .. } if program == "xrandr"
    ));
    assert!(controller.status().is_empty());
}

#[test]
fn non_xrandr_ownership_kind_is_unsupported() {
    let runner = FakeCommandRunner::new();
    let mut controller = XrandrOwnershipController::new(Box::new(runner));
    let mut ownership = ownership("host-seat-0", "HDMI-1", 0);
    ownership.kind = DisplayOwnershipKind::XScreen;

    let error = controller.apply(&ownership).expect_err("unsupported kind");
    assert!(matches!(error, MultiseatError::Unsupported(_)));
}

#[test]
fn ownership_apply_rolls_back_earlier_seats_on_failure() {
    let runner = FakeCommandRunner::new()
        .respond(
            "xrandr",
            &["--output", "HDMI-1", "--mode", "1920x1080", "--pos", "0x0"],
            CommandOutput::success(),
        )
        .respond_failure(
            "xrandr",
            &["--output", "DP-2", "--mode", "1920x1080", "--pos", "1920x0"],
            1,
            "cannot set mode",
        )
        .respond(
            "xrandr",
            &["--output", "HDMI-1", "--auto"],
            CommandOutput::success(),
        );
    let mut controller = XrandrOwnershipController::new(Box::new(runner.clone()));

    let ownerships = vec![
        ownership("host-seat-0", "HDMI-1", 0),
        ownership("host-seat-1", "DP-2", 1920),
    ];
    let error = apply_ownership(&mut controller, &ownerships).expect_err("second apply fails");
    assert!(matches!(error, MultiseatError::CommandFailed { .. }));
    assert!(controller.status().is_empty());
    assert_eq!(
        runner.last_command().expect("rollback").argv(),
        vec!["xrandr", "--output", "HDMI-1", "--auto"]
    );
}

#[test]
fn duplicate_outputs_are_rejected_before_any_mode_change() {
    let runner = FakeCommandRunner::new();
    let mut controller = XrandrOwnershipController::new(Box::new(runner.clone()));
    let ownerships = vec![
        ownership("host-seat-0", "HDMI-1", 0),
        ownership("host-seat-1", "HDMI-1", 1920),
    ];
    let error = apply_ownership(&mut controller, &ownerships).expect_err("duplicate output");
    assert!(matches!(error, MultiseatError::DisplayOwnership(_)));
    assert_eq!(runner.run_count(), 0);
}

fn input(path: &str, kind: InputDeviceKind, hub: &str) -> InputDevice {
    InputDevice {
        path: path.to_owned(),
        name: path.to_owned(),
        kind,
        usb_hub: Some(hub.to_owned()),
        sysfs_path: None,
    }
}

#[test]
fn planner_wires_ownership_and_input_routing_into_the_seat_and_env() {
    let displays = vec![
        DisplayOutput::new(0, "HDMI-1", 1920, 1080, 0, 0, ":0"),
        DisplayOutput::new(1, "DP-2", 1920, 1080, 1920, 0, ":0"),
    ];
    let inputs = vec![
        input("/dev/input/event0", InputDeviceKind::Keyboard, "1-1"),
        input("/dev/input/event1", InputDeviceKind::Mouse, "1-1"),
        input("/dev/input/event2", InputDeviceKind::Keyboard, "2-1"),
        input("/dev/input/event3", InputDeviceKind::Mouse, "2-1"),
    ];
    let plan = MultiseatPlanner::new(PlannerOptions {
        machine_name: "host".to_owned(),
        ..PlannerOptions::default()
    })
    .plan_with_backends(
        &FakeDisplayBackend::new(displays),
        &FakeInputBackend::new(inputs),
    )
    .expect("plan");

    let seat = &plan.seats[0];
    let ownership = seat.ownership.as_ref().expect("ownership");
    assert_eq!(ownership.seat_id, "host-seat-0");
    assert_eq!(ownership.output, "HDMI-1");
    assert_eq!(ownership.mode, "1920x1080");
    assert_eq!(
        ownership.xrandr_args(),
        vec!["--output", "HDMI-1", "--mode", "1920x1080", "--pos", "0x0"]
    );

    let route = seat.input_route.as_ref().expect("input route");
    assert_eq!(route.hub_path, "1-1");
    assert_eq!(route.display_output, "HDMI-1");
    assert_eq!(route.pointer_devices, vec!["/dev/input/event1"]);
    assert!(!route.grab_exclusive);
    assert_eq!(
        route.exclusive_devices,
        vec!["/dev/input/event0", "/dev/input/event1"]
    );

    assert_eq!(
        seat.command
            .env
            .get(OZMA_X11_OUTPUT_ENV)
            .map(String::as_str),
        Some("HDMI-1")
    );
    assert_eq!(
        seat.command.env.get(OZMA_X11_MODE_ENV).map(String::as_str),
        Some("1920x1080")
    );
    assert_eq!(
        seat.command
            .env
            .get(OZMA_DISPLAY_OWNERSHIP_ENV)
            .map(String::as_str),
        Some("xrandr-output")
    );
    assert_eq!(
        seat.command
            .env
            .get(OZMA_INPUT_ROUTER_ENV)
            .map(String::as_str),
        Some("xinput-map-to-output")
    );
    assert_eq!(
        seat.command
            .env
            .get(OZMA_UINPUT_SEAT_ENV)
            .map(String::as_str),
        Some("host-seat-0")
    );
}
