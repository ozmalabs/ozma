// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Opt-in Gate 2 live node check.
//!
//! This target is deliberately `#[ignore]`d: it talks to an already-running
//! `ozma-node --profile vm`, not to a fake server, and it must never start
//! QEMU, ffmpeg, a WebRTC helper, or the Python controller. The default test
//! suite compiles it but does not run it. Running it without an environment is
//! a clean skip.
//!
//! Example (QMP + HLS + helper):
//!
//! ```text
//! OZMA_GATE2_NODE_URL=http://127.0.0.1:7382 \
//! OZMA_GATE2_NODE_ID=ozma-vm-gate2._ozma._udp.local. \
//! cargo test -p ozma-node --test gate2_live -- --ignored --nocapture
//! ```
//!
//! Optional:
//!
//! * `OZMA_GATE2_API_TOKEN` (falls back to `OZMA_API_TOKEN`) for a guarded node.
//! * `OZMA_GATE2_EXPECT_PROFILE` (default `vm`).
//! * `OZMA_GATE2_HLS_WAIT_SECS` (default 10) to wait for the first manifest.
//! * `OZMA_GATE2_HLS_ADVANCE_SECS` (default 15; 0 disables the advance check).
//! * `OZMA_GATE2_WEBRTC_SDP_FILE` containing a real browser offer JSON
//!   (`{"sdp": "...", "type": "offer"}`). Without it, the helper is reported
//!   but `/webrtc/offer` is not exercised because a synthetic SDP is not a
//!   valid negotiation.
//!
//! This asserts only the node's HTTP surface. A live controller registration,
//! dashboard proxy, guest-visible input, and browser media path remain manual
//! Gate 2 evidence rows.

use std::time::{Duration, Instant};

