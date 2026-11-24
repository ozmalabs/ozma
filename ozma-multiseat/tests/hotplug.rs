// SPDX-License-Identifier: AGPL-3.0-only

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use ozma_multiseat::{
    DiagnosticCode, DisplayOutput, FakeHotplugMonitor, FakeSeatSupervisor, HotplugDebounce,
    HotplugEvent, HotplugMonitor, HotplugOptions, InputDevice, InputDeviceKind, MultiseatError,
    MultiseatPlanner, PlannerOptions, SeatReconciler, SeatState, SeatStore, SeatSupervisor,
    SysfsHotplugMonitor,
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

fn planner() -> MultiseatPlanner {
    MultiseatPlanner::new(PlannerOptions {
        machine_name: "host".to_owned(),
        ..PlannerOptions::default()
    })
}

fn reconciler(displays: Vec<DisplayOutput>, inputs: Vec<InputDevice>) -> SeatReconciler {
    SeatReconciler::new(planner(), SeatStore::new(), displays, inputs)
}

#[test]
fn adding_and_removing_a_display_starts_and_stops_the_seat() {
    let mut supervisor = FakeSeatSupervisor::new();
    let mut reconciler = reconciler(vec![display(0, "HDMI-1", 0)], hub_devices("1-1", 0));

    let report = reconciler.reconcile(&mut supervisor);
    assert_eq!(report.started, vec!["host-seat-0"]);
    assert_eq!(supervisor.start_count("host-seat-0"), 1);

    reconciler.apply_event(HotplugEvent::DisplayAdded(display(1, "DP-2", 1920)));
    let report = reconciler.reconcile(&mut supervisor);
    assert_eq!(report.started, vec!["host-seat-1"]);
    assert_eq!(supervisor.state("host-seat-1"), Some(SeatState::Running));
    assert_eq!(reconciler.store().seats.len(), 2);

    reconciler.apply_event(HotplugEvent::DisplayRemoved {
        output: "HDMI-1".to_owned(),
    });
    let report = reconciler.reconcile(&mut supervisor);
    assert_eq!(report.stopped, vec!["host-seat-0"]);
    assert_eq!(supervisor.state("host-seat-0"), Some(SeatState::Stopped));
    assert_eq!(supervisor.stop_count("host-seat-0"), 1);
    // The removed display's identity is retained for replug, not resurrected.
    assert!(reconciler.store().display_outputs().contains("HDMI-1"));
    assert_eq!(report.plan.expect("plan").seats.len(), 1);
}

#[test]
fn hub_add_and_remove_restarts_the_affected_seat_with_diagnostics() {
    let mut supervisor = FakeSeatSupervisor::new();
    let mut reconciler = reconciler(vec![display(0, "HDMI-1", 0)], Vec::new());

    reconciler.reconcile(&mut supervisor);
    assert_eq!(supervisor.start_count("host-seat-0"), 1);
    assert!(reconciler
        .known_seats()
        .next()
        .expect("seat")
        .config
        .input_devices
        .is_empty());

    reconciler.apply_event(HotplugEvent::InputHubAdded {
        hub_path: "1-1".to_owned(),
        devices: hub_devices("1-1", 0),
    });
    let report = reconciler.reconcile(&mut supervisor);
    assert_eq!(report.restarted, vec!["host-seat-0"]);
    assert_eq!(supervisor.start_count("host-seat-0"), 2);
    assert_eq!(supervisor.stop_count("host-seat-0"), 1);
    let seat = reconciler.known_seats().next().expect("seat");
    assert_eq!(seat.config.input_hub.as_deref(), Some("1-1"));
    assert_eq!(seat.config.input_devices.len(), 2);

    reconciler.apply_event(HotplugEvent::InputHubRemoved {
        hub_path: "1-1".to_owned(),
    });
    let report = reconciler.reconcile(&mut supervisor);
    assert_eq!(report.restarted, vec!["host-seat-0"]);
    assert!(report
        .diagnostics
        .iter()
        .any(|diagnostic| { diagnostic.code == DiagnosticCode::PersistedSeatMissingInput }));
    assert!(reconciler
        .known_seats()
        .next()
        .expect("seat")
        .config
        .input_devices
        .is_empty());
}

#[test]
fn display_geometry_change_restarts_without_changing_the_seat_id() {
    let mut supervisor = FakeSeatSupervisor::new();
    let mut reconciler = reconciler(vec![display(0, "HDMI-1", 0)], Vec::new());
    reconciler.reconcile(&mut supervisor);

    let mut resized = display(0, "HDMI-1", 0);
    resized.width = 2560;
    resized.height = 1440;
    reconciler.apply_event(HotplugEvent::DisplayAdded(resized));

    let report = reconciler.reconcile(&mut supervisor);
    assert_eq!(report.restarted, vec!["host-seat-0"]);
    assert_eq!(supervisor.start_count("host-seat-0"), 2);
    let seat = reconciler.known_seats().next().expect("seat");
    assert_eq!(seat.config.id, "host-seat-0");
    assert_eq!(seat.config.capture.width(), 2560);
    assert_eq!(seat.config.capture.height(), 1440);
}

#[test]
fn debounce_cancels_a_transient_add_remove_and_keeps_a_noop_quiet() {
    let mut supervisor = FakeSeatSupervisor::new();
    let mut reconciler = reconciler(vec![display(0, "HDMI-1", 0)], Vec::new());
    reconciler.reconcile(&mut supervisor);
    let store_before = reconciler.store().clone();

    let mut monitor = FakeHotplugMonitor::new();
    monitor
        .push(HotplugEvent::DisplayAdded(display(1, "DP-2", 1920)))
        .push(HotplugEvent::DisplayRemoved {
            output: "DP-2".to_owned(),
        });

    let mut debounce = HotplugDebounce::new(Duration::from_millis(500));
    let start = Instant::now();
    let report = reconciler
        .poll_and_reconcile(&mut monitor, &mut debounce, start, &mut supervisor)
        .expect("first poll");
    assert!(!report.changed);
    assert_eq!(supervisor.start_count("host-seat-0"), 1);

    let report = reconciler
        .poll_and_reconcile(
            &mut monitor,
            &mut debounce,
            start + Duration::from_millis(600),
            &mut supervisor,
        )
        .expect("settled poll");
    assert!(!report.changed);
    assert!(report.started.is_empty());
    assert!(report.stopped.is_empty());
    assert!(report.restarted.is_empty());
    assert_eq!(supervisor.start_count("host-seat-0"), 1);
    assert_eq!(reconciler.store(), &store_before);

    // A later quiet poll for unchanged topology is also a no-op.
    let report = reconciler
        .poll_and_reconcile(
            &mut monitor,
            &mut debounce,
            start + Duration::from_millis(1200),
            &mut supervisor,
        )
        .expect("quiet poll");
    assert!(!report.changed);
    assert!(report.unchanged.is_empty());
    assert_eq!(report.statuses.len(), 1);
    assert_eq!(report.statuses[0].seat_id, "host-seat-0");
    assert_eq!(monitor.polls(), 3);

    // A direct reconcile of unchanged topology reports the seat as unchanged.
    let report = reconciler.reconcile(&mut supervisor);
    assert!(!report.changed);
    assert_eq!(report.unchanged, vec!["host-seat-0"]);
}

#[test]
fn excluded_displays_hubs_and_seats_are_not_touched() {
    // Excluded display never enters the plan.
    let mut supervisor = FakeSeatSupervisor::new();
    let options = HotplugOptions::default()
        .with_excluded_display("DP-2")
        .with_excluded_hub("9-9");
    let mut first = reconciler(
        vec![display(0, "HDMI-1", 0), display(1, "DP-2", 1920)],
        hub_devices("9-9", 0),
    )
    .with_options(options);
    let report = first.reconcile(&mut supervisor);
    assert_eq!(report.started, vec!["host-seat-0"]);
    assert!(report
        .plan
        .expect("plan")
        .seats
        .iter()
        .all(|seat| seat.config.display_output != "DP-2"));

    // An excluded running seat is not stopped when its display disappears.
    let mut supervisor = FakeSeatSupervisor::new();
    let mut second = reconciler(
        vec![display(0, "HDMI-1", 0), display(1, "DP-2", 1920)],
        Vec::new(),
    );
    second.reconcile(&mut supervisor);
    assert_eq!(supervisor.state("host-seat-0"), Some(SeatState::Running));

    second.set_options(HotplugOptions::default().with_excluded_seat("host-seat-0"));
    second.apply_event(HotplugEvent::DisplayRemoved {
        output: "HDMI-1".to_owned(),
    });
    let report = second.reconcile(&mut supervisor);
    assert!(report.stopped.is_empty());
    assert!(report.excluded.contains(&"host-seat-0".to_owned()));
    assert_eq!(supervisor.state("host-seat-0"), Some(SeatState::Running));
}

#[test]
fn monitor_failure_is_a_typed_error_not_a_false_report() {
    let mut supervisor = FakeSeatSupervisor::new();
    let mut reconciler = reconciler(vec![display(0, "HDMI-1", 0)], Vec::new());
    let mut monitor = FakeHotplugMonitor::new();
    monitor.fail_next_poll("udev socket closed");
    let mut debounce = HotplugDebounce::new(Duration::from_millis(1));

    let error = reconciler
        .poll_and_reconcile(&mut monitor, &mut debounce, Instant::now(), &mut supervisor)
        .expect_err("poll failure");
    assert!(matches!(error, MultiseatError::Hotplug(message) if message == "udev socket closed"));
    assert!(supervisor.status().is_empty());
}

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent");
    }
    fs::write(path, contents).expect("write fixture");
}

