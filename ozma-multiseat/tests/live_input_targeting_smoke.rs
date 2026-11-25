// SPDX-License-Identifier: AGPL-3.0-only
//!
//! Opt-in live smoke for the real `xinput` targeter on a throwaway `Xvfb`.
//!
//! Nothing runs unless `OZMA_MULTISEAT_REAL_TARGETING=1`. It starts a private
//! `Xvfb` on the first free display at or above `:90`, asks the real
//! `ProcessSeatInputTargeter` to target an existing core device, proves a
//! missing own device is a typed error, and terminates the server. It never
//! touches the operator's desktop and creates no evdev/uinput device.
//!
//! ```bash
//! OZMA_MULTISEAT_REAL_TARGETING=1 \
//!   cargo test -p ozma-multiseat --test live_input_targeting_smoke \
//!   -- --ignored --nocapture
//! ```

#![cfg(unix)]

use std::process::Command;
use std::time::{Duration, Instant};

use ozma_multiseat::{
    parse_xinput_list, ProcessSeatDisplayLauncher, ProcessSeatInputTargeter, SeatDisplayBackend,
    SeatDisplayLauncher, SeatDisplayOptions, SeatDisplayRequest, SeatDisplaySize,
    SeatInputTargetRequest, SeatInputTargeter, DEFAULT_DISPLAY_READY_TIMEOUT,
};

fn enabled() -> bool {
    matches!(
        std::env::var("OZMA_MULTISEAT_REAL_TARGETING").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

#[test]
#[ignore = "opt-in live Xvfb + xinput smoke: set OZMA_MULTISEAT_REAL_TARGETING=1"]
fn real_targeter_lists_enables_and_reports_a_missing_device_on_a_throwaway_display() {
    if !enabled() {
        eprintln!("skipped: set OZMA_MULTISEAT_REAL_TARGETING=1");
        return;
    }

    let options = SeatDisplayOptions {
        backend: SeatDisplayBackend::Xvfb,
        size: SeatDisplaySize::new(1280, 800).expect("size"),
        number_start: 90,
        number_end: 99,
        ready_timeout: DEFAULT_DISPLAY_READY_TIMEOUT,
        ready_poll: Duration::from_millis(100),
        parent_display: None,
    };
    let launcher = ProcessSeatDisplayLauncher::new(options);
    let request = SeatDisplayRequest {
        seat_id: "live-targeting".to_owned(),
        backend: SeatDisplayBackend::Xvfb,
        size: SeatDisplaySize::new(1280, 800).expect("size"),
        parent_display: None,
    };
    let mut display = launcher.spawn(&request).expect("start throwaway Xvfb");
    let display_name = display.display().to_owned();

    let deadline = Instant::now() + DEFAULT_DISPLAY_READY_TIMEOUT;
    while !display.is_ready().unwrap_or(false) {
        if Instant::now() >= deadline {
            let _ = display.terminate();
            panic!("{display_name} never answered the X11 handshake");
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    println!("started throwaway display {display_name}");
    let xauthority = display.xauthority().map(|path| path.to_path_buf());

    let mut list_command = Command::new("xinput");
    list_command
        .args(["list", "--short"])
        .env("DISPLAY", &display_name)
        .env_remove("XAUTHORITY");
    if let Some(path) = xauthority.as_ref() {
        list_command.env("XAUTHORITY", path);
    }
    let listed = list_command.output().expect("run xinput");
    assert!(
        listed.status.success(),
        "xinput list failed: {}",
        String::from_utf8_lossy(&listed.stderr)
    );
    let devices = parse_xinput_list(&String::from_utf8_lossy(&listed.stdout));
    let own = devices
        .first()
        .expect("Xvfb exposes at least one XInput device");
    println!(
        "xinput sees {} device(s); targeting {:?} (id {})",
        devices.len(),
        own.name,
        own.id
    );

    let mut targeter = ProcessSeatInputTargeter::new();
    let request = SeatInputTargetRequest {
        seat_id: "live-targeting".to_owned(),
        display: display_name.clone(),
        xauthority: xauthority.clone(),
        devices: vec![own.name.clone()],
    };
    let targeting = targeter
        .apply(&request)
        .expect("real xinput targeting on throwaway display");
    assert_eq!(targeting.enabled, vec![own.name.clone()]);
    assert_eq!(targeting.display, display_name);
    assert!(targeter.status_for("live-targeting").is_some());

    let missing = targeter.apply(&SeatInputTargetRequest {
        seat_id: "live-missing".to_owned(),
        display: display_name.clone(),
        xauthority: xauthority.clone(),
        devices: vec!["ozma-seat-live-missing-kbd-event0".to_owned()],
    });
    let error = missing.expect_err("a device Xvfb does not expose must fail");
    println!("missing-device typed error: {error}");
    assert!(error.to_string().contains("not visible"));

    targeter
        .release("live-targeting")
        .expect("release targeting");
    assert!(targeter.status_for("live-targeting").is_none());

    display.terminate().expect("stop throwaway Xvfb");
    println!("PASS: {display_name} targeted and stopped cleanly");
}
