// SPDX-License-Identifier: AGPL-3.0-only
//!
//! Opt-in, live X11 routing smoke. Nothing runs unless `OZMA_MULTISEAT_REAL=1`.
//!
//! The read-only check runs `xinput list --short` against an explicit display.
//! The mapping check additionally requires `OZMA_MULTISEAT_XINPUT_MAP=1` and is
//! intended for a throwaway Xvfb display, never the operator's desktop. It
//! maps one pointer with `xinput map-to-output` and resets it to `all`; it never
//! grabs an evdev device and never injects input.
//!
//! ```bash
//! Xvfb :99 -screen 0 1280x800x24 &
//! OZMA_MULTISEAT_REAL=1 OZMA_MULTISEAT_XINPUT_MAP=1 DISPLAY=:99 \
//!   cargo test -p ozma-multiseat --test real_routing -- --ignored --nocapture
//! ```

use ozma_multiseat::{
    FakeDeviceNameResolver, InputGroup, InputRouter, InputTarget, ProcessCommandRunner,
    XInputRouter,
};

fn enabled() -> bool {
    std::env::var("OZMA_MULTISEAT_REAL").as_deref() == Ok("1")
}

fn display() -> String {
    std::env::var("OZMA_MULTISEAT_XDISPLAY")
        .or_else(|_| std::env::var("DISPLAY"))
        .expect("set DISPLAY or OZMA_MULTISEAT_XDISPLAY")
}

#[test]
#[ignore = "opt-in real host: set OZMA_MULTISEAT_REAL=1"]
fn real_xinput_list_is_read_only() {
    if !enabled() {
        return;
    }
    let display = display();
    let router = XInputRouter::new(Box::new(ProcessCommandRunner));
    let devices = router.list_devices(&display).expect("xinput list");
    assert!(
        !devices.is_empty(),
        "xinput reported no devices on {display}; refusing a fake success"
    );
    for device in &devices {
        println!("xinput id={} name={:?}", device.id, device.name);
    }
}

#[test]
#[ignore = "opt-in throwaway Xvfb: set OZMA_MULTISEAT_REAL=1 and OZMA_MULTISEAT_XINPUT_MAP=1"]
fn real_xinput_map_to_output_and_reset_on_a_throwaway_display() {
    if !enabled() || std::env::var("OZMA_MULTISEAT_XINPUT_MAP").as_deref() != Ok("1") {
        return;
    }

    let display = display();
    let device_name =
        std::env::var("OZMA_MULTISEAT_XINPUT_DEVICE").unwrap_or_else(|_| "Xvfb mouse".to_owned());
    let output =
        std::env::var("OZMA_MULTISEAT_XINPUT_OUTPUT").unwrap_or_else(|_| "screen".to_owned());

    let resolver =
        FakeDeviceNameResolver::new().with_name("/dev/input/event0", device_name.clone());
    let mut router =
        XInputRouter::with_resolver(Box::new(ProcessCommandRunner), Box::new(resolver));

    let group = InputGroup {
        hub_path: "xvfb-test".to_owned(),
        keyboards: Vec::new(),
        mice: vec!["/dev/input/event0".to_owned()],
        gamepads: Vec::new(),
        other: Vec::new(),
    };
    let target = InputTarget::new("xvfb-seat-0", &output, &display).expect("target");

    let assignment = router.assign(&group, &target).expect("map-to-output");
    assert_eq!(assignment.xinput.len(), 1);
    assert_eq!(assignment.xinput[0].device_name, device_name);
    println!(
        "mapped xinput id {} ({:?}) to {} on {}",
        assignment.xinput[0].xinput_id, assignment.xinput[0].device_name, output, display
    );

    let release = router.release("xvfb-seat-0").expect("reset to all");
    assert_eq!(release.released_devices, vec!["/dev/input/event0"]);
    println!("released {release:?}");
}
