// SPDX-License-Identifier: AGPL-3.0-only
//!
//! Protocol and helper state-machine tests. Every fake backend records requests
//! and touches no `/dev/input`, `/dev/uinput`, or process except the opt-in
//! `--backend fake` subprocess protocol check.

use ozma_multiseat::{
    decode_request, decode_response, encode_request, encode_response, helper_devices,
    validate_device_path, HelperBackendKind, HelperClient, HelperClientConfig, HelperDevice,
    HelperErrorCode, HelperRequest, HelperResponse, InputDeviceKind, InputGroup,
    InputIsolationHelper, InputRoutePlan, InputRoutingStrategy, SeatInputHelper,
    HELPER_PROTOCOL_VERSION,
};
use ozma_multiseat::{FakeHelperBackend, FakeSeatInputHelper};

fn device(path: &str) -> HelperDevice {
    HelperDevice::new(path, InputDeviceKind::Keyboard)
}

fn grab_request(seat: &str, paths: &[&str]) -> HelperRequest {
    HelperRequest::Grab {
        seat_id: seat.to_owned(),
        devices: paths.iter().map(|path| device(path)).collect(),
    }
}

#[test]
fn grab_confirms_both_devices_and_release_cleans_up() {
    let backend = FakeHelperBackend::new();
    let observed = backend.clone();
    let mut helper = InputIsolationHelper::new(backend, "test-helper", 7);

    let ready = helper.handle(HelperRequest::Hello {
        protocol: HELPER_PROTOCOL_VERSION,
    });
    assert!(matches!(
        ready,
        HelperResponse::Ready {
            protocol: HELPER_PROTOCOL_VERSION,
            backend: HelperBackendKind::Fake,
            pid: 7,
            ..
        }
    ));

    let response = helper.handle(grab_request(
        "host-seat-0",
        &["/dev/input/event0", "/dev/input/event1"],
    ));
    match response {
        HelperResponse::Grabbed {
            seat_id,
            grabbed,
            uinput_devices,
        } => {
            assert_eq!(seat_id, "host-seat-0");
            assert_eq!(grabbed, vec!["/dev/input/event0", "/dev/input/event1"]);
            assert_eq!(uinput_devices.len(), 2);
            assert!(uinput_devices[0].starts_with("ozma-seat-host-seat-0-event0"));
        }
        other => panic!("unexpected response: {other:?}"),
    }
    assert_eq!(
        observed.grabbed(),
        vec!["/dev/input/event0", "/dev/input/event1"]
    );
    assert_eq!(observed.uinput_names().len(), 2);

    let status = helper.handle(HelperRequest::Status);
    match status {
        HelperResponse::Status { seats } => {
            assert_eq!(seats.len(), 1);
            assert_eq!(seats[0].seat_id, "host-seat-0");
            assert_eq!(seats[0].grabbed.len(), 2);
        }
        other => panic!("unexpected status: {other:?}"),
    }

    let released = helper.handle(HelperRequest::Release {
        seat_id: "host-seat-0".to_owned(),
    });
    match released {
        HelperResponse::Released { released, .. } => {
            assert_eq!(released, vec!["/dev/input/event0", "/dev/input/event1"])
        }
        other => panic!("unexpected release: {other:?}"),
    }
    assert!(observed.grabbed().is_empty());
    assert!(observed.uinput_names().is_empty());
}

#[test]
fn invalid_paths_are_rejected_before_any_grab() {
    let backend = FakeHelperBackend::new();
    let observed = backend.clone();
    let mut helper = InputIsolationHelper::new(backend, "test-helper", 1);

    for path in [
        "/dev/input/by-id/keyboard",
        "/dev/input/event",
        "/dev/input/eventX",
        "/dev/uinput",
        "/dev/input/event0/../event1",
        "relative/event0",
    ] {
        let response = helper.handle(grab_request("host-seat-0", &[path]));
        match response {
            HelperResponse::Error { error } => {
                assert_eq!(error.code, HelperErrorCode::InvalidPath, "path {path}");
                assert_eq!(error.device.as_deref(), Some(path));
            }
            other => panic!("path {path} was not rejected: {other:?}"),
        }
    }
    assert!(observed.events().is_empty());
}

