// SPDX-License-Identifier: AGPL-3.0-only

use ozma_multiseat::{
    assign_seats, CommandOutput, EvdevGrabRouter, FakeCommandRunner, FakeDeviceNameResolver,
    FakeEvdevGrabBackend, FakeGrabAction, InputGroup, InputRouter, InputTarget, MultiseatError,
    SeatInputAssignment, XInputRouter, OZMA_INPUT_ROUTER_ENV, OZMA_UINPUT_SEAT_ENV,
    XINPUT_ROUTER_NAME,
};

const XINPUT_LIST: &str = "\
⎡ Virtual core pointer                     id=2    [master pointer  (3)]
⎜   ↳ USB Optical Mouse                   id=12   [slave  pointer  (2)]
⎜   ↳ Secondary Mouse                     id=13   [slave  pointer  (2)]
⎜   ↳ AT Translated Set 2 keyboard         id=14   [slave  keyboard (3)]
";

fn group(hub: &str, mice: &[&str], keyboards: &[&str]) -> InputGroup {
    InputGroup {
        hub_path: hub.to_owned(),
        keyboards: keyboards.iter().map(|path| (*path).to_owned()).collect(),
        mice: mice.iter().map(|path| (*path).to_owned()).collect(),
        gamepads: Vec::new(),
        other: Vec::new(),
    }
}

fn target(seat: &str, output: &str) -> InputTarget {
    InputTarget::new(seat, output, ":0").expect("target")
}

#[test]
fn xinput_assign_parses_list_and_maps_pointer_with_exact_argv() {
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
        );
    let resolver = FakeDeviceNameResolver::new()
        .with_name("/dev/input/event0", "AT Translated Set 2 keyboard")
        .with_name("/dev/input/event1", "USB Optical Mouse");

    let mut router = XInputRouter::with_resolver(Box::new(runner.clone()), Box::new(resolver));
    let assignment = router
        .assign(
            &group("1-1", &["/dev/input/event1"], &["/dev/input/event0"]),
            &target("host-seat-0", "HDMI-1"),
        )
        .expect("assign");

    assert_eq!(assignment.seat_id, "host-seat-0");
    assert_eq!(assignment.router, XINPUT_ROUTER_NAME);
    assert_eq!(assignment.xinput.len(), 1);
    assert_eq!(assignment.xinput[0].device_name, "USB Optical Mouse");
    assert_eq!(assignment.xinput[0].xinput_id, 12);
    assert_eq!(assignment.xinput[0].output, "HDMI-1");
    assert_eq!(
        assignment.env.get("DISPLAY").map(String::as_str),
        Some(":0")
    );
    assert_eq!(
        assignment.env.get(OZMA_UINPUT_SEAT_ENV).map(String::as_str),
        Some("host-seat-0")
    );
    assert_eq!(
        assignment
            .env
            .get(OZMA_INPUT_ROUTER_ENV)
            .map(String::as_str),
        Some(XINPUT_ROUTER_NAME)
    );

    let commands = runner.commands();
    assert_eq!(commands.len(), 2);
    assert_eq!(commands[0].argv(), vec!["xinput", "list", "--short"]);
    assert_eq!(
        commands[0].env.get("DISPLAY").map(String::as_str),
        Some(":0")
    );
    assert_eq!(
        commands[1].argv(),
        vec!["xinput", "map-to-output", "12", "HDMI-1"]
    );
    assert_eq!(router.status().len(), 1);

    let release = router.release("host-seat-0").expect("release");
    assert_eq!(release.released_devices, vec!["/dev/input/event1"]);
    assert!(router.status().is_empty());
    assert_eq!(
        runner.last_command().expect("reset command").argv(),
        vec!["xinput", "map-to-output", "12", "all"]
    );
}

#[test]
fn partial_map_failure_rolls_back_the_first_pointer() {
    let runner = FakeCommandRunner::new()
        .respond_stdout("xinput", &["list", "--short"], XINPUT_LIST)
        .respond(
            "xinput",
            &["map-to-output", "12", "HDMI-1"],
            CommandOutput::success(),
        )
        .respond_failure(
            "xinput",
            &["map-to-output", "13", "HDMI-1"],
            1,
            "no such output",
        )
        .respond(
            "xinput",
            &["map-to-output", "12", "all"],
            CommandOutput::success(),
        );
    let resolver = FakeDeviceNameResolver::new()
        .with_name("/dev/input/event1", "USB Optical Mouse")
        .with_name("/dev/input/event2", "Secondary Mouse");

    let mut router = XInputRouter::with_resolver(Box::new(runner.clone()), Box::new(resolver));
    let error = router
        .assign(
            &group("1-1", &["/dev/input/event1", "/dev/input/event2"], &[]),
            &target("host-seat-0", "HDMI-1"),
        )
        .expect_err("second map fails");

    assert!(matches!(
        error,
        MultiseatError::CommandFailed {
            ref program,
            status: 1,
            ..
        } if program == "xinput"
    ));
    assert!(router.status().is_empty());
    let commands = runner.commands();
    assert_eq!(
        commands.last().expect("rollback command").argv(),
        vec!["xinput", "map-to-output", "12", "all"]
    );
}

#[test]
fn absent_xinput_is_a_typed_error_not_a_mapping() {
    let runner = FakeCommandRunner::new().with_unavailable("xinput");
    let resolver =
        FakeDeviceNameResolver::new().with_name("/dev/input/event1", "USB Optical Mouse");
    let mut router = XInputRouter::with_resolver(Box::new(runner), Box::new(resolver));

    let error = router
        .assign(
            &group("1-1", &["/dev/input/event1"], &[]),
            &target("host-seat-0", "HDMI-1"),
        )
        .expect_err("missing tool");
    assert!(matches!(
        error,
        MultiseatError::CommandUnavailable { program, .. } if program == "xinput"
    ));
    assert!(router.status().is_empty());
}

