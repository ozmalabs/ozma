//! Controller registration task.
//!
//! Registers this agent with the controller's direct-registration endpoint
//! (`POST /api/v1/nodes/register`) and then sends periodic heartbeats
//! (`POST /api/v1/nodes/heartbeat`) to keep the node alive in the controller's
//! state.
//!
//! The node ID follows the mDNS convention used by soft nodes:
//!   `{hostname}._ozma._udp.local.`
//!
//! Registration is retried with exponential back-off if the controller is
//! unreachable; heartbeat failures are logged but do not stop the loop.

use anyhow::Result;
use serde_json::json;
use tracing::{info, warn};

const HEARTBEAT_INTERVAL: tokio::time::Duration = tokio::time::Duration::from_secs(30);
const REGISTER_TIMEOUT:   std::time::Duration   = std::time::Duration::from_secs(10);

/// Derive the mDNS-style node ID from the machine hostname.
pub fn node_id() -> String {
    let host = hostname::get()
        .map(|h| h.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "unknown".to_string());
    format!("{host}._ozma._udp.local.")
}

/// Register with the controller and send periodic heartbeats.
///
/// * `controller_url` — e.g. `http://localhost:7380`
/// * `api_port`       — the port this agent's own HTTP API is bound on
/// * `wg_port`        — the WireGuard UDP port
pub async fn run(controller_url: String, api_port: u16, wg_port: u16) -> Result<()> {
    let id = node_id();
    info!(id, controller_url, "registration task starting");

    let client = reqwest::Client::builder()
        .timeout(REGISTER_TIMEOUT)
        .build()?;

    // ── Initial registration with back-off ────────────────────────────────────

    let backoff = backoff::ExponentialBackoffBuilder::new()
        .with_initial_interval(std::time::Duration::from_secs(2))
        .with_max_interval(std::time::Duration::from_secs(60))
        .with_max_elapsed_time(None) // retry forever
        .build();

    let register_url = format!("{controller_url}/api/v1/nodes/register");

    backoff::future::retry(backoff, || {
        let client      = client.clone();
        let id          = id.clone();
        let url         = register_url.clone();
        let api_port_s  = api_port.to_string();
        async move {
            let payload = json!({
                "id":           id,
                "host":         local_ip(),
                "port":         wg_port,
                "role":         "compute",
                "hw":           "soft",
                "fw":           env!("CARGO_PKG_VERSION"),
                "cap":          "agent",
                "api_port":     api_port_s,
                "machine_class": "workstation",
            });

            match client.post(&url).json(&payload).send().await {
                Ok(resp) if resp.status().is_success() => {
                    info!("registered with controller as {}", id);
                    Ok(())
                }
                Ok(resp) => {
                    warn!("registration returned {}", resp.status());
                    Err(backoff::Error::transient(anyhow::anyhow!(
                        "controller returned {}",
                        resp.status()
                    )))
                }
                Err(e) => {
                    warn!("registration failed: {e:#}");
                    Err(backoff::Error::transient(anyhow::anyhow!("{e}")))
                }
            }
        }
    })
    .await?;

    // ── Heartbeat loop ────────────────────────────────────────────────────────

    let heartbeat_url = format!("{controller_url}/api/v1/nodes/heartbeat");
    loop {
        tokio::time::sleep(HEARTBEAT_INTERVAL).await;

        let payload = json!({ "node_id": id });
        match client.post(&heartbeat_url).json(&payload).send().await {
            Ok(resp) if resp.status().is_success() => {
                // silent success
            }
            Ok(resp) => warn!("heartbeat returned {}", resp.status()),
            Err(e)   => warn!("heartbeat failed: {e:#}"),
        }
    }
}

/// Best-effort local IP: first non-loopback IPv4, else 127.0.0.1.
fn local_ip() -> String {
    // Probe by connecting a UDP socket — the OS picks the outbound interface.
    use std::net::UdpSocket;
    UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| {
            s.connect("8.8.8.8:80")?;
            s.local_addr()
        })
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|_| "127.0.0.1".to_string())
}
