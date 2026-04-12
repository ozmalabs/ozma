// SPDX-License-Identifier: AGPL-3.0-only
//! Integration test: send keypresses to a real QEMU VM and verify via VNC
//! screenshot.
//!
//! Requires:
//!   QEMU_CTRL_SOCKET  — path to QMP control socket  (default: /tmp/vm-ctrl.qmp)
//!   QEMU_INPUT_SOCKET — path to QMP input socket    (default: /tmp/vm-input.qmp)
//!   QEMU_SCREENSHOT   — path to write screenshot PPM (default: /tmp/qmp-test.ppm)
//!
//! The test is skipped automatically when the sockets don't exist (CI without
//! a running QEMU instance).

use std::path::Path;
use std::time::Duration;

use softnode_qmp::hid_to_qmp::KeyboardReportState;
use softnode_qmp::qmp_client::QmpClient;

fn socket_path(env_var: &str, default: &str) -> String {
    std::env::var(env_var).unwrap_or_else(|_| default.to_string())
}

#[tokio::test]
async fn test_send_keypresses_to_qemu() {
    let ctrl_path = socket_path("QEMU_CTRL_SOCKET", "/tmp/vm-ctrl.qmp");
    let input_path = socket_path("QEMU_INPUT_SOCKET", "/tmp/vm-input.qmp");
    let screenshot_path = socket_path("QEMU_SCREENSHOT", "/tmp/qmp-test.ppm");

    // Skip if QEMU is not running
    if !Path::new(&ctrl_path).exists() {
        eprintln!("Skipping integration test: {} not found", ctrl_path);
        return;
    }

    let client = if Path::new(&input_path).exists() {
        QmpClient::new_dual(&ctrl_path, &input_path)
    } else {
        QmpClient::new_single(&ctrl_path)
    };

    client.start().await;
    // Allow time for connection
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert!(
        client.connected().await,
        "QMP client should be connected to QEMU"
    );

    // Query initial VM status
    let status = client.query_status().await;
    assert!(status.is_some(), "query-status should return a result");
    eprintln!("VM status before test: {:?}", status);

    // Type "hello\n" via HID boot reports
    // h=0x0B, e=0x08, l=0x0F, l=0x0F, o=0x12, Enter=0x28
    let mut kbd = KeyboardReportState::new();
    let keys: &[u8] = &[0x0B, 0x08, 0x0F, 0x0F, 0x12, 0x28];

    for &hid in keys {
        // Press key
        let mut report = [0u8; 8];
        report[2] = hid;
        let events = kbd.diff(&report);
        assert!(!events.is_empty(), "press should produce events");
        let ok = client.send_input_events(&events).await;
        assert!(ok, "send_input_events (press) should succeed");
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Release key
        let report = [0u8; 8];
        let events = kbd.diff(&report);
        assert!(!events.is_empty(), "release should produce events");
        let ok = client.send_input_events(&events).await;
        assert!(ok, "send_input_events (release) should succeed");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Allow VM to process input
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Take a screenshot to verify visually
    let ok = client.screendump(&screenshot_path).await;
    assert!(ok, "screendump should succeed");
    assert!(
        Path::new(&screenshot_path).exists(),
        "screenshot file should exist at {}",
        screenshot_path
    );
    eprintln!("Screenshot saved to {}", screenshot_path);

    // Release all keys (cleanup)
    let release_events = kbd.release_all();
    if !release_events.is_empty() {
        client.send_input_events(&release_events).await;
    }

    client.stop().await;
}

#[tokio::test]
async fn test_mouse_click_and_move() {
    let ctrl_path = socket_path("QEMU_CTRL_SOCKET", "/tmp/vm-ctrl.qmp");
    let input_path = socket_path("QEMU_INPUT_SOCKET", "/tmp/vm-input.qmp");

    if !Path::new(&ctrl_path).exists() {
        eprintln!("Skipping integration test: {} not found", ctrl_path);
        return;
    }

    let client = if Path::new(&input_path).exists() {
        QmpClient::new_dual(&ctrl_path, &input_path)
    } else {
        QmpClient::new_single(&ctrl_path)
    };

    client.start().await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert!(client.connected().await, "should be connected");

    use softnode_qmp::hid_to_qmp::MouseReportState;
    let mut mouse = MouseReportState::new();

    // Move to centre of a 1920×1080 display (QMP abs range 0–32767)
    // Centre: x = 32767/2 ≈ 16383, y = 32767/2 ≈ 16383
    let x_lo = (16383u16 & 0xFF) as u8;
    let x_hi = ((16383u16 >> 8) & 0xFF) as u8;
    let y_lo = (16383u16 & 0xFF) as u8;
    let y_hi = ((16383u16 >> 8) & 0xFF) as u8;

    // Move (no buttons, no scroll)
    let report = [0x00, x_lo, x_hi, y_lo, y_hi, 0x00];
    let events = mouse.decode(&report);
    assert!(!events.is_empty());
    client.send_input_events(&events).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Left click
    let report = [0x01, x_lo, x_hi, y_lo, y_hi, 0x00]; // button down
    let events = mouse.decode(&report);
    client.send_input_events(&events).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let report = [0x00, x_lo, x_hi, y_lo, y_hi, 0x00]; // button up
    let events = mouse.decode(&report);
    client.send_input_events(&events).await;

    client.stop().await;
}
