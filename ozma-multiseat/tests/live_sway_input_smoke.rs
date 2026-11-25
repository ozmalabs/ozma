// SPDX-License-Identifier: AGPL-3.0-only
//!
//! Opt-in live proof that two headless sway seats accept synthetic input only
//! through their own Wayland sockets.
//!
//! Nothing runs unless `OZMA_MULTISEAT_REAL_SWAY=1`. Two throwaway sway
//! compositors are started with private runtime directories. A minimal Wayland
//! keyboard probe maps one toplevel in each compositor. The real
//! `WaylandSeatInputInjector` types `a` into seat A and `b` into seat B; the
//! test requires A to see keycode 38 only and B to see keycode 56 only.
//!
//! ```bash
//! OZMA_MULTISEAT_REAL_SWAY=1 \
//!   cargo test -p ozma-multiseat --test live_sway_input_smoke \
//!   -- --ignored --nocapture
//! ```
//!
//! It never opens DRM, creates uinput, or connects to the operator's real
//! Wayland/X session.

#![cfg(unix)]

use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use ozma_multiseat::{
    resolve_program, ProcessSeatDisplayLauncher, SeatDisplayBackend, SeatDisplayHandle,
    SeatDisplayLauncher, SeatDisplayOptions, SeatDisplayRequest, SeatDisplaySize,
    SeatInputInjector, SeatInputRequest, WaylandSeatInputInjector, BTN_LEFT,
};

const SEAT_A: &str = "sway-input-seat-0";
const SEAT_B: &str = "sway-input-seat-1";

fn enabled() -> bool {
    std::env::var("OZMA_MULTISEAT_REAL_SWAY").as_deref() == Ok("1")
}

