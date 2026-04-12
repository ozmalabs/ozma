// SPDX-License-Identifier: AGPL-3.0-only
//! Integration test: virtual gamepad via Linux uinput → gilrs event routing.
//!
//! Requires:
//!   - Linux with `/dev/uinput` accessible (CI: run as root or with `uinput` group)
//!   - `uinput` crate (dev-dependency)
//!
//! The test:
//!   1. Creates a virtual Xbox-style gamepad via uinput.
//!   2. Spawns `GamepadDriver::run` in a background thread.
//!   3. Injects button / axis events.
//!   4. Asserts the expected `ControlEvent`s arrive via the callback.

#![cfg(target_os = "linux")]

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use ozma_drivers::gamepad::{mapping::ControlEvent, GamepadDriver};

// ── uinput virtual device ─────────────────────────────────────────────────────

fn create_virtual_gamepad() -> Result<uinput::Device, Box<dyn std::error::Error>> {
    let device = uinput::open("/dev/uinput")?
        .name("Virtual Xbox Controller")?
        // Face buttons
        .event(uinput::event::keyboard::Key::ButtonSouth)?
        .event(uinput::event::keyboard::Key::ButtonEast)?
        .event(uinput::event::keyboard::Key::ButtonNorth)?
        .event(uinput::event::keyboard::Key::ButtonWest)?
        // Bumpers / triggers (digital)
        .event(uinput::event::keyboard::Key::ButtonTL)?
        .event(uinput::event::keyboard::Key::ButtonTR)?
        // Guide
        .event(uinput::event::keyboard::Key::ButtonMode)?
        // Analog axes
        .event(uinput::event::absolute::Position::X)?
        .event(uinput::event::absolute::Position::Y)?
        .event(uinput::event::absolute::Position::RX)?
        .event(uinput::event::absolute::Position::RY)?
        .event(uinput::event::absolute::Position::Z)?
        .event(uinput::event::absolute::Position::RZ)?
        // D-pad hat
        .event(uinput::event::absolute::Hat::X0)?
        .event(uinput::event::absolute::Hat::Y0)?
        .create()?;

    // Give the kernel time to register the device with gilrs
    thread::sleep(Duration::from_millis(250));
    Ok(device)
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn press(dev: &mut uinput::Device, btn: uinput::event::keyboard::Key) {
    dev.press(&btn).unwrap();
    dev.synchronize().unwrap();
    thread::sleep(Duration::from_millis(20));
    dev.release(&btn).unwrap();
    dev.synchronize().unwrap();
    thread::sleep(Duration::from_millis(10));
}

fn axis(dev: &mut uinput::Device, ax: uinput::event::absolute::Position, value: i32) {
    dev.position(&ax, value).unwrap();
    dev.synchronize().unwrap();
    thread::sleep(Duration::from_millis(20));
}

// ── test ──────────────────────────────────────────────────────────────────────

#[test]
fn virtual_gamepad_events_route_through_gilrs() {
    // Skip gracefully when /dev/uinput is unavailable (macOS, restricted CI)
    if !std::path::Path::new("/dev/uinput").exists() {
        eprintln!("SKIP: /dev/uinput not available");
        return;
    }

    let mut vdev = match create_virtual_gamepad() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("SKIP: could not create uinput device: {e}");
            return;
        }
    };

    // Shared event log
    let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let log_cb = Arc::clone(&log);

    let mut driver = GamepadDriver::new().expect("gilrs init");
    driver.set_callback(Box::new(move |_surface: String, ev: ControlEvent| {
        log_cb.lock().unwrap().push(ev.control.clone());
    }));

    // Run driver in a background thread (it blocks)
    thread::spawn(move || driver.run());

    // Give gilrs time to discover the virtual device
    thread::sleep(Duration::from_millis(300));

    // ── inject events ─────────────────────────────────────────────────────────

    // BTN_SOUTH → south (scenario.activate)
    press(&mut vdev, uinput::event::keyboard::Key::ButtonSouth);
    // BTN_TR (RB) → rb (scenario.next +1)
    press(&mut vdev, uinput::event::keyboard::Key::ButtonTR);
    // BTN_TL (LB) → lb (scenario.next -1)
    press(&mut vdev, uinput::event::keyboard::Key::ButtonTL);
    // BTN_MODE (Guide) → guide (audio.mute)
    press(&mut vdev, uinput::event::keyboard::Key::ButtonMode);
    // ABS_RZ high → rt_volume
    axis(&mut vdev, uinput::event::absolute::Position::RZ, 220);

    // Allow events to propagate through gilrs
    thread::sleep(Duration::from_millis(250));

    // ── assertions ────────────────────────────────────────────────────────────

    let controls = log.lock().unwrap().clone();

    assert!(
        controls.contains(&"south".to_string()),
        "Expected 'south', got: {controls:?}"
    );
    assert!(
        controls.contains(&"rb".to_string()),
        "Expected 'rb', got: {controls:?}"
    );
    assert!(
        controls.contains(&"lb".to_string()),
        "Expected 'lb', got: {controls:?}"
    );
    assert!(
        controls.contains(&"guide".to_string()),
        "Expected 'guide', got: {controls:?}"
    );
    assert!(
        controls.contains(&"rt_volume".to_string()),
        "Expected 'rt_volume', got: {controls:?}"
    );
}