use reqwest::StatusCode;
use serde_json::Value;

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn join(base: &str, path: &str) -> String {
    format!(
        "{}/{}",
        base.trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}

fn preview(body: &str) -> String {
    body.chars().take(400).collect()
}

async fn get_json(client: &reqwest::Client, url: &str, token: Option<&str>) -> (StatusCode, Value) {
    let mut request = client.get(url);
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    let response = request
        .send()
        .await
        .unwrap_or_else(|error| panic!("GET {url} failed: {error}"));
    let status = response.status();
    let body = response
        .text()
        .await
        .unwrap_or_else(|error| panic!("GET {url} body failed: {error}"));
    let json = serde_json::from_str(&body).unwrap_or_else(|error| {
        panic!(
            "GET {url} did not return JSON ({status}): {error}; body={}",
            preview(&body)
        )
    });
    (status, json)
}

async fn get_text(client: &reqwest::Client, url: &str) -> (StatusCode, String) {
    let response = client
        .get(url)
        .send()
        .await
        .unwrap_or_else(|error| panic!("GET {url} failed: {error}"));
    let status = response.status();
    let body = response
        .text()
        .await
        .unwrap_or_else(|error| panic!("GET {url} body failed: {error}"));
    (status, body)
}

#[tokio::test]
#[ignore = "live Gate 2 check; set OZMA_GATE2_NODE_URL and pass --ignored"]
async fn gate2_live_node_surface() {
    let Some(base) = env_nonempty("OZMA_GATE2_NODE_URL") else {
        eprintln!("SKIP gate2_live_node_surface: OZMA_GATE2_NODE_URL is not set");
        return;
    };
    let token = env_nonempty("OZMA_GATE2_API_TOKEN").or_else(|| env_nonempty("OZMA_API_TOKEN"));
    let expected_id = env_nonempty("OZMA_GATE2_NODE_ID");
    let expected_profile =
        env_nonempty("OZMA_GATE2_EXPECT_PROFILE").unwrap_or_else(|| "vm".to_owned());

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .connect_timeout(Duration::from_secs(5))
        .build()
        .expect("build HTTP client");

    // `/health` is intentionally readable without credentials.
    let (status, health) = get_json(&client, &join(&base, "/health"), token.as_deref()).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "/health returned {status}: {}",
        preview(&health.to_string())
    );
    assert_eq!(health["ok"], true, "/health did not report ok=true");

    let (status, connection) = get_json(
        &client,
        &join(&base, "/api/v1/connection"),
        token.as_deref(),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "/api/v1/connection returned {status}: {}",
        preview(&connection.to_string())
    );
    assert_eq!(connection["ok"], true, "connection did not report ok=true");
    assert_eq!(
        connection["profile"].as_str(),
        Some(expected_profile.as_str()),
        "connection profile mismatch: {}",
        preview(&connection.to_string())
    );
    if let Some(expected_id) = expected_id.as_deref() {
        assert_eq!(
            connection["node_id"].as_str(),
            Some(expected_id),
            "connection node_id mismatch: {}",
            preview(&connection.to_string())
        );
    }

    let (status, power) = get_json(&client, &join(&base, "/power/state"), token.as_deref()).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "/power/state returned {status}: {}",
        preview(&power.to_string())
    );
    assert_eq!(
        power["available"],
        true,
        "power backend is not available; configure --qmp-socket or --libvirt-domain: {}",
        preview(&power.to_string())
    );
    assert!(
        power["status"]
            .as_str()
            .is_some_and(|value| !value.is_empty()),
        "power state has no status string: {}",
        preview(&power.to_string())
    );

    let hls_state = connection["hls"].as_str().unwrap_or("");
    match hls_state {
        "running" => {
            let wait_secs = env_nonempty("OZMA_GATE2_HLS_WAIT_SECS")
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(10);
            let playlist_url = join(&base, "/stream/stream.m3u8");
            // Deliberately no bearer: the controller's HLS proxy fetches this
            // path without credentials.
            let hls_client = reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .connect_timeout(Duration::from_secs(5))
                .build()
                .expect("build HLS HTTP client");

            let deadline = Instant::now() + Duration::from_secs(wait_secs);
            let first = loop {
                let (status, body) = get_text(&hls_client, &playlist_url).await;
                if status == StatusCode::OK && body.starts_with("#EXTM3U") {
                    break body;
                }
                assert!(
                    Instant::now() < deadline,
                    "HLS is running but {playlist_url} did not return an #EXTM3U playlist within {wait_secs}s (last status {status}, body {})",
                    preview(&body)
                );
                tokio::time::sleep(Duration::from_millis(250)).await;
            };

            let advance_secs = env_nonempty("OZMA_GATE2_HLS_ADVANCE_SECS")
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(15);
            if advance_secs > 0 {
                let deadline = Instant::now() + Duration::from_secs(advance_secs);
                loop {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    let (status, second) = get_text(&hls_client, &playlist_url).await;
                    assert_eq!(
                        status,
                        StatusCode::OK,
                        "playlist fetch returned {status} while waiting for an advance: {}",
                        preview(&second)
                    );
                    assert!(
                        second.starts_with("#EXTM3U"),
                        "playlist fetch is not an HLS playlist: {}",
                        preview(&second)
                    );
                    if second != first {
                        break;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "HLS playlist did not advance within {advance_secs}s; capture may be stalled"
                    );
                }
            } else {
                eprintln!("SKIP HLS playlist advance check (OZMA_GATE2_HLS_ADVANCE_SECS=0)");
            }
        }
        "failed" => panic!(
            "HLS is configured but failed: {}",
            connection["hls_error"]
                .as_str()
                .unwrap_or("no hls_error was provided")
        ),
        "not-configured" => {
            eprintln!("SKIP HLS checks: the node was started without --hls");
        }
        other => panic!("unexpected connection.hls value: {other:?}"),
    }

    let webrtc_state = connection["webrtc"].as_str().unwrap_or("");
    match webrtc_state {
        "running" => {
            if let Some(path) = env_nonempty("OZMA_GATE2_WEBRTC_SDP_FILE") {
                let offer_text = std::fs::read_to_string(&path)
                    .unwrap_or_else(|error| panic!("read {path}: {error}"));
                let offer: Value = serde_json::from_str(&offer_text)
                    .unwrap_or_else(|error| panic!("{path} is not JSON: {error}"));
                assert!(
                    offer
                        .get("sdp")
                        .and_then(Value::as_str)
                        .is_some_and(|sdp| !sdp.trim().is_empty()),
                    "{path} has no non-empty sdp field"
                );

                let mut request = client.post(join(&base, "/webrtc/offer")).json(&offer);
                if let Some(token) = token.as_deref() {
                    request = request.bearer_auth(token);
                }
                let response = request
                    .send()
                    .await
                    .unwrap_or_else(|error| panic!("POST /webrtc/offer failed: {error}"));
                let status = response.status();
                let body = response
                    .text()
                    .await
                    .unwrap_or_else(|error| panic!("offer body failed: {error}"));
                assert_eq!(
                    status,
                    StatusCode::OK,
                    "/webrtc/offer returned {status}: {}",
                    preview(&body)
                );
                let answer: Value = serde_json::from_str(&body).unwrap_or_else(|error| {
                    panic!(
                        "/webrtc/offer did not return JSON: {error}; body={}",
                        preview(&body)
                    )
                });
                assert_eq!(
                    answer["type"].as_str(),
                    Some("answer"),
                    "offer response is not an answer: {}",
                    preview(&body)
                );
                assert!(
                    answer["sdp"]
                        .as_str()
                        .is_some_and(|sdp| !sdp.trim().is_empty()),
                    "offer response has an empty sdp: {}",
                    preview(&body)
                );
            } else {
                eprintln!(
                    "SKIP /webrtc/offer: helper is running; set OZMA_GATE2_WEBRTC_SDP_FILE to a real browser offer"
                );
            }
        }
        "failed" => panic!(
            "WebRTC helper is configured but failed: {}",
            connection["webrtc_error"]
                .as_str()
                .unwrap_or("no webrtc_error was provided")
        ),
        "not-configured" => {
            eprintln!("SKIP WebRTC checks: the node was started without --webrtc-helper");
        }
        other => panic!("unexpected connection.webrtc value: {other:?}"),
    }
}