fn wait_ready(handle: &mut dyn SeatDisplayHandle, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(Some(exit)) = handle.try_wait() {
            panic!(
                "sway display {} exited before readiness: {exit}",
                handle.display()
            );
        }
        match handle.is_ready() {
            Ok(true) => return true,
            Ok(false) => {}
            Err(error) => println!(
                "readiness probe for {} not ready yet: {error}",
                handle.display()
            ),
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn spawn_probe(runtime: &Path, display: &str, log: &Path) -> Child {
    let stdout = File::create(log).expect("create probe log");
    let stderr = File::create(log.with_extension("stderr")).expect("create probe stderr");
    Command::new(env!("CARGO_BIN_EXE_ozma-wayland-key-probe"))
        .env("XDG_RUNTIME_DIR", runtime)
        .env("WAYLAND_DISPLAY", display)
        .env_remove("DISPLAY")
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .expect("spawn Wayland key probe")
}

fn read_log(path: &Path) -> String {
    let mut text = String::new();
    if let Ok(mut file) = File::open(path) {
        let _ = file.read_to_string(&mut text);
    }
    text
}

fn wait_for(path: &Path, needle: &str, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    loop {
        let text = read_log(path);
        if text.contains(needle) {
            return text;
        }
        if Instant::now() >= deadline {
            return text;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn seat_log(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "ozma-live-sway-input-{}-{name}.log",
        std::process::id()
    ))
}

#[test]
#[ignore = "opt-in live sway input smoke: set OZMA_MULTISEAT_REAL_SWAY=1"]
fn two_headless_sway_seats_isolate_injected_keys() {
    if !enabled() {
        return;
    }
    assert!(
        resolve_program(Path::new("sway")).is_some(),
        "FAIL: sway is not available on PATH"
    );

    let launcher = ProcessSeatDisplayLauncher::new(SeatDisplayOptions {
        backend: SeatDisplayBackend::Sway,
        size: SeatDisplaySize::new(1280, 800).expect("size"),
        number_start: 90,
        number_end: 99,
        ready_timeout: Duration::from_secs(10),
        ready_poll: Duration::from_millis(50),
        ..SeatDisplayOptions::default()
    });

    let mut displays: Vec<Box<dyn SeatDisplayHandle>> = Vec::new();
    for seat in [SEAT_A, SEAT_B] {
        let mut handle = launcher
            .spawn(&SeatDisplayRequest {
                seat_id: seat.to_owned(),
                backend: SeatDisplayBackend::Sway,
                size: SeatDisplaySize::new(1280, 800).expect("size"),
                parent_display: None,
            })
            .expect("spawn headless sway");
        assert!(
            wait_ready(handle.as_mut(), Duration::from_secs(10)),
            "FAIL: sway display {} did not become ready",
            handle.display()
        );
        let env = handle.wayland().expect("sway wayland env");
        println!(
            "started {seat} display={} runtime={} socket={} pid={:?}",
            handle.display(),
            env.xdg_runtime_dir.display(),
            env.socket.display(),
            handle.pid()
        );
        displays.push(handle);
    }

    let log_a = seat_log("a");
    let log_b = seat_log("b");
    let env_a = displays[0].wayland().expect("seat A env").clone();
    let env_b = displays[1].wayland().expect("seat B env").clone();
    let mut probe_a = spawn_probe(&env_a.xdg_runtime_dir, &env_a.wayland_display, &log_a);
    let mut probe_b = spawn_probe(&env_b.xdg_runtime_dir, &env_b.wayland_display, &log_b);

    // The headless sway seat starts with no libinput devices and therefore no
    // keyboard capability. Creating the virtual keyboard first makes the seat
    // advertise a keyboard and focus the probe; only then can injected keys be
    // delivered.
    let mut injector = WaylandSeatInputInjector::new();
    let caps_a = injector
        .connect(&SeatInputRequest {
            seat_id: SEAT_A.to_owned(),
            wayland: env_a.clone(),
        })
        .expect("connect seat A injector");
    let caps_b = injector
        .connect(&SeatInputRequest {
            seat_id: SEAT_B.to_owned(),
            wayland: env_b.clone(),
        })
        .expect("connect seat B injector");
    assert!(
        caps_a.is_complete(),
        "seat A did not expose both virtual protocols: {caps_a:?}"
    );
    assert!(
        caps_b.is_complete(),
        "seat B did not expose both virtual protocols: {caps_b:?}"
    );

    let focused_a = wait_for(&log_a, "ENTER", Duration::from_secs(10));
    let focused_b = wait_for(&log_b, "ENTER", Duration::from_secs(10));
    assert!(
        focused_a.contains("ENTER"),
        "FAIL: seat A probe never received keyboard focus:\n{focused_a}"
    );
    assert!(
        focused_b.contains("ENTER"),
        "FAIL: seat B probe never received keyboard focus:\n{focused_b}"
    );

    injector
        .inject_text(SEAT_A, "a")
        .expect("inject a into seat A");
    injector
        .inject_text(SEAT_B, "b")
        .expect("inject b into seat B");
    injector
        .pointer_motion(SEAT_A, 12.0, -4.0)
        .expect("pointer motion on seat A");
    injector
        .pointer_button(SEAT_A, BTN_LEFT, ozma_multiseat::SeatKeyState::Pressed)
        .expect("pointer button on seat A");
    injector
        .pointer_button(SEAT_A, BTN_LEFT, ozma_multiseat::SeatKeyState::Released)
        .expect("pointer release on seat A");

    let text_a = wait_for(&log_a, "KEY 38", Duration::from_secs(5));
    let text_b = wait_for(&log_b, "KEY 56", Duration::from_secs(5));
    // Give a cross-routed event time to appear before asserting absence.
    std::thread::sleep(Duration::from_millis(300));
    let final_a = read_log(&log_a);
    let final_b = read_log(&log_b);

    println!("seat A protocol caps: {caps_a:?}");
    println!("seat B protocol caps: {caps_b:?}");
    println!("seat A events:\n{text_a}");
    println!("seat B events:\n{text_b}");

    assert!(
        final_a.contains("KEY 38"),
        "FAIL: seat A did not receive keycode 38 (a):\n{final_a}"
    );
    assert!(
        final_b.contains("KEY 56"),
        "FAIL: seat B did not receive keycode 56 (b):\n{final_b}"
    );
    assert!(
        !final_a.contains("KEY 56"),
        "FAIL: seat A received seat B's keycode 56:\n{final_a}"
    );
    assert!(
        !final_b.contains("KEY 38"),
        "FAIL: seat B received seat A's keycode 38:\n{final_b}"
    );

    injector.disconnect(SEAT_A);
    injector.disconnect(SEAT_B);
    let _ = probe_a.kill();
    let _ = probe_b.kill();
    let _ = probe_a.wait();
    let _ = probe_b.wait();

    let sockets: Vec<PathBuf> = displays
        .iter()
        .map(|handle| handle.wayland().expect("env").socket.clone())
        .collect();
    for handle in &mut displays {
        handle.terminate().expect("terminate sway");
    }
    for socket in &sockets {
        assert!(!socket.exists(), "socket was not cleaned up: {socket:?}");
    }
    let _ = fs::remove_file(&log_a);
    let _ = fs::remove_file(log_a.with_extension("stderr"));
    let _ = fs::remove_file(&log_b);
    let _ = fs::remove_file(log_b.with_extension("stderr"));

    println!(
        "PASS: seat A received keycode 38 only, seat B received keycode 56 only; \
         displays={:?}",
        sockets
    );
}
