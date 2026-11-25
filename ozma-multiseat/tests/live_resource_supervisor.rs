// SPDX-License-Identifier: AGPL-3.0-only
//!
//! Opt-in live supervisor resource smoke on a throwaway X server.
//!
//! Nothing runs unless `OZMA_MULTISEAT_REAL=1` and
//! `OZMA_MULTISEAT_SUPERVISOR_APPLY=1`. It applies real `xrandr` ownership and
//! a real `xinput map-to-output` route on the explicit display, starts a tiny
//! fake HTTP node, stops it, and verifies that both resources were released.
//! Never point this at the operator's desktop:
//!
//! ```bash
//! Xvfb :97 -screen 0 1280x800x24 &
//! OZMA_MULTISEAT_REAL=1 OZMA_MULTISEAT_SUPERVISOR_APPLY=1 DISPLAY=:97 \
//!   cargo test -p ozma-multiseat --test live_resource_supervisor \
//!   -- --ignored --nocapture
//! ```

#![cfg(unix)]

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

use ozma_multiseat::{
    DisplayOutput, FakeDeviceNameResolver, FakeDisplayBackend, FakeInputBackend, HostTopology,
    HttpHealthProbe, InputDevice, InputDeviceKind, MultiseatPlanner, PlannerOptions,
    ProcessCommandRunner, ProcessLauncher, ProcessSeatSupervisor, SeatState, SeatSupervisor,
    SupervisorOptions, XInputRouter, XrandrOwnershipController,
};

fn enabled() -> bool {
    std::env::var("OZMA_MULTISEAT_REAL").as_deref() == Ok("1")
        && std::env::var("OZMA_MULTISEAT_SUPERVISOR_APPLY").as_deref() == Ok("1")
}

fn display() -> String {
    std::env::var("OZMA_MULTISEAT_XDISPLAY")
        .or_else(|_| std::env::var("DISPLAY"))
        .expect("set DISPLAY or OZMA_MULTISEAT_XDISPLAY")
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind free port");
    listener.local_addr().expect("local addr").port()
}

fn fake_node_script() -> (PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!("ozma-multiseat-live-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create fake node dir");
    let path = dir.join("fake-node.py");
    let mut file = std::fs::File::create(&path).expect("create fake node script");
    file.write_all(
        br#"#!/usr/bin/env python3
import http.server, json, socketserver, sys

def http_port(argv):
    for index, arg in enumerate(argv):
        if arg == "--http-port":
            return int(argv[index + 1])
    return 7382

class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = json.dumps({"ok": True, "fake": True}).encode()
        self.send_response(200 if self.path in ("/health", "/status") else 404)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def log_message(self, *args):
        pass

class Server(socketserver.TCPServer):
    allow_reuse_address = True

with Server(("127.0.0.1", http_port(sys.argv)), Handler) as server:
    server.serve_forever()
"#,
    )
    .expect("write fake node script");
    let mut permissions = std::fs::metadata(&path)
        .expect("fake node metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&path, permissions).expect("chmod fake node");
    (dir, path)
}

#[test]
#[ignore = "opt-in throwaway Xvfb: set OZMA_MULTISEAT_REAL=1 and OZMA_MULTISEAT_SUPERVISOR_APPLY=1"]
fn live_supervisor_applies_and_releases_ownership_and_routing() {
    if !enabled() {
        return;
    }

    let display = display();
    let (temp_dir, node_bin) = fake_node_script();
    let display_output = DisplayOutput::new(0, "screen", 1280, 800, 0, 0, &display);
    let inputs = vec![InputDevice {
        path: "/dev/input/event0".to_owned(),
        name: "xvfb-mouse".to_owned(),
        kind: InputDeviceKind::Mouse,
        usb_hub: None,
        sysfs_path: None,
    }];

    let topology = HostTopology::enumerate(
        &FakeDisplayBackend::new(vec![display_output]),
        &FakeInputBackend::new(inputs),
    )
    .expect("topology");
    let plan = topology
        .plan(&MultiseatPlanner::new(PlannerOptions {
            machine_name: "xvfb".to_owned(),
            base_udp_port: 7631,
            base_http_port: free_port(),
            x11_display: Some(display.clone()),
            ..PlannerOptions::default()
        }))
        .expect("plan");
    assert_eq!(plan.seat_count(), 1);

    let resolver = FakeDeviceNameResolver::new().with_name("/dev/input/event0", "Xvfb mouse");
    let supervisor = ProcessSeatSupervisor::with_parts(
        SupervisorOptions {
            node_bin: Some(node_bin.display().to_string()),
            readiness_timeout: Duration::from_secs(10),
            readiness_poll: Duration::from_millis(100),
            restart_backoff: Duration::from_millis(100),
            restart_backoff_max: Duration::from_millis(500),
            healthy_uptime: Duration::from_secs(30),
            dry_run: false,
            apply_ownership: true,
            apply_routing: true,
            input_isolation: false,
            input_targeting: Default::default(),
            seat_display: Default::default(),
            seat_input: Default::default(),
            drm_leases: Default::default(),
        },
        std::sync::Arc::new(ProcessLauncher),
        std::sync::Arc::new(HttpHealthProbe::default()),
    )
    .with_ownership_controller(Box::new(XrandrOwnershipController::new(Box::new(
        ProcessCommandRunner,
    ))))
    .with_routing_controller(Box::new(XInputRouter::with_resolver(
        Box::new(ProcessCommandRunner),
        Box::new(resolver),
    )));
    let mut supervisor = supervisor;

    let started = supervisor.start(&plan.seats[0]).expect("live start");
    assert_eq!(started.state, SeatState::Running);
    assert!(started.resources.ownership_applied);
    assert!(started.resources.routing_applied);
    println!("started: {started:?}");

    let stopped = supervisor.stop("xvfb-seat-0").expect("live stop");
    assert_eq!(stopped.state, SeatState::Stopped);
    assert!(!stopped.resources.ownership_applied);
    assert!(!stopped.resources.routing_applied);
    assert!(stopped.resources.ownership_released);
    assert!(stopped.resources.routing_released);
    assert!(
        stopped.resources.errors.is_empty(),
        "release errors: {:?}",
        stopped.resources.errors
    );
    println!("stopped: {stopped:?}");

    let _ = std::fs::remove_dir_all(temp_dir);
}
