// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Process-level graceful-shutdown test.
//!
//! Starts the real `ozma-node` binary with a synthetic capture source, HLS,
//! an external WebRTC helper stand-in, and the HTTP/UDP HID listeners, then
//! sends SIGTERM and asserts a bounded, successful exit with no surviving
//! helper/ffmpeg children.
//!
//! This is ignored by default because it requires the built binary and
//! `python3` for the helper stand-in. Run it explicitly:
//!
//! ```text
//! OZMA_NODE_SHUTDOWN_TEST=1 \
//!   cargo test -p ozma-node --test shutdown -- --ignored --nocapture
//! ```

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const EXIT_BUDGET: Duration = Duration::from_secs(8);
const CHILD_REAP_BUDGET: Duration = Duration::from_secs(5);

#[test]
#[ignore = "process-level SIGTERM test; run with OZMA_NODE_SHUTDOWN_TEST=1 -- --ignored"]
fn sigterm_exits_the_node_within_the_bound_and_reaps_children() {
    if std::env::var("OZMA_NODE_SHUTDOWN_TEST").as_deref() != Ok("1") {
        eprintln!("OZMA_NODE_SHUTDOWN_TEST is not 1; skipping");
        return;
    }
    if Command::new("python3")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| !status.success())
        .unwrap_or(true)
    {
        eprintln!("python3 is not available; skipping the WebRTC helper leg");
        return;
    }

    let root = temp_root();
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create test root");

    let http_port = free_tcp_port();
    let hid_port = free_udp_port();
    let hls_dir = root.join("hls");
    std::fs::create_dir_all(&hls_dir).expect("create HLS directory");

    let ffmpeg_pid = root.join("ffmpeg.pid");
    let helper_pid = root.join("helper.pid");
    let helper_socket = root.join("webrtc.sock");
    let helper_shm = root.join("frame.bgra");
    let helper_script = root.join("helper.py");
    let log_path = root.join("node.log");
    write_fake_ffmpeg(&root.join("ffmpeg.sh"), &ffmpeg_pid);
    write_fake_helper(&helper_script, &helper_pid);

    let log = std::fs::File::create(&log_path).expect("create node log");
    let binary = env!("CARGO_BIN_EXE_ozma-node");
    let mut command = Command::new(binary);
    command
        .args([
            "--name",
            "shutdown-test",
            "--profile",
            "vm",
            "--cap",
            "video,hid",
            "--http-port",
            &http_port.to_string(),
            "--hid-udp-port",
            &hid_port.to_string(),
            "--capture-provider",
            "fake",
            "--fake-width",
            "160",
            "--fake-height",
            "120",
            "--fake-fps",
            "5",
            "--hls",
            "--hls-dir",
            hls_dir.to_str().expect("HLS path is UTF-8"),
            "--hls-fps",
            "5",
            "--hls-bitrate-kbps",
            "100",
            "--ffmpeg-bin",
            root.join("ffmpeg.sh")
                .to_str()
                .expect("ffmpeg path is UTF-8"),
            "--webrtc-helper",
            "python3",
            "--webrtc-helper-script",
            helper_script.to_str().expect("helper path is UTF-8"),
            "--webrtc-socket",
            helper_socket.to_str().expect("socket path is UTF-8"),
            "--webrtc-shm",
            helper_shm.to_str().expect("shm path is UTF-8"),
            "--webrtc-width",
            "160",
            "--webrtc-height",
            "120",
            "--webrtc-fps",
            "5",
        ])
        .env("RUST_LOG", "info")
        .env("OZMA_TEST_HELPER_PIDFILE", &helper_pid)
        .stdin(Stdio::null())
        .stdout(log.try_clone().expect("clone log"))
        .stderr(log)
        .current_dir(&root);

    let mut child = command.spawn().expect("spawn ozma-node");
    let node_pid = child.id() as i32;

    let result = (|| -> Result<(), String> {
        wait_for_health(http_port, Duration::from_secs(10))
            .map_err(|error| format!("node API never became healthy: {error}"))?;
        wait_for_file(&ffmpeg_pid, Duration::from_secs(5))
            .map_err(|error| format!("HLS ffmpeg was not spawned: {error}"))?;
        wait_for_file(&helper_pid, Duration::from_secs(5))
            .map_err(|error| format!("WebRTC helper was not spawned: {error}"))?;

        let started = Instant::now();
        // SAFETY: the pid came from this child process and it has not been
        // waited on yet.
        let sent = unsafe { libc::kill(node_pid, libc::SIGTERM) };
        if sent != 0 {
            return Err(format!(
                "kill(SIGTERM) failed: {}",
                std::io::Error::last_os_error()
            ));
        }

        let status = wait_for_exit(&mut child, EXIT_BUDGET).map_err(|error| {
            let _ = child.kill();
            let _ = child.wait();
            format!("node did not exit within {EXIT_BUDGET:?}: {error}")
        })?;
        let elapsed = started.elapsed();
        if !status.success() {
            return Err(format!("SIGTERM shutdown exited with {status}"));
        }
        if elapsed > EXIT_BUDGET {
            return Err(format!("shutdown took {elapsed:?}"));
        }

        let helper = read_pid(&helper_pid)?;
        let ffmpeg = read_pid(&ffmpeg_pid)?;
        wait_for_process_exit(helper, CHILD_REAP_BUDGET)
            .map_err(|error| format!("WebRTC helper {helper} survived: {error}"))?;
        wait_for_process_exit(ffmpeg, CHILD_REAP_BUDGET)
            .map_err(|error| format!("HLS ffmpeg {ffmpeg} survived: {error}"))?;
        Ok(())
    })();

    // Never leave a node behind if an assertion failed.
    if child.try_wait().ok().flatten().is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }

    if let Err(error) = result {
        let log = std::fs::read_to_string(&log_path).unwrap_or_default();
        panic!("{error}\n--- node log ---\n{log}");
    }
    let _ = std::fs::remove_dir_all(&root);
}

