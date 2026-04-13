//! End-to-end tests for ozma-agent.
//!
//! Spawns the real binary against a pair of ephemeral ports, exercises every
//! HTTP endpoint, then terminates the process and verifies a clean exit.
//!
//! The tests point the agent at the real local controller (http://localhost:7380)
//! so the mesh reconciliation task runs a real attempt — the controller's
//! /api/v1/mesh/peers returns 404 today, and the test verifies the agent
//! tolerates that gracefully (it should stay up, not crash).

use std::{
    net::TcpListener,
    process::{Child, Command, Stdio},
    time::Duration,
};

use serde_json::Value;

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Pick a free TCP port by binding to :0 and reading the assigned port.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Spawn the agent binary with given ports and controller URL.
fn start_agent(api_port: u16, metrics_port: u16, controller_url: &str) -> Child {
    let bin = env!("CARGO_BIN_EXE_ozma-agent");
    Command::new(bin)
        .args([
            "--api-port",      &api_port.to_string(),
            "--metrics-port",  &metrics_port.to_string(),
            "--controller-url", controller_url,
            // Use a throwaway WG port that won't conflict.
            "--wg-port",       "0",
        ])
        .env("RUST_LOG", "warn")  // quieter output during tests
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn ozma-agent binary")
}

/// Poll `url` until it returns 200 or `timeout` elapses.
/// Returns `true` if the server became healthy in time.
async fn wait_healthy(url: &str, timeout: Duration) -> bool {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(1))
        .build()
        .unwrap();

    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if let Ok(r) = client.get(url).send().await {
            if r.status().is_success() {
                return true;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

/// RAII guard: kills the child process when dropped.
struct AgentGuard(Child);

impl Drop for AgentGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// Verify the agent starts, exposes /healthz, and returns 200 OK.
#[tokio::test]
async fn healthz_returns_ok() {
    let api_port     = free_port();
    let metrics_port = free_port();

    let child = start_agent(api_port, metrics_port, "http://localhost:7380");
    let _guard = AgentGuard(child);

    let healthz = format!("http://127.0.0.1:{api_port}/healthz");
    assert!(
        wait_healthy(&healthz, Duration::from_secs(10)).await,
        "agent did not become healthy within 10 s"
    );

    let resp = reqwest::get(&healthz).await.expect("GET /healthz");
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(resp.text().await.unwrap().trim(), "ok");
}

/// Verify /api/v1/status returns JSON with status=running and a version field.
#[tokio::test]
async fn status_endpoint_returns_running() {
    let api_port     = free_port();
    let metrics_port = free_port();

    let child = start_agent(api_port, metrics_port, "http://localhost:7380");
    let _guard = AgentGuard(child);

    let healthz = format!("http://127.0.0.1:{api_port}/healthz");
    assert!(wait_healthy(&healthz, Duration::from_secs(10)).await);

    let resp = reqwest::get(format!("http://127.0.0.1:{api_port}/api/v1/status"))
        .await
        .expect("GET /api/v1/status");

    assert_eq!(resp.status().as_u16(), 200);

    let body: Value = resp.json().await.expect("parse JSON");
    assert_eq!(body["status"], "running", "status field should be 'running'");
    assert!(
        body["version"].as_str().is_some_and(|v| !v.is_empty()),
        "version field should be a non-empty string"
    );
}

/// Verify /api/v1/version returns a non-empty version string.
#[tokio::test]
async fn version_endpoint_returns_string() {
    let api_port     = free_port();
    let metrics_port = free_port();

    let child = start_agent(api_port, metrics_port, "http://localhost:7380");
    let _guard = AgentGuard(child);

    let healthz = format!("http://127.0.0.1:{api_port}/healthz");
    assert!(wait_healthy(&healthz, Duration::from_secs(10)).await);

    let resp = reqwest::get(format!("http://127.0.0.1:{api_port}/api/v1/version"))
        .await
        .expect("GET /api/v1/version");

    assert_eq!(resp.status().as_u16(), 200);
    let version = resp.text().await.unwrap();
    assert!(!version.trim().is_empty(), "version should not be empty");
    // Should look like a semver string, e.g. "0.1.0"
    assert!(
        version.trim().contains('.'),
        "version '{version}' should contain a dot"
    );
}

/// Verify the Prometheus metrics endpoint is reachable and returns process_ metrics.
#[tokio::test]
async fn metrics_endpoint_returns_prometheus_text() {
    let api_port     = free_port();
    let metrics_port = free_port();

    let child = start_agent(api_port, metrics_port, "http://localhost:7380");
    let _guard = AgentGuard(child);

    // Wait for the API to be healthy first (implies the process is up).
    let healthz = format!("http://127.0.0.1:{api_port}/healthz");
    assert!(wait_healthy(&healthz, Duration::from_secs(10)).await);

    // Give the metrics server a moment to bind (it starts concurrently).
    tokio::time::sleep(Duration::from_millis(200)).await;

    let resp = reqwest::get(format!("http://127.0.0.1:{metrics_port}/metrics"))
        .await
        .expect("GET /metrics");

    assert_eq!(resp.status().as_u16(), 200);

    let body = resp.text().await.unwrap();
    // Prometheus text format always starts with #
    assert!(
        body.contains("process_"),
        "metrics output should contain process_ metrics, got: {body:.200}"
    );
    assert!(
        body.contains("# HELP") || body.contains("# TYPE"),
        "metrics output should contain Prometheus HELP/TYPE comments"
    );
}

/// Verify the agent stays up despite the mesh reconciliation failing with 404.
///
/// The running controller at localhost:7380 has no /api/v1/mesh/peers endpoint
/// (returns 404). The agent should log a warning and keep running — not crash.
#[tokio::test]
async fn agent_tolerates_mesh_404_from_controller() {
    let api_port     = free_port();
    let metrics_port = free_port();

    // Point at the real controller — it returns 404 for mesh/peers.
    let child = start_agent(api_port, metrics_port, "http://localhost:7380");
    let _guard = AgentGuard(child);

    let healthz = format!("http://127.0.0.1:{api_port}/healthz");
    assert!(
        wait_healthy(&healthz, Duration::from_secs(10)).await,
        "agent crashed after mesh 404"
    );

    // Stay up for another second — mesh reconcile fires every 30s so won't
    // trigger again in the test window, but the agent must still be alive.
    tokio::time::sleep(Duration::from_secs(1)).await;

    let resp = reqwest::get(&healthz).await.expect("second health check");
    assert_eq!(resp.status().as_u16(), 200, "agent should still be healthy");
}

/// Verify the agent starts without a running controller (connection refused).
///
/// Connect-refused on the controller URL is non-fatal: the mesh task should
/// log a warning and retry, not bring down the process.
#[tokio::test]
async fn agent_starts_without_controller() {
    let api_port     = free_port();
    let metrics_port = free_port();

    // Port 1 is reserved — nothing should be listening there.
    let child = start_agent(api_port, metrics_port, "http://127.0.0.1:1");
    let _guard = AgentGuard(child);

    let healthz = format!("http://127.0.0.1:{api_port}/healthz");
    assert!(
        wait_healthy(&healthz, Duration::from_secs(10)).await,
        "agent failed to start when controller is unreachable"
    );
}

/// Verify the agent shuts down cleanly on SIGTERM (Unix only).
#[cfg(unix)]
#[tokio::test]
async fn agent_shuts_down_on_sigterm() {
    use std::os::unix::process::ExitStatusExt;

    let api_port     = free_port();
    let metrics_port = free_port();

    let mut child = start_agent(api_port, metrics_port, "http://localhost:7380");

    let healthz = format!("http://127.0.0.1:{api_port}/healthz");
    assert!(wait_healthy(&healthz, Duration::from_secs(10)).await);

    // Send SIGTERM.
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };

    // Wait up to 5 s for the process to exit.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            // On SIGTERM the process may exit 0 (graceful) or with signal 15.
            // Either is acceptable — what matters is it exits.
            let _ = status.signal(); // just consuming it
            return; // test passed
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            panic!("agent did not exit within 5 s of SIGTERM");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
