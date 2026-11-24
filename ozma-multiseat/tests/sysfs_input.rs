// SPDX-License-Identifier: AGPL-3.0-only

use std::fs;
use std::path::Path;

use ozma_multiseat::{InputDeviceKind, SysfsInputBackend};

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent");
    }
    fs::write(path, contents).expect("write fixture");
}

fn write_device(sysfs_input: &Path, event: &str, name: &str, ev: &str, key: &str) {
    let device = sysfs_input.join(event).join("device");
    write(&device.join("name"), name);
    write(&device.join("capabilities/ev"), ev);
    write(&device.join("capabilities/key"), key);
}

#[test]
fn enumerates_and_classifies_sysfs_inputs_by_usb_hub() {
    let temp = tempfile::tempdir().expect("tempdir");
    // Ancestors deliberately look like a real USB path so canonicalization
    // exercises the hub trace.
    let sysfs_input = temp
        .path()
        .join("sys/devices/pci0000:00/usb1/1-1/1-1.2/1-1.2:1.0/class/input");
    let dev_input = temp.path().join("dev/input");
    fs::create_dir_all(&dev_input).expect("dev input");

    for event in ["event0", "event1", "event2", "event3", "event4", "event5"] {
        write(&dev_input.join(event), "");
    }

    write_device(
        &sysfs_input,
        "event0",
        "AT Keyboard",
        "3",
        "ffffffffffffffff",
    );
    write_device(&sysfs_input, "event1", "USB Mouse", "7", "70000");
    write_device(&sysfs_input, "event2", "Gamepad", "b", "30000");
    write_device(&sysfs_input, "event3", "Power Button", "1", "");

    let backend = SysfsInputBackend::with_roots(&sysfs_input, &dev_input);
    let devices = backend.enumerate().expect("enumerate");
    assert_eq!(devices.len(), 3);

    assert_eq!(
        devices[0].path,
        dev_input.join("event0").display().to_string()
    );
    assert_eq!(devices[0].name, "AT Keyboard");
    assert_eq!(devices[0].kind, InputDeviceKind::Keyboard);
    assert_eq!(devices[0].usb_hub.as_deref(), Some("1-1"));

    assert_eq!(devices[1].kind, InputDeviceKind::Mouse);
    assert_eq!(devices[2].kind, InputDeviceKind::Gamepad);
    assert!(devices
        .iter()
        .all(|device| device.usb_hub.as_deref() == Some("1-1")));
}

#[test]
fn devices_without_sysfs_capabilities_are_skipped() {
    let temp = tempfile::tempdir().expect("tempdir");
    let sysfs_input = temp.path().join("sys/class/input");
    let dev_input = temp.path().join("dev/input");
    fs::create_dir_all(&dev_input).expect("dev input");
    write(&dev_input.join("event0"), "");
    write(&dev_input.join("event1"), "");
    write(&dev_input.join("event2"), "");

    // event0 has no device directory.
    // event1 has a device but no capabilities.
    fs::create_dir_all(sysfs_input.join("event1/device")).expect("device");
    // event2 classifies as nothing.
    write_device(&sysfs_input, "event2", "Odd Device", "1", "");

    let backend = SysfsInputBackend::with_roots(&sysfs_input, &dev_input);
    assert!(backend.enumerate().expect("enumerate").is_empty());
}

#[test]
fn detection_requires_event_nodes() {
    let temp = tempfile::tempdir().expect("tempdir");
    let dev_input = temp.path().join("dev/input");
    fs::create_dir_all(&dev_input).expect("dev input");

    assert!(!SysfsInputBackend::detect_at(&dev_input).available);
    write(&dev_input.join("event0"), "");
    let detection = SysfsInputBackend::detect_at(&dev_input);
    assert!(detection.available);
    assert_eq!(detection.backend, "sysfs");

    let missing = temp.path().join("missing");
    assert!(!SysfsInputBackend::detect_at(&missing).available);
}