fn temp_root() -> PathBuf {
    std::env::temp_dir().join(format!("ozma-node-shutdown-test-{}", std::process::id()))
}

fn free_tcp_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind ephemeral TCP port");
    listener.local_addr().expect("local TCP address").port()
}

fn free_udp_port() -> u16 {
    let socket = UdpSocket::bind(("127.0.0.1", 0)).expect("bind ephemeral UDP port");
    socket.local_addr().expect("local UDP address").port()
}

fn wait_for_health(port: u16, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) {
            let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
            let request = b"GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n";
            if stream.write_all(request).is_ok() {
                let mut response = String::new();
                let _ = stream.read_to_string(&mut response);
                if response.starts_with("HTTP/1.1 200") {
                    return Ok(());
                }
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(format!("no 200 /health response on port {port}"))
}

fn wait_for_file(path: &Path, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.is_file() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Err(format!("{} did not appear", path.display()))
}

fn wait_for_exit(child: &mut Child, timeout: Duration) -> Result<std::process::ExitStatus, String> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if Instant::now() >= deadline => return Err("still running".to_owned()),
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(error) => return Err(format!("try_wait failed: {error}")),
        }
    }
}

fn read_pid(path: &Path) -> Result<u32, String> {
    std::fs::read_to_string(path)
        .map_err(|error| format!("read {}: {error}", path.display()))?
        .trim()
        .parse()
        .map_err(|error| format!("parse pid in {}: {error}", path.display()))
}

fn wait_for_process_exit(pid: u32, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    #[cfg(target_os = "linux")]
    {
        let proc_path = format!("/proc/{pid}");
        while Instant::now() < deadline {
            if !Path::new(&proc_path).exists() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Err(format!("process {pid} still exists"))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (pid, deadline);
        Ok(())
    }
}

#[cfg(unix)]
fn write_fake_ffmpeg(path: &Path, pidfile: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let script = format!(
        "#!/bin/sh\necho $$ > '{}'\ncat >/dev/null\n",
        pidfile.display()
    );
    std::fs::write(path, script).expect("write fake ffmpeg");
    let mut permissions = std::fs::metadata(path)
        .expect("fake ffmpeg metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).expect("chmod fake ffmpeg");
}

#[cfg(not(unix))]
fn write_fake_ffmpeg(_path: &Path, _pidfile: &Path) {
    panic!("the shutdown test needs a Unix host");
}

#[cfg(unix)]
fn write_fake_helper(path: &Path, _pidfile: &Path) {
    // The pidfile comes from `OZMA_TEST_HELPER_PIDFILE` in the node's
    // environment, so the helper needs no extra argv that the real supervisor
    // does not pass.
    let script = r#"import argparse, os, socket
p = argparse.ArgumentParser()
for flag in ('--sock', '--shm', '--width', '--height', '--fps', '--audio-source',
             '--audio-backend', '--stun-server', '--turn-server', '--turn-user',
             '--turn-password'):
    p.add_argument(flag)
a, _ = p.parse_known_args()
open(os.environ['OZMA_TEST_HELPER_PIDFILE'], 'w').write(str(os.getpid()))
try:
    os.unlink(a.sock)
except FileNotFoundError:
    pass
server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
server.bind(a.sock)
server.listen(1)
connection, _ = server.accept()
while connection.recv(65536):
    pass
"#;
    std::fs::write(path, script).expect("write fake WebRTC helper");
}

#[cfg(not(unix))]
fn write_fake_helper(_path: &Path, _pidfile: &Path) {
    panic!("the shutdown test needs a Unix host");
}
