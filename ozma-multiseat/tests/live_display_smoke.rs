// SPDX-License-Identifier: AGPL-3.0-only
//!
//! Opt-in live smoke for two private `Xvfb` seat displays.
//!
//! Nothing runs unless `OZMA_MULTISEAT_REAL_DISPLAY=1`. It starts two
//! throwaway seats on the first free numbers at or above `:90`, proves each
//! fake node received a distinct `DISPLAY`, stops both, and verifies the
//! servers and sockets are gone. It never touches the operator's desktop:
//! each server is a private `Xvfb` with `-nolisten tcp`.
//!
//! ```bash
//! OZMA_MULTISEAT_REAL_DISPLAY=1 \
//!   cargo test -p ozma-multiseat --test live_display_smoke -- --ignored --nocapture
//! ```

#![cfg(unix)]

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ozma_multiseat::{
    resolve_program, DisplayOutput, FakeDisplayBackend, FakeInputBackend, HostTopology,
    HttpHealthProbe, MultiseatPlanner, PlannerOptions, ProcessLauncher, ProcessSeatDisplayLauncher,
    ProcessSeatSupervisor, SeatDisplayBackend, SeatDisplayOptions, SeatDisplaySize, SeatState,
    SeatSupervisor, SupervisorOptions,
};

fn enabled() -> bool {
    std::env::var("OZMA_MULTISEAT_REAL_DISPLAY").as_deref() == Ok("1")
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind free port");
    listener.local_addr().expect("local addr").port()
}

