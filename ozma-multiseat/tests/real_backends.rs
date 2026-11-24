// SPDX-License-Identifier: AGPL-3.0-only
//!
//! Opt-in checks against the real host. These are ignored by default; run with
//! `OZMA_MULTISEAT_REAL=1 cargo test -p ozma-multiseat --test real_backends
//! -- --ignored --nocapture`.

use ozma_multiseat::{detect_drm, SysfsInputBackend, XrandrDisplayBackend};

fn opted_in() -> bool {
    std::env::var("OZMA_MULTISEAT_REAL").as_deref() == Ok("1")
}

#[test]
#[ignore = "opt-in real host: set OZMA_MULTISEAT_REAL=1"]
fn real_xrandr_enumeration() {
    if !opted_in() {
        eprintln!("skipped: set OZMA_MULTISEAT_REAL=1");
        return;
    }
    let display = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".to_owned());
    let detection = XrandrDisplayBackend::detect(&display);
    assert!(
        detection.available,
        "xrandr detection failed: {detection:?}"
    );

    let backend = XrandrDisplayBackend::new(&display);
    let displays = backend.enumerate().expect("xrandr enumeration");
    eprintln!("display {display}: {} output(s)", displays.len());
    for display in displays {
        eprintln!(
            "  [{}] {} {} primary={}",
            display.index,
            display.name,
            display.geometry(),
            display.primary
        );
    }
}

#[test]
#[ignore = "opt-in real host: set OZMA_MULTISEAT_REAL=1"]
fn real_sysfs_input_enumeration_is_read_only() {
    if !opted_in() {
        eprintln!("skipped: set OZMA_MULTISEAT_REAL=1");
        return;
    }
    let backend = SysfsInputBackend::new();
    let devices = backend.enumerate().expect("sysfs enumeration");
    eprintln!("{} classified input device(s)", devices.len());
    for device in devices {
        eprintln!(
            "  {} {:?} hub={:?}",
            device.path, device.kind, device.usb_hub
        );
    }
}

#[test]
#[ignore = "opt-in real host: set OZMA_MULTISEAT_REAL=1"]
fn real_drm_connector_detection() {
    if !opted_in() {
        eprintln!("skipped: set OZMA_MULTISEAT_REAL=1");
        return;
    }
    let detection = detect_drm();
    eprintln!(
        "drm available={} connectors={} reason={:?}",
        detection.available,
        detection.connectors.len(),
        detection.reason
    );
    for connector in &detection.connectors {
        eprintln!(
            "  {}-{} {:?} modes={}",
            connector.card,
            connector.connector,
            connector.status,
            connector.modes.len()
        );
    }
}