#[test]
fn unknown_pointer_is_reported_instead_of_silently_succeeding() {
    let runner =
        FakeCommandRunner::new().respond_stdout("xinput", &["list", "--short"], XINPUT_LIST);
    let resolver =
        FakeDeviceNameResolver::new().with_name("/dev/input/event1", "Not Present In XInput");

    let mut router = XInputRouter::with_resolver(Box::new(runner), Box::new(resolver));
    let error = router
        .assign(
            &group("1-1", &["/dev/input/event1"], &[]),
            &target("host-seat-0", "HDMI-1"),
        )
        .expect_err("unmapped pointer");
    assert!(
        matches!(error, MultiseatError::InputRouting(message) if message.contains("none of the 1 pointer"))
    );
}

#[test]
fn keyboard_only_group_is_tracked_without_a_pointer_mapping() {
    let runner =
        FakeCommandRunner::new().respond_stdout("xinput", &["list", "--short"], XINPUT_LIST);
    let resolver = FakeDeviceNameResolver::new()
        .with_name("/dev/input/event0", "AT Translated Set 2 keyboard");
    let mut router = XInputRouter::with_resolver(Box::new(runner), Box::new(resolver));

    let assignment = router
        .assign(
            &group("1-1", &[], &["/dev/input/event0"]),
            &target("host-seat-0", "HDMI-1"),
        )
        .expect("keyboard-only assignment");
    assert!(assignment.xinput.is_empty());
    assert!(assignment.warnings.is_empty());
    assert_eq!(assignment.devices, vec!["/dev/input/event0"]);
}

#[test]
fn evdev_grab_router_uses_only_the_injected_fake_backend() {
    let backend = FakeEvdevGrabBackend::new();
    let mut router = EvdevGrabRouter::with_backend(Box::new(backend.clone()));
    let group = group("2-1", &["/dev/input/event3"], &["/dev/input/event2"]);

    let assignment = router
        .assign(&group, &target("host-seat-1", "DP-2"))
        .expect("fake grab");
    assert_eq!(
        assignment.grabbed,
        vec!["/dev/input/event2", "/dev/input/event3"]
    );
    assert_eq!(backend.grabbed(), assignment.grabbed);
    assert_eq!(backend.events().len(), 2);
    assert!(backend
        .events()
        .iter()
        .all(|event| event.action == FakeGrabAction::Grab));

    let release = router.release("host-seat-1").expect("release");
    assert_eq!(release.released_devices.len(), 2);
    assert!(backend.grabbed().is_empty());
    assert!(router.status().is_empty());
}

#[test]
fn default_evdev_router_refuses_to_pretend_it_grabbed() {
    let mut router = EvdevGrabRouter::new();
    let error = router
        .assign(
            &group("2-1", &["/dev/input/event3"], &["/dev/input/event2"]),
            &target("host-seat-1", "DP-2"),
        )
        .expect_err("unsupported");
    assert!(matches!(error, MultiseatError::Unsupported(_)));
    assert!(router.status().is_empty());
}

#[test]
fn seat_assignment_rolls_back_earlier_seats_on_failure() {
    let runner = FakeCommandRunner::new()
        .respond_stdout("xinput", &["list", "--short"], XINPUT_LIST)
        .respond(
            "xinput",
            &["map-to-output", "12", "HDMI-1"],
            CommandOutput::success(),
        )
        .respond_failure("xinput", &["map-to-output", "13", "DP-2"], 1, "bad output")
        .respond(
            "xinput",
            &["map-to-output", "12", "all"],
            CommandOutput::success(),
        );
    let resolver = FakeDeviceNameResolver::new()
        .with_name("/dev/input/event1", "USB Optical Mouse")
        .with_name("/dev/input/event2", "Secondary Mouse");
    let mut router = XInputRouter::with_resolver(Box::new(runner.clone()), Box::new(resolver));

    let seats = vec![
        SeatInputAssignment::new(
            group("1-1", &["/dev/input/event1"], &[]),
            target("host-seat-0", "HDMI-1"),
        ),
        SeatInputAssignment::new(
            group("2-1", &["/dev/input/event2"], &[]),
            target("host-seat-1", "DP-2"),
        ),
    ];
    let error = assign_seats(&mut router, &seats).expect_err("second seat fails");
    assert!(matches!(error, MultiseatError::CommandFailed { .. }));
    assert!(router.status().is_empty());
    assert_eq!(
        runner.last_command().expect("rollback").argv(),
        vec!["xinput", "map-to-output", "12", "all"]
    );
}

#[test]
fn duplicate_seat_targets_are_rejected_before_any_command() {
    let runner = FakeCommandRunner::new();
    let resolver = FakeDeviceNameResolver::new();
    let mut router = XInputRouter::with_resolver(Box::new(runner.clone()), Box::new(resolver));

    let seats = vec![
        SeatInputAssignment::new(
            group("1-1", &["/dev/input/event1"], &[]),
            target("host-seat-0", "HDMI-1"),
        ),
        SeatInputAssignment::new(
            group("2-1", &["/dev/input/event2"], &[]),
            target("host-seat-0", "DP-2"),
        ),
    ];
    let error = assign_seats(&mut router, &seats).expect_err("duplicate seat");
    assert!(matches!(error, MultiseatError::InputRouting(_)));
    assert_eq!(runner.run_count(), 0);
}