fn write_input_device(sysfs_input: &Path, event: &str, name: &str, ev: &str, key: &str) {
    let device = sysfs_input.join(event).join("device");
    write(&device.join("name"), name);
    write(&device.join("capabilities/ev"), ev);
    write(&device.join("capabilities/key"), key);
}

fn write_drm_connector(root: &Path, name: &str, status: &str, mode: &str) {
    let connector = root.join(format!("card0-{name}"));
    write(&connector.join("status"), status);
    write(&connector.join("modes"), mode);
}

#[test]
fn sysfs_monitor_baseline_then_reports_display_and_hub_changes() {
    let temp = tempfile::tempdir().expect("tempdir");
    let sysfs_input = temp
        .path()
        .join("sys/devices/pci0000:00/usb1/1-1/1-1.2/1-1.2:1.0/class/input");
    let dev_input = temp.path().join("dev/input");
    let drm_root = temp.path().join("sys/class/drm");
    fs::create_dir_all(&dev_input).expect("dev input");
    fs::create_dir_all(&drm_root).expect("drm root");

    let mut monitor = SysfsHotplugMonitor::with_roots(&sysfs_input, &dev_input, &drm_root);
    assert!(monitor.poll().expect("baseline").is_empty());

    write(&dev_input.join("event0"), "");
    write_input_device(
        &sysfs_input,
        "event0",
        "USB Keyboard",
        "3",
        "ffffffffffffffff",
    );
    write_drm_connector(&drm_root, "HDMI-A-1", "connected", "1920x1080\n");

    let events = monitor.poll().expect("adds");
    assert!(events.iter().any(
        |event| matches!(event, HotplugEvent::DisplayAdded(display) if display.name == "HDMI-A-1")
    ));
    assert!(events.iter().any(|event| {
        matches!(event, HotplugEvent::InputHubAdded { hub_path, devices }
            if hub_path == "1-1" && devices.len() == 1)
    }));

    fs::remove_file(dev_input.join("event0")).expect("remove event");
    write_drm_connector(&drm_root, "HDMI-A-1", "disconnected", "1920x1080\n");

    let events = monitor.poll().expect("removals");
    assert!(events.iter().any(|event| {
        matches!(event, HotplugEvent::DisplayRemoved { output } if output == "HDMI-A-1")
    }));
    assert!(events.iter().any(|event| {
        matches!(event, HotplugEvent::InputHubRemoved { hub_path } if hub_path == "1-1")
    }));
}
