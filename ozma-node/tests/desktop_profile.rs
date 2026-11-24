// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Desktop-profile startup and compatibility routes with fake providers.
//!
//! This test starts the real `ozma-node` binary in `--profile desktop` with a
//! synthetic capture source and the recording-only input sink. It therefore
//! needs no PipeWire session, `/dev/uinput`, X11 display, portal consent, or
//! privileged port, and it never injects desktop input.
//!
//! Covered: `/health`, `/status`, `/snapshot`, `/display/snapshot`,
//! `/display/info`, `/api/v1/connection`, `/api/v1/capabilities`, and the
//! honest `/audio/*` 501 surface on a desktop node.

use std::net::{TcpListener, UdpSocket};
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::process::Command;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

/// Reserve a TCP pair (`http`, `quic = http + 2`) and a UDP HID port.
///
/// The listeners are dropped before the node starts; the short race is
/// acceptable for a local test and the ports are outside the fixed parity
/// fixtures.
fn reserve_ports() -> (u16, u16, u16) {
    for base in (20_000u16..40_000).step_by(4) {
        let http = TcpListener::bind(("127.0.0.1", base));
        let quic = TcpListener::bind(("127.0.0.1", base + 2));
        let hid = UdpSocket::bind(("127.0.0.1", base + 1));
        if let (Ok(http), Ok(quic), Ok(hid)) = (http, quic, hid) {
            let ports = (
                http.local_addr().unwrap().port(),
                hid.local_addr().unwrap().port(),
                quic.local_addr().unwrap().port(),
            );
            drop(http);
            drop(quic);
            drop(hid);
            return ports;
        }
    }
    panic!("no free desktop-profile test ports");
}

async fn wait_until_ready(client: &reqwest::Client, url: &str) -> Value {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        if let Ok(response) = client.get(url).send().await {
            if response.status().is_success() {
                return response.json().await.expect("health JSON");
            }
        }
        assert!(
            Instant::now() < deadline,
            "desktop node did not answer {url} within {STARTUP_TIMEOUT:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn desktop_profile_serves_status_snapshot_and_connection_with_fake_providers() {
    let (http_port, hid_port, _quic_port) = reserve_ports();
    let base = format!("http://127.0.0.1:{http_port}");
    let http_port_arg = http_port.to_string();
    let hid_port_arg = hid_port.to_string();

    let mut child = Command::new(env!("CARGO_BIN_EXE_ozma-node"))
        .args([
            "--profile",
            "desktop",
            "--name",
            "desktop-profile-test",
            "--http-port",
            &http_port_arg,
            "--hid-udp-port",
            &hid_port_arg,
            // Explicit dev/test providers: no PipeWire capture or uinput is
            // opened, so this test is safe on CI and on a real desktop.
            "--capture-provider",
            "fake",
            "--fake-width",
            "320",
            "--fake-height",
            "240",
            "--fake-fps",
            "10",
            "--input-provider",
            "fake-recording",
        ])
        .env("RUST_LOG", "error")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn ozma-node desktop profile");

    let client = reqwest::Client::new();
    let health = wait_until_ready(&client, &format!("{base}/health")).await;
    assert_eq!(health["ok"], true);
    assert_eq!(health["node_id"], "desktop-profile-test._ozma._udp.local.");

    // `/status` is the desktop agent's legacy alias for the connection summary.
    let status: Value = client
        .get(format!("{base}/status"))
        .send()
        .await
        .expect("/status request")
        .json()
        .await
        .expect("/status JSON");
    assert_eq!(status["ok"], true);
    assert_eq!(status["profile"], "desktop");
    assert_eq!(status["sink_kind"], "fake-recording");
    assert_eq!(status["api_port"], http_port);
    assert_eq!(status["hid_udp_port"], hid_port);

    let connection: Value = client
        .get(format!("{base}/api/v1/connection"))
        .send()
        .await
        .expect("/api/v1/connection request")
        .json()
        .await
        .expect("/api/v1/connection JSON");
    assert_eq!(connection["ok"], true);
    assert_eq!(connection["profile"], "desktop");
    assert_eq!(connection["input"]["available"], true);
    assert_eq!(
        connection["input"]["injecting"], false,
        "the recording sink must never be reported as injecting"
    );
    assert_eq!(connection["hls"], "not-configured");
    assert_eq!(connection["webrtc"], "not-configured");
    assert_eq!(connection["audio"], "not-configured");

    // Both the controller route and the desktop agent alias return a real JPEG.
    for route in ["/snapshot", "/display/snapshot"] {
        let response = client
            .get(format!("{base}{route}"))
            .send()
            .await
            .unwrap_or_else(|error| panic!("{route}: {error}"));
        assert_eq!(response.status(), reqwest::StatusCode::OK, "{route}");
        assert_eq!(
            response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("image/jpeg"),
            "{route}"
        );
        let bytes = response.bytes().await.expect("snapshot body");
        assert!(
            bytes.starts_with(&[0xFF, 0xD8]),
            "{route} did not return a JPEG"
        );
    }

    let info: Value = client
        .get(format!("{base}/display/info"))
        .send()
        .await
        .expect("/display/info request")
        .json()
        .await
        .expect("/display/info JSON");
    assert_eq!(info["ok"], true);
    assert_eq!(info["source_id"], "fake0");
    assert_eq!(info["snapshot_available"], true);
    assert_eq!(info["snapshot_encoding"], "jpeg");

    // Capability honesty: the recording sink injects nothing, and the only
    // video source is the explicitly requested fake (never relabelled as a
    // desktop screen).
    let capabilities: Value = client
        .get(format!("{base}/api/v1/capabilities"))
        .send()
        .await
        .expect("/api/v1/capabilities request")
        .json()
        .await
        .expect("/api/v1/capabilities JSON");
    assert_eq!(capabilities["input"]["keyboard"], false);
    assert_eq!(capabilities["input"]["mouse_absolute"], false);
    assert_eq!(capabilities["input"]["mouse_relative"], false);
    assert_eq!(capabilities["display"]["supported"], false);
    let captures = capabilities["capture"]
        .as_array()
        .expect("capture capability list");
    assert_eq!(captures.len(), 1);
    assert_eq!(captures[0]["id"], "fake0");
    assert_eq!(captures[0]["kind"], "fake");

    // Audio routing is not configured, so the control route is a typed 501 and
    // discovery/sweep stay honest unsupported responses rather than fake graphs.
    for (method, route) in [
        ("GET", "/audio/nodes"),
        ("POST", "/audio/sweep"),
        ("POST", "/audio/stop"),
    ] {
        let request = match method {
            "GET" => client.get(format!("{base}{route}")),
            _ => client.post(format!("{base}{route}")),
        };
        let response = request.send().await.expect(route);
        assert_eq!(
            response.status(),
            reqwest::StatusCode::NOT_IMPLEMENTED,
            "{method} {route}"
        );
        let body: Value = response.json().await.expect("audio error JSON");
        assert_eq!(body["ok"], false, "{method} {route}");
    }

    child.kill().await.expect("stop desktop test node");
}
