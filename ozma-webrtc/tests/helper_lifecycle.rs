// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Process-level lifecycle test: the real binary creates its socket, answers
//! signaling messages with the node's expected shapes, and exits on SIGTERM
//! leaving no socket behind.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_for_socket(path: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if UnixStream::connect(path).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    false
}

fn exchange(
    reader: &mut BufReader<UnixStream>,
    writer: &mut UnixStream,
    message: &str,
) -> serde_json::Value {
    writer.write_all(message.as_bytes()).expect("write");
    writer.write_all(b"\n").expect("newline");
    writer.flush().expect("flush");
    let mut line = String::new();
    reader.read_line(&mut line).expect("read response");
    serde_json::from_str(line.trim()).unwrap_or_else(|error| {
        panic!("response `{line}` is not JSON: {error}");
    })
}

#[test]
fn helper_serves_signaling_and_stops_cleanly() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("signal.sock");
    let shm = dir.path().join("frame.bgra");

    let child = Command::new(env!("CARGO_BIN_EXE_ozma-webrtc-native"))
        .args([
            "--sock",
            sock.to_str().unwrap(),
            "--shm",
            shm.to_str().unwrap(),
            "--width",
            "64",
            "--height",
            "48",
            "--fps",
            "10",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn helper");
    let pid = child.id();
    let mut guard = ChildGuard(child);

    assert!(
        wait_for_socket(&sock, Duration::from_secs(10)),
        "helper did not create {}",
        sock.display()
    );
    let stream = UnixStream::connect(&sock).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("read timeout");
    let mut writer = stream.try_clone().expect("clone");
    let mut reader = BufReader::new(stream);

    // Malformed JSON is a typed error, not a crash.
    let response = exchange(&mut reader, &mut writer, "not json");
    assert_eq!(response["type"], "error");
    assert!(response["error"].as_str().is_some());

    // Bitrate is clamped to the helper's documented range.
    let response = exchange(
        &mut reader,
        &mut writer,
        r#"{"type":"bitrate","bitrate":100000}"#,
    );
    assert_eq!(response["ok"], true);
    assert_eq!(response["bitrate"], 500_000u64);

    let response = exchange(
        &mut reader,
        &mut writer,
        r#"{"type":"bitrate","bitrate":60000000}"#,
    );
    assert_eq!(response["bitrate"], 50_000_000u64);

    // A candidate with no live session is an explicit failure.
    let response = exchange(
        &mut reader,
        &mut writer,
        r#"{"type":"add-ice-candidate","session":"nope","candidate":"candidate:1 1 UDP 1 192.0.2.1 5000 typ host","sdpMLineIndex":0}"#,
    );
    assert_eq!(response["ok"], false);
    assert_eq!(response["error"], "unknown WebRTC session");

    drop(reader);
    drop(writer);

    // SIGTERM must stop the helper and remove the socket.
    let status = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .expect("run kill");
    assert!(status.success(), "kill -TERM failed");

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match guard.0.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Ok(None) => panic!("helper did not exit within 10s of SIGTERM"),
            Err(error) => panic!("could not poll helper: {error}"),
        }
    }
    assert!(!sock.exists(), "helper left its signaling socket behind");
}
