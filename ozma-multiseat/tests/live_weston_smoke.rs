// SPDX-License-Identifier: AGPL-3.0-only
//!
//! Opt-in live smoke for two throwaway headless-Weston seat compositors.
//!
//! Nothing runs unless `OZMA_MULTISEAT_REAL_WAYLAND=1`. Each seat gets its own
//! private `XDG_RUNTIME_DIR`, generated `weston.ini`, and `WAYLAND_DISPLAY`
//! socket. A real Wayland client (`wayland-info`) runs against each socket; a
//! cross-runtime client connection must fail. Both compositors are then stopped
//! and their runtime directories, sockets, and pids must be gone.
//!
//! ```bash
//! OZMA_MULTISEAT_REAL_WAYLAND=1 \
//!   cargo test -p ozma-multiseat --test live_weston_smoke -- --ignored --nocapture
//! ```
//!
//! It never opens DRM, mode-sets a GPU, or connects to the operator's real
//! Wayland/X session.

#![cfg(unix)]

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use ozma_multiseat::{
    resolve_program, ProcessSeatDisplayLauncher, SeatDisplayBackend, SeatDisplayHandle,
    SeatDisplayLauncher, SeatDisplayOptions, SeatDisplayRequest, SeatDisplaySize,
};

fn enabled() -> bool {
    std::env::var("OZMA_MULTISEAT_REAL_WAYLAND").as_deref() == Ok("1")
}

fn wait_ready(handle: &mut dyn SeatDisplayHandle, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(Some(exit)) = handle.try_wait() {
            panic!(
                "compositor {} exited before readiness: {exit}",
                handle.display()
            );
        }
        match handle.is_ready() {
            Ok(true) => return true,
            Ok(false) => {}
            Err(error) => {
                println!(
                    "readiness probe for {} not ready yet: {error}",
                    handle.display()
                );
            }
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn wayland_info(runtime: &Path, display: &str) -> std::process::Output {
    Command::new("wayland-info")
        .env("XDG_RUNTIME_DIR", runtime)
        .env("WAYLAND_DISPLAY", display)
        .env_remove("DISPLAY")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run wayland-info")
}

#[test]
#[ignore = "opt-in live Weston smoke: set OZMA_MULTISEAT_REAL_WAYLAND=1"]
fn two_headless_weston_seats_have_distinct_sockets_and_clean_up() {
    if !enabled() {
        return;
    }
    assert!(
        resolve_program(Path::new("weston")).is_some(),
        "FAIL: weston is not available on PATH"
    );
    assert!(
        resolve_program(Path::new("wayland-info")).is_some(),
        "FAIL: wayland-info is not available on PATH"
    );

    let options = SeatDisplayOptions {
        backend: SeatDisplayBackend::Weston,
        size: SeatDisplaySize::new(640, 480).expect("size"),
        number_start: 90,
        number_end: 199,
        ready_timeout: Duration::from_secs(15),
        ready_poll: Duration::from_millis(50),
        parent_display: None,
        xorg_config: None,
    };
    let launcher = ProcessSeatDisplayLauncher::new(options);

    let mut seats = Vec::new();
    for index in 0..2 {
        let request = SeatDisplayRequest {
            seat_id: format!("smoke-seat-{index}"),
            backend: SeatDisplayBackend::Weston,
            size: SeatDisplaySize::new(640, 480).expect("size"),
            parent_display: None,
        };
        let mut handle = launcher.spawn(&request).expect("spawn headless weston");
        assert!(
            wait_ready(handle.as_mut(), Duration::from_secs(15)),
            "compositor for {} never became ready; pid={:?}",
            request.seat_id,
            handle.pid()
        );
        let env = handle.wayland().expect("wayland env").clone();
        assert!(
            env.socket.exists(),
            "socket {} missing",
            env.socket.display()
        );
        println!(
            "started {} display={} socket={} pid={:?}",
            request.seat_id,
            handle.display(),
            env.socket.display(),
            handle.pid()
        );
        seats.push((request.seat_id, handle, env));
    }

    let names: Vec<String> = seats
        .iter()
        .map(|(_, _, env)| env.wayland_display.clone())
        .collect();
    let sockets: Vec<_> = seats.iter().map(|(_, _, env)| env.socket.clone()).collect();
    let runtimes: Vec<_> = seats
        .iter()
        .map(|(_, _, env)| env.xdg_runtime_dir.clone())
        .collect();
    assert_ne!(names[0], names[1], "display names must be distinct");
    assert_ne!(sockets[0], sockets[1], "sockets must be distinct");
    assert_ne!(runtimes[0], runtimes[1], "runtime dirs must be distinct");

    // Every seat answers a real Wayland client round-trip with its own env.
    for (seat_id, _, env) in &seats {
        let output = wayland_info(&env.xdg_runtime_dir, &env.wayland_display);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "wayland-info failed for {seat_id}: {stderr}"
        );
        assert!(
            stdout.contains("interface: 'wl_compositor'"),
            "no wl_compositor for {seat_id}: {stdout}"
        );
        assert!(
            stdout.contains("name: 'headless'"),
            "unexpected output for {seat_id}: {stdout}"
        );
        println!(
            "{seat_id} client connected: {} bytes of protocol output",
            stdout.len()
        );
    }

    // A client using seat A's private runtime cannot reach seat B's socket.
    let cross = wayland_info(&seats[0].2.xdg_runtime_dir, &seats[1].2.wayland_display);
    assert!(
        !cross.status.success(),
        "cross-runtime connection unexpectedly succeeded: {}",
        String::from_utf8_lossy(&cross.stdout)
    );
    println!(
        "cross-runtime isolation confirmed: {} -> {} refused ({})",
        seats[0].2.xdg_runtime_dir.display(),
        seats[1].2.wayland_display,
        String::from_utf8_lossy(&cross.stderr).trim()
    );

    let pids: Vec<u32> = seats
        .iter()
        .filter_map(|(_, handle, _)| handle.pid())
        .collect();
    let stopped_names: Vec<String> = seats
        .iter()
        .map(|(_, handle, _)| handle.display().to_owned())
        .collect();

    for (_, handle, _) in seats.iter_mut() {
        handle.terminate().expect("stop compositor");
    }

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let sockets_gone = sockets.iter().all(|socket| !socket.exists());
        let pids_gone = pids
            .iter()
            .all(|pid| !Path::new(&format!("/proc/{pid}")).exists());
        let dirs_gone = runtimes.iter().all(|runtime| !runtime.exists());
        if sockets_gone && pids_gone && dirs_gone {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    for socket in &sockets {
        assert!(!socket.exists(), "socket {} leaked", socket.display());
    }
    for runtime in &runtimes {
        assert!(
            !runtime.exists(),
            "runtime directory {} leaked",
            runtime.display()
        );
    }
    for pid in &pids {
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists(),
            "weston pid {pid} leaked"
        );
    }

    println!(
        "PASS: displays={names:?} sockets={:?} pids={pids:?} stopped cleanly",
        sockets
            .iter()
            .map(|socket| socket.display().to_string())
            .collect::<Vec<_>>()
    );
    println!("stopped compositors: {stopped_names:?}");
}
