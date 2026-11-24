// SPDX-License-Identifier: AGPL-3.0-only

use std::fs;

use ozma_multiseat::{
    DiagnosticCode, DisplayOutput, InputDevice, InputDeviceKind, MultiseatError, MultiseatPlanner,
    PersistedSeat, PlannerOptions, SeatProfile, SeatStore, SEAT_STORE_VERSION,
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

fn hub_devices(hub: &str, base: u32) -> Vec<InputDevice> {
    vec![
        device(
            &format!("/dev/input/event{base}"),
            InputDeviceKind::Keyboard,
            hub,
        ),
        device(
            &format!("/dev/input/event{}", base + 1),
            InputDeviceKind::Mouse,
            hub,
        ),
    ]
}

fn two_seat_inputs() -> Vec<InputDevice> {
    let mut inputs = hub_devices("1-1", 0);
    inputs.extend(hub_devices("2-1", 2));
    inputs
}

fn planner() -> MultiseatPlanner {
    MultiseatPlanner::new(PlannerOptions {
        machine_name: "host".to_owned(),
        ..PlannerOptions::default()
    })
}

fn initial() -> (ozma_multiseat::MultiseatPlan, SeatStore) {
    let displays = vec![display(0, "HDMI-1", 0), display(1, "DP-2", 1920)];
    let persisted = planner()
        .plan_with_store(&displays, &two_seat_inputs(), &SeatStore::new())
        .expect("initial plan");
    (persisted.plan, persisted.store)
}

#[test]
fn save_reload_roundtrip_is_atomic_and_versioned() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("seats.json");
    let (_, store) = initial();

    store.save_atomic(&path).expect("save");
    let loaded = SeatStore::load(&path).expect("load");
    assert_eq!(loaded, store);
    assert_eq!(loaded.version, SEAT_STORE_VERSION);
    assert_eq!(loaded.seats.len(), 2);

    let entries: Vec<String> = fs::read_dir(directory.path())
        .expect("read dir")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(entries, vec!["seats.json".to_owned()]);

    // A second save replaces the file in place and leaves no temporary file.
    store.save_atomic(&path).expect("overwrite");
    assert_eq!(SeatStore::load(&path).expect("reload"), store);
    assert_eq!(fs::read_dir(directory.path()).expect("read dir").count(), 1);
}

#[test]
fn missing_file_is_an_empty_store() {
    let directory = tempfile::tempdir().expect("tempdir");
    let store = SeatStore::load(directory.path().join("missing.json")).expect("load");
    assert!(store.seats.is_empty());
    assert_eq!(store.version, SEAT_STORE_VERSION);
}

#[test]
fn unversioned_store_migrates_in_memory() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("seats.json");
    let (_, store) = initial();

    let mut value = serde_json::to_value(&store).expect("json");
    value.as_object_mut().expect("object").remove("version");
    fs::write(&path, serde_json::to_string_pretty(&value).expect("encode")).expect("write");

    let loaded = SeatStore::load(&path).expect("migrate");
    assert_eq!(loaded.version, SEAT_STORE_VERSION);
    assert_eq!(loaded.seats, store.seats);
}

#[test]
fn corrupt_and_newer_files_are_typed_errors() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("seats.json");

    fs::write(&path, "{ not json").expect("write corrupt");
    assert!(matches!(
        SeatStore::load(&path).expect_err("corrupt"),
        MultiseatError::SeatStoreParse { .. }
    ));

    fs::write(&path, r#"{"version":999,"seats":[]}"#).expect("write newer");
    assert!(matches!(
        SeatStore::load(&path).expect_err("newer"),
        MultiseatError::SeatStoreVersion { found: 999, .. }
    ));
}

#[test]
fn duplicate_ids_ports_and_displays_are_rejected() {
    let (_, store) = initial();
    let mut duplicate = store.clone();
    duplicate.seats[1].id = duplicate.seats[0].id.clone();
    assert!(matches!(
        duplicate.validate().expect_err("duplicate id"),
        MultiseatError::SeatStoreConflict(_)
    ));

    let mut duplicate = store.clone();
    duplicate.seats[1].udp_port = duplicate.seats[0].udp_port;
    assert!(matches!(
        duplicate.validate().expect_err("duplicate port"),
        MultiseatError::SeatStoreConflict(_)
    ));

    let mut duplicate = store.clone();
    duplicate.seats[1].display = duplicate.seats[0].display.clone();
    assert!(matches!(
        duplicate.validate().expect_err("duplicate display"),
        MultiseatError::SeatStoreConflict(_)
    ));

    let mut zero = store.clone();
    zero.seats[0].udp_port = 0;
    assert!(matches!(
        zero.validate().expect_err("zero port"),
        MultiseatError::SeatStoreInvalid(_)
    ));
}

#[test]
fn persisted_identity_is_stable_when_displays_are_reordered() {
    let (initial_plan, store) = initial();
    let initial_identity: Vec<(String, String, u16, u16)> = initial_plan
        .seats
        .iter()
        .map(|seat| {
            (
                seat.config.display_output.clone(),
                seat.config.id.clone(),
                seat.config.udp_port,
                seat.config.http_port,
            )
        })
        .collect();

    // DP-2 now enumerates first. Matching is by output name, not ordinal.
    let reordered = vec![display(0, "DP-2", 0), display(1, "HDMI-1", 1920)];
    let persisted = planner()
        .plan_with_store(&reordered, &two_seat_inputs(), &store)
        .expect("replan");
    assert_eq!(persisted.plan.seats.len(), 2);

    for seat in &persisted.plan.seats {
        let expected = initial_identity
            .iter()
            .find(|(output, _, _, _)| output == &seat.config.display_output)
            .expect("persisted display");
        assert_eq!(seat.config.id, expected.1);
        assert_eq!(seat.config.udp_port, expected.2);
        assert_eq!(seat.config.http_port, expected.3);
    }
    // The plan is ordered by the new display indexes, but identities follow
    // their displays.
    assert_eq!(persisted.plan.seats[0].config.display_output, "DP-2");
    assert_eq!(persisted.plan.seats[0].config.id, "host-seat-1");
    assert_eq!(persisted.plan.seats[1].config.display_output, "HDMI-1");
    assert_eq!(persisted.plan.seats[1].config.id, "host-seat-0");
}