#[test]
fn duplicate_paths_in_one_request_are_rejected() {
    let backend = FakeHelperBackend::new();
    let observed = backend.clone();
    let mut helper = InputIsolationHelper::new(backend, "test-helper", 1);

    let response = helper.handle(grab_request(
        "host-seat-0",
        &["/dev/input/event0", "/dev/input/event0"],
    ));
    match response {
        HelperResponse::Error { error } => {
            assert_eq!(error.code, HelperErrorCode::DuplicateDevice);
        }
        other => panic!("duplicate was not rejected: {other:?}"),
    }
    assert!(observed.events().is_empty());
}

#[test]
fn cross_seat_and_changed_set_grabs_are_rejected() {
    let backend = FakeHelperBackend::new();
    let observed = backend.clone();
    let mut helper = InputIsolationHelper::new(backend, "test-helper", 1);

    assert!(matches!(
        helper.handle(grab_request("seat-a", &["/dev/input/event0"])),
        HelperResponse::Grabbed { .. }
    ));

    let conflict = helper.handle(grab_request("seat-b", &["/dev/input/event0"]));
    match conflict {
        HelperResponse::Error { error } => {
            assert_eq!(error.code, HelperErrorCode::CrossSeatConflict);
            assert_eq!(error.seat_id.as_deref(), Some("seat-b"));
            assert_eq!(error.device.as_deref(), Some("/dev/input/event0"));
        }
        other => panic!("cross-seat grab was not rejected: {other:?}"),
    }

    // The same seat repeating the same set is idempotent, while a changed set
    // requires an explicit release first.
    assert!(matches!(
        helper.handle(grab_request("seat-a", &["/dev/input/event0"])),
        HelperResponse::Grabbed { .. }
    ));
    let changed = helper.handle(grab_request(
        "seat-a",
        &["/dev/input/event0", "/dev/input/event1"],
    ));
    match changed {
        HelperResponse::Error { error } => {
            assert_eq!(error.code, HelperErrorCode::SeatAlreadyGrabbed);
        }
        other => panic!("changed set was not rejected: {other:?}"),
    }
    assert_eq!(observed.grabbed(), vec!["/dev/input/event0"]);
}

#[test]
fn grab_failure_rolls_back_earlier_devices() {
    let backend = FakeHelperBackend::new().fail_device("/dev/input/event1");
    let observed = backend.clone();
    let mut helper = InputIsolationHelper::new(backend, "test-helper", 1);

    let response = helper.handle(grab_request(
        "seat-a",
        &["/dev/input/event0", "/dev/input/event1"],
    ));
    match response {
        HelperResponse::Error { error } => {
            assert_eq!(error.code, HelperErrorCode::GrabFailed);
            assert!(error.message.contains("rollback") || error.device.is_some());
        }
        other => panic!("grab failure was not reported: {other:?}"),
    }
    assert!(observed.grabbed().is_empty());
    assert!(observed.uinput_names().is_empty());
    assert!(matches!(
        helper.handle(HelperRequest::Status),
        HelperResponse::Status { seats } if seats.is_empty()
    ));
}

#[test]
fn uinput_failure_rolls_back_the_grab_and_prior_devices() {
    let backend = FakeHelperBackend::new().fail_uinput("/dev/input/event1");
    let observed = backend.clone();
    let mut helper = InputIsolationHelper::new(backend, "test-helper", 1);

    let response = helper.handle(grab_request(
        "seat-a",
        &["/dev/input/event0", "/dev/input/event1"],
    ));
    match response {
        HelperResponse::Error { error } => {
            assert_eq!(error.code, HelperErrorCode::UinputFailed);
        }
        other => panic!("uinput failure was not reported: {other:?}"),
    }
    assert!(observed.grabbed().is_empty());
    assert!(observed.uinput_names().is_empty());
}