fn fake_node_script(dir: &Path) -> PathBuf {
    let path = dir.join("fake-node.py");
    let mut file = std::fs::File::create(&path).expect("create fake node script");
    file.write_all(
        br#"#!/usr/bin/env python3
import http.server, json, os, socketserver, sys

def value(flag):
    for index, arg in enumerate(sys.argv):
        if arg == flag:
            return sys.argv[index + 1]
    return None

http_port = int(value("--http-port") or "7382")
record = value("--record-dir")

if record:
    os.makedirs(record, exist_ok=True)
    with open(os.path.join(record, "display-%d" % http_port), "w") as handle:
        handle.write(os.environ.get("DISPLAY", ""))
    with open(os.path.join(record, "pid-%d" % http_port), "w") as handle:
        handle.write(str(os.getpid()))

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

with Server(("127.0.0.1", http_port), Handler) as server:
    server.serve_forever()
"#,
    )
    .expect("write fake node script");
    let mut permissions = std::fs::metadata(&path)
        .expect("fake node metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&path, permissions).expect("chmod fake node");
    path
}

fn socket_path(number: u32) -> PathBuf {
    PathBuf::from(format!("/tmp/.X11-unix/X{number}"))
}

#[test]
#[ignore = "opt-in live Xvfb smoke: set OZMA_MULTISEAT_REAL_DISPLAY=1"]
fn two_private_displays_start_with_distinct_display_strings_and_stop_cleanly() {
    if !enabled() {
        return;
    }
    assert!(
        resolve_program(Path::new("Xvfb")).is_some(),
        "FAIL: Xvfb is not available on PATH"
    );

    let root = std::env::temp_dir().join(format!(
        "ozma-multiseat-display-smoke-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create smoke dir");
    let record_dir = root.join("records");
    std::fs::create_dir_all(&record_dir).expect("create record dir");
    let node_bin = fake_node_script(&root);

    let display_options = SeatDisplayOptions {
        backend: SeatDisplayBackend::Xvfb,
        size: SeatDisplaySize::new(640, 480).expect("size"),
        number_start: 90,
        number_end: 99,
        ready_timeout: Duration::from_secs(10),
        ready_poll: Duration::from_millis(50),
        parent_display: None,
    };
    let displays = vec![
        DisplayOutput::new(0, "screen-0", 640, 480, 0, 0, ":0"),
        DisplayOutput::new(1, "screen-1", 640, 480, 640, 0, ":0"),
    ];
    let topology = HostTopology::enumerate(
        &FakeDisplayBackend::new(displays),
        &FakeInputBackend::new(Vec::new()),
    )
    .expect("topology");
    let mut plan = topology
        .plan(&MultiseatPlanner::new(PlannerOptions {
            machine_name: "smoke".to_owned(),
            base_udp_port: 7731,
            base_http_port: free_port(),
            node_program: node_bin.display().to_string(),
            x11_display: Some(":0".to_owned()),
            ..PlannerOptions::default()
        }))
        .expect("plan");
    for seat in &mut plan.seats {
        seat.command.args.push("--record-dir".to_owned());
        seat.command.args.push(record_dir.display().to_string());
    }
    assert_eq!(plan.seat_count(), 2);

    let options = SupervisorOptions {
        node_bin: Some(node_bin.display().to_string()),
        readiness_timeout: Duration::from_secs(10),
        readiness_poll: Duration::from_millis(50),
        restart_backoff: Duration::from_millis(100),
        restart_backoff_max: Duration::from_millis(500),
        healthy_uptime: Duration::from_secs(30),
        dry_run: false,
        apply_ownership: false,
        apply_routing: false,
        input_isolation: false,
        input_targeting: Default::default(),
        seat_display: display_options.clone(),
        drm_leases: Default::default(),
    };
    let mut supervisor = ProcessSeatSupervisor::with_parts(
        options,
        std::sync::Arc::new(ProcessLauncher),
        std::sync::Arc::new(HttpHealthProbe::default()),
    )
    .with_display_launcher(Box::new(ProcessSeatDisplayLauncher::new(display_options)));

    let ninety_busy = socket_path(90).exists() || socket_path(91).exists();
    let mut displays_seen = Vec::new();
    for seat in &plan.seats {
        let status = supervisor.start(seat).expect("live seat start");
        assert_eq!(status.state, SeatState::Running);
        assert!(
            status.display.display_ready,
            "display not ready: {status:?}"
        );
        let display = status.display.display.clone().expect("display string");
        let number: u32 = display.trim_start_matches(':').parse().expect("number");
        assert!(socket_path(number).exists());
        displays_seen.push((display, status.display.display_pid));
        println!(
            "started {} display={} pid={:?}",
            seat.config.id,
            displays_seen.last().unwrap().0,
            status.display.display_pid
        );
    }
    displays_seen.sort();
    assert_eq!(displays_seen.len(), 2);
    assert_ne!(displays_seen[0].0, displays_seen[1].0);
    if !ninety_busy {
        assert_eq!(
            displays_seen
                .iter()
                .map(|(display, _)| display.clone())
                .collect::<Vec<_>>(),
            vec![":90".to_owned(), ":91".to_owned()],
            "the two seats must receive :90 and :91 when they are free"
        );
    }

    // Each node wrote its own DISPLAY from the environment inherited at spawn.
    let mut outputs: Vec<String> = Vec::new();
    for seat in &plan.seats {
        let record = record_dir.join(format!("display-{}", seat.config.http_port));
        assert!(
            record.exists(),
            "fake node for {} did not record DISPLAY",
            seat.config.id
        );
        outputs.push(std::fs::read_to_string(&record).expect("read DISPLAY"));
    }
    outputs.sort();
    println!("recorded DISPLAY values: {outputs:?}");
    assert_eq!(outputs.len(), 2);
    assert_ne!(outputs[0], outputs[1]);
    assert!(outputs.iter().all(|display| display.starts_with(':')));

    let pids: Vec<u32> = displays_seen.iter().filter_map(|(_, pid)| *pid).collect();
    let numbers: Vec<u32> = displays_seen
        .iter()
        .map(|(display, _)| display.trim_start_matches(':').parse().expect("number"))
        .collect();

    let stopped = supervisor.stop_all();
    assert_eq!(stopped.len(), 2);
    for status in &stopped {
        assert_eq!(status.state, SeatState::Stopped, "stop status: {status:?}");
        assert_eq!(status.display.display, None);
        assert!(!status.display.display_ready);
    }

    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        let sockets_gone = numbers.iter().all(|number| !socket_path(*number).exists());
        let pids_gone = pids
            .iter()
            .all(|pid| !Path::new(&format!("/proc/{pid}")).exists());
        if sockets_gone && pids_gone {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    for number in &numbers {
        assert!(
            !socket_path(*number).exists(),
            "display socket :{number} leaked"
        );
    }
    for pid in &pids {
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists(),
            "X server pid {pid} leaked"
        );
    }

    println!(
        "PASS: seats={:?} displays={:?} pids={:?} stopped cleanly",
        plan.seats
            .iter()
            .map(|seat| &seat.config.id)
            .collect::<Vec<_>>(),
        numbers,
        pids
    );
    let _ = std::fs::remove_dir_all(&root);
}