#[test]
fn missing_display_is_retained_but_not_planned() {
    let (_, store) = initial();
    let only_hdmi = vec![display(0, "HDMI-1", 0)];

    let persisted = planner()
        .plan_with_store(&only_hdmi, &two_seat_inputs(), &store)
        .expect("replan");
    assert_eq!(persisted.plan.seats.len(), 1);
    assert_eq!(persisted.plan.seats[0].config.id, "host-seat-0");
    assert!(persisted.plan.diagnostics.iter().any(|diagnostic| {
        diagnostic.code == DiagnosticCode::PersistedSeatMissingDisplay
            && diagnostic.seat_id.as_deref() == Some("host-seat-1")
    }));
    // The missing display's entry survives so it can regain its id on replug.
    assert!(persisted.store.display_outputs().contains("DP-2"));
    assert_eq!(persisted.store.seats.len(), 2);
}

#[test]
fn missing_hub_and_devices_are_diagnosed_without_resurrection() {
    let (_, store) = initial();
    let displays = vec![display(0, "HDMI-1", 0), display(1, "DP-2", 1920)];

    let persisted = planner()
        .plan_with_store(&displays, &[], &store)
        .expect("replan");
    assert_eq!(persisted.plan.seats.len(), 2);
    // Both persisted hub groups are missing.
    assert_eq!(
        persisted
            .plan
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.code == DiagnosticCode::PersistedSeatMissingInput)
            .count(),
        2
    );
    assert!(persisted
        .plan
        .seats
        .iter()
        .all(|seat| seat.config.input_devices.is_empty()));

    // Removing one device path from an otherwise present hub is also explicit.
    let partial = vec![
        device("/dev/input/event0", InputDeviceKind::Keyboard, "1-1"),
        device("/dev/input/event2", InputDeviceKind::Keyboard, "2-1"),
    ];
    let persisted = planner()
        .plan_with_store(&displays, &partial, &store)
        .expect("partial replan");
    assert!(persisted.plan.diagnostics.iter().any(|diagnostic| {
        diagnostic.code == DiagnosticCode::PersistedSeatMissingInput
            && diagnostic.message.contains("/dev/input/event1")
    }));

    // A different discovered hub must not be silently assigned to a seat whose
    // persisted hub is missing.
    let other = hub_devices("3-3", 10);
    let persisted = planner()
        .plan_with_store(&displays, &other, &store)
        .expect("other hub");
    assert!(persisted
        .plan
        .seats
        .iter()
        .all(|seat| seat.config.input_devices.is_empty()));
    assert!(persisted.plan.diagnostics.iter().any(|diagnostic| {
        diagnostic.code == DiagnosticCode::UnassignedInputGroup
            && diagnostic.message.contains("3-3")
    }));
}

#[test]
fn new_display_gets_the_first_unused_identity_and_port_pair() {
    let (_, mut store) = initial();
    // Pretend the operator renamed/allocated the first persisted seat at 5.
    store.seats[0].id = "host-seat-5".to_owned();
    store.seats[0].udp_port = 7335;
    store.seats[0].http_port = 7386;
    store.seats[0].audio_sink = "ozma-seat-5".to_owned();

    let displays = vec![
        display(0, "HDMI-1", 0),
        display(1, "DP-2", 1920),
        display(2, "DP-3", 3840),
    ];
    let persisted = planner()
        .plan_with_store(&displays, &two_seat_inputs(), &store)
        .expect("replan");
    let added = persisted
        .plan
        .seats
        .iter()
        .find(|seat| seat.config.display_output == "DP-3")
        .expect("new display planned");
    assert_eq!(added.config.id, "host-seat-0");
    assert_eq!(added.config.udp_port, 7331);
    assert_eq!(added.config.http_port, 7382);
    assert_eq!(added.config.audio_sink, "ozma-seat-0");
    assert_eq!(added.config.profile, SeatProfile::Desktop);
}

#[test]
fn stored_profile_is_reused_for_capture_defaults() {
    let (_, mut store) = initial();
    store.seats[0].profile = SeatProfile::Kiosk;
    let displays = vec![display(0, "HDMI-1", 0)];

    let persisted = planner()
        .plan_with_store(&displays, &two_seat_inputs(), &store)
        .expect("replan");
    assert_eq!(persisted.plan.seats[0].config.profile, SeatProfile::Kiosk);
    // X11 capture follows the real display region; the persisted profile
    // controls the capture frame-rate default.
    assert_eq!(persisted.plan.seats[0].config.capture.fps(), 10);
    assert_eq!(persisted.plan.seats[0].config.capture.width(), 1920);
    assert_eq!(persisted.plan.seats[0].config.capture.height(), 1080);
}

#[test]
fn persisted_seat_roundtrips_through_json() {
    let (_, store) = initial();
    let seat: &PersistedSeat = &store.seats[0];
    let value = serde_json::to_value(seat).expect("seat json");
    let decoded: PersistedSeat = serde_json::from_value(value).expect("seat decode");
    assert_eq!(&decoded, seat);
}