#[test]
fn release_failure_keeps_the_assignment_for_retry() {
    let backend = FakeHelperBackend::new().fail_next_release("injected release failure");
    let observed = backend.clone();
    let mut helper = InputIsolationHelper::new(backend, "test-helper", 1);

    helper.handle(grab_request("seat-a", &["/dev/input/event0"]));
    let failed = helper.handle(HelperRequest::Release {
        seat_id: "seat-a".to_owned(),
    });
    match failed {
        HelperResponse::Error { error } => {
            assert_eq!(error.code, HelperErrorCode::Backend);
            assert!(error.message.contains("injected release failure"));
        }
        other => panic!("release failure was not reported: {other:?}"),
    }
    assert!(matches!(
        helper.handle(HelperRequest::Status),
        HelperResponse::Status { seats } if seats.len() == 1
    ));

    let retried = helper.handle(HelperRequest::Release {
        seat_id: "seat-a".to_owned(),
    });
    assert!(matches!(retried, HelperResponse::Released { .. }));
    assert!(observed.grabbed().is_empty());
}

#[test]
fn release_is_idempotent_and_shutdown_drops_every_seat() {
    let backend = FakeHelperBackend::new();
    let observed = backend.clone();
    let mut helper = InputIsolationHelper::new(backend, "test-helper", 1);

    assert!(matches!(
        helper.handle(HelperRequest::Release {
            seat_id: "unknown".to_owned()
        }),
        HelperResponse::Released { released, .. } if released.is_empty()
    ));

    helper.handle(grab_request("seat-a", &["/dev/input/event0"]));
    helper.handle(grab_request("seat-b", &["/dev/input/event1"]));
    assert!(matches!(
        helper.handle(HelperRequest::Shutdown),
        HelperResponse::ShuttingDown
    ));
    assert!(observed.grabbed().is_empty());
    assert!(observed.uinput_names().is_empty());
    assert!(matches!(
        helper.handle(HelperRequest::Status),
        HelperResponse::Error { .. }
    ));
}

#[test]
fn plan_switches_to_the_helper_router_and_environment() {
    let mut group = InputGroup::new("1-1");
    group.keyboards.push("/dev/input/event0".to_owned());
    group.mice.push("/dev/input/event1".to_owned());
    let mut route = InputRoutePlan::for_group("host-seat-0", "HDMI-1", ":0", &group);
    assert_eq!(route.strategy, InputRoutingStrategy::XInputMapToOutput);

    route.enable_input_isolation();
    assert_eq!(route.strategy, InputRoutingStrategy::InputIsolation);
    assert!(route.grab_exclusive);
    assert_eq!(
        route.env.get("OZMA_INPUT_ROUTER").map(String::as_str),
        Some("seat-input-helper")
    );
    assert_eq!(
        route.env.get("OZMA_UINPUT_SEAT").map(String::as_str),
        Some("host-seat-0")
    );
    assert_eq!(helper_devices(&group).len(), 2);
}

#[test]
fn protocol_frames_round_trip() {
    let request = grab_request("seat-a", &["/dev/input/event0"]);
    let encoded = encode_request(&request).expect("encode");
    assert!(!encoded.contains('\n'));
    assert_eq!(decode_request(&encoded).expect("decode"), request);

    let response = HelperResponse::Released {
        seat_id: "seat-a".to_owned(),
        released: vec!["/dev/input/event0".to_owned()],
    };
    let encoded = encode_response(&response).expect("encode");
    assert_eq!(decode_response(&encoded).expect("decode"), response);
}

