// SPDX-License-Identifier: AGPL-3.0-only

use ozma_multiseat::{
    DisplayOutput, FakeDisplayBackend, FakeInputBackend, InputDevice, InputDeviceKind,
    MultiseatPlanner, MultiseatStatus, PlannerOptions, SeatRuntimeStatus, SeatState,
};

fn plan() -> ozma_multiseat::MultiseatPlan {
    let displays = vec![
        DisplayOutput::new(0, "HDMI-1", 1920, 1080, 0, 0, ":0"),
        DisplayOutput::new(1, "DP-2", 2560, 1440, 1920, 0, ":0"),
    ];
    let devices = vec![
        InputDevice {
            path: "/dev/input/event0".to_owned(),
            name: "Keyboard".to_owned(),
            kind: InputDeviceKind::Keyboard,
            usb_hub: Some("1-1".to_owned()),
            sysfs_path: None,
        },
        InputDevice {
            path: "/dev/input/event1".to_owned(),
            name: "Mouse".to_owned(),
            kind: InputDeviceKind::Mouse,
            usb_hub: Some("1-1".to_owned()),
            sysfs_path: None,
        },
    ];
    MultiseatPlanner::new(PlannerOptions {
        machine_name: "host".to_owned(),
        ..PlannerOptions::default()
    })
    .plan_with_backends(
        &FakeDisplayBackend::new(displays),
        &FakeInputBackend::new(devices),
    )
    .expect("plan")
}

#[test]
fn status_document_has_one_node_per_seat_and_stable_json() {
    let plan = plan();
    let status = MultiseatStatus::from_plan(&plan);

    assert!(status.capability.available);
    assert_eq!(status.capability.seat_count, 2);
    assert_eq!(status.capability.node_count, 2);
    assert_eq!(status.capability.display_backend, "fake-display");
    assert_eq!(status.capability.input_backend, "fake-input");
    assert_eq!(status.capability.capture_providers, vec!["x11"]);
    assert_eq!(status.seats.len(), 2);
    assert!(status
        .seats
        .iter()
        .all(|seat| seat.state == SeatState::Planned));

    let value = serde_json::to_value(&status).expect("json");
    assert_eq!(value["capability"]["seat_count"], 2);
    assert_eq!(value["capability"]["node_count"], 2);
    assert_eq!(value["capability"]["capture_providers"][0], "x11");
    assert_eq!(value["seats"][0]["seat_id"], "host-seat-0");
    assert_eq!(value["seats"][0]["node_name"], "host-seat-0");
    assert_eq!(value["seats"][0]["udp_port"], 7331);
    assert_eq!(value["seats"][0]["http_port"], 7382);
    assert_eq!(value["seats"][0]["audio_sink"], "ozma-seat-0");
    assert_eq!(value["seats"][0]["capture"]["kind"], "x11");
    assert_eq!(value["seats"][0]["capture"]["display"], ":0");
    assert_eq!(value["seats"][0]["state"], "planned");
    assert_eq!(
        value["seats"][0]["input_devices"],
        serde_json::json!(["/dev/input/event0", "/dev/input/event1"])
    );

    let decoded: MultiseatStatus = serde_json::from_value(value).expect("roundtrip");
    assert_eq!(decoded, status);
}

#[test]
fn runtime_statuses_update_states_and_unknown_seats_are_ignored() {
    let plan = plan();
    let runtime = vec![
        SeatRuntimeStatus::new("host-seat-0", SeatState::Running),
        SeatRuntimeStatus::new("host-seat-1", SeatState::Failed).with_message("port in use"),
        SeatRuntimeStatus::new("other-seat", SeatState::Running),
    ];

    let status = MultiseatStatus::from_plan_with_runtime(&plan, &runtime);
    assert_eq!(status.seats[0].state, SeatState::Running);
    assert_eq!(status.seats[1].state, SeatState::Failed);
    assert_eq!(status.seats[1].message.as_deref(), Some("port in use"));
    assert_eq!(status.seats.len(), 2);
    assert_eq!(status.capability.node_count, status.capability.seat_count);
}

#[test]
fn pipewire_status_uses_the_pipewire_tag() {
    let displays = vec![DisplayOutput::new(0, "HDMI-1", 1920, 1080, 0, 0, ":0")];
    let mut targets = std::collections::BTreeMap::new();
    targets.insert("HDMI-1".to_owned(), "node:42".to_owned());
    let plan = MultiseatPlanner::new(PlannerOptions {
        machine_name: "host".to_owned(),
        capture: ozma_multiseat::CapturePreference::PipeWire { targets },
        ..PlannerOptions::default()
    })
    .plan(&displays, &[])
    .expect("plan");

    let status = MultiseatStatus::from_plan(&plan);
    assert_eq!(status.capability.capture_providers, vec!["pipewire"]);
    let value = serde_json::to_value(status).expect("json");
    assert_eq!(value["seats"][0]["capture"]["kind"], "pipewire");
    assert_eq!(value["seats"][0]["capture"]["target"], "node:42");
}