#[test]
fn process_client_speaks_the_protocol_to_the_fake_helper() {
    let program = env!("CARGO_BIN_EXE_ozma-seat-input");
    let mut client = HelperClient::new(HelperClientConfig {
        program: program.to_owned(),
        args: vec!["--backend".to_owned(), "fake".to_owned()],
        allow_fake_backend: true,
    });

    let ready = client.ensure_ready().expect("helper ready");
    assert!(matches!(
        ready,
        HelperResponse::Ready {
            backend: HelperBackendKind::Fake,
            ..
        }
    ));

    let devices = vec![
        HelperDevice::new("/dev/input/event0", InputDeviceKind::Keyboard),
        HelperDevice::new("/dev/input/event1", InputDeviceKind::Mouse),
    ];
    let assignment = client.grab("host-seat-0", &devices).expect("grab");
    assert_eq!(assignment.grabbed.len(), 2);
    assert_eq!(assignment.uinput_devices.len(), 2);

    let status = client
        .seat_status("host-seat-0")
        .expect("status")
        .expect("assignment");
    assert_eq!(status.grabbed.len(), 2);

    let release = client.release("host-seat-0").expect("release");
    assert_eq!(release.released.len(), 2);
    assert!(client.seat_status("host-seat-0").expect("status").is_none());

    client.shutdown();
    assert!(!client.is_alive());
}

#[test]
fn process_client_refuses_a_fake_helper_without_explicit_opt_in() {
    let program = env!("CARGO_BIN_EXE_ozma-seat-input");
    let mut client = HelperClient::new(HelperClientConfig {
        program: program.to_owned(),
        args: vec!["--backend".to_owned(), "fake".to_owned()],
        allow_fake_backend: false,
    });

    let error = client.ensure_ready().expect_err("fake refused");
    assert!(error.to_string().contains("test-only fake backend"));
    assert!(!client.is_alive());
}

#[test]
fn fake_supervisor_helper_records_assignments_without_devices() {
    let mut helper = FakeSeatInputHelper::new().fail_next_grab("injected");
    assert!(helper
        .grab("seat-a", &[device("/dev/input/event0")])
        .is_err());
    assert!(!helper.has_assignment("seat-a"));
}

/// Opt-in real-device smoke.
///
/// Requires `OZMA_MULTISEAT_REAL_INPUT_GRAB=1`, an explicit
/// `OZMA_MULTISEAT_INPUT_DEVICE=/dev/input/eventN`, and permission to open and
/// `EVIOCGRAB` that node plus `/dev/uinput`. It releases the device before
/// returning. Never part of the default suite.
#[test]
#[ignore = "requires OZMA_MULTISEAT_REAL_INPUT_GRAB=1, a real evdev node, and input/uinput permission"]
fn real_helper_grabs_and_releases_the_configured_device() {
    if std::env::var("OZMA_MULTISEAT_REAL_INPUT_GRAB").as_deref() != Ok("1") {
        eprintln!("SKIP: set OZMA_MULTISEAT_REAL_INPUT_GRAB=1 to run");
        return;
    }
    let path = std::env::var("OZMA_MULTISEAT_INPUT_DEVICE")
        .expect("set OZMA_MULTISEAT_INPUT_DEVICE=/dev/input/eventN");
    validate_device_path(&path).expect("configured path must be /dev/input/eventN");

    let program = env!("CARGO_BIN_EXE_ozma-seat-input");
    let mut client = HelperClient::new(HelperClientConfig {
        program: program.to_owned(),
        args: Vec::new(),
        allow_fake_backend: false,
    });
    let assignment = client
        .grab(
            "real-input-test",
            &[HelperDevice::new(&path, InputDeviceKind::Keyboard)],
        )
        .expect("real grab");
    eprintln!(
        "grabbed {path} as seat real-input-test with uinput {:?}",
        assignment.uinput_devices
    );
    assert_eq!(assignment.grabbed, vec![path.clone()]);
    assert_eq!(assignment.uinput_devices.len(), 1);

    let release = client.release("real-input-test").expect("real release");
    assert_eq!(release.released, vec![path]);
    client.shutdown();
}
