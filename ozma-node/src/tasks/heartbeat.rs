//! Controller registration and heartbeat
//!
//! On startup: POST /api/v1/nodes/register with full node metadata.
//! Every `heartbeat_secs`: POST /api/v1/nodes/heartbeat to keep the node
//! visible in the controller's node list.
//!
//! Registration is retried with a 5 s back-off until the controller responds.
//! When --register-url is not set, mDNS handles discovery and we skip
//! registration but still run the heartbeat loop if a URL is configured.

use anyhow::Result;
use reqwest::Client;
use serde_json::json;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn, debug};

use crate::Cli;

pub async fn run(cli: Cli, cancel: CancellationToken) -> Result<()> {
    let client = Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;

    // Honour --register-host override (SLIRP / QEMU port-forward scenarios)
    let local_ip = match cli.register_host.as_deref() {
        Some(h) if !h.is_empty() => h.to_string(),
        _ => resolve_local_ip().await.unwrap_or_else(|| "127.0.0.1".to_string()),
    };

    // Only register directly when --register-url is set; otherwise mDNS handles discovery
    let effective_url = match cli.register_url.as_deref() {
        Some(url) if !url.is_empty() => url.to_string(),
        _ => {
            info!("No --register-url set; relying on mDNS for controller discovery");
            // Nothing to do — just wait for cancellation
            cancel.cancelled().await;
            return Ok(());
        }
    };

    // Initial registration — retry until the controller is reachable
    loop {
        match register(&client, &cli, &local_ip, &effective_url).await {
            Ok(_) => {
                info!(controller = %effective_url, "Registered with controller");
                break;
            }
            Err(e) => {
                warn!("Registration failed ({e:#}), retrying in 5s");
                tokio::select! {
                    _ = cancel.cancelled() => return Ok(()),
                    _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                }
            }
        }
    }

    // Heartbeat loop
    let interval = Duration::from_secs(cli.heartbeat_secs);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                info!("Heartbeat task shutting down");
                break;
            }
            _ = tokio::time::sleep(interval) => {
                if let Err(e) = heartbeat(&client, &cli, &effective_url).await {
                    warn!("Heartbeat failed: {e:#}");
                }
            }
        }
    }

    Ok(())
}

async fn register(client: &Client, cli: &Cli, local_ip: &str, base_url: &str) -> Result<()> {
    let url = format!("{}/api/v1/nodes/register", base_url);
    // node_id mirrors node.py: "<name>._ozma._udp.local."
    let node_id = cli.node_id();
    let http_port = cli.http_port.to_string();
    let mut body = serde_json::json!({
        "id":            node_id,
        "host":          local_ip,
        "port":          cli.hid_udp_port,
        "proto":         "1",
        "role":          cli.role,
        "hw":            cli.hw,
        "fw":            cli.fw,
        "cap":           cli.cap,
        "api_port":      http_port,
        "machine_class": "workstation",
        "capture_device": cli.capture_device,
    });
    // Advertise stream fields when video capability is present
    if cli.cap.split(',').any(|c| c.trim() == "video") {
        body["stream_port"] = serde_json::Value::String(http_port.clone());
        body["stream_path"] = serde_json::Value::String("/stream/stream.m3u8".into());
    }

    let resp = client.post(&url).json(&body).send().await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("HTTP {status}: {text}");
    }
    Ok(())
}

async fn heartbeat(client: &Client, cli: &Cli, base_url: &str) -> Result<()> {
    let url  = format!("{}/api/v1/nodes/heartbeat", base_url);
    let body = json!({ "node_id": cli.node_id() });
    let resp = client.post(&url).json(&body).send().await?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("Heartbeat HTTP {status}: {text}");
    }
    debug!("Heartbeat OK ({status})");
    Ok(())
}

/// Resolve the outbound LAN IP using a UDP connect trick.
/// No packet is sent — the OS picks the interface and we read the local addr.
async fn resolve_local_ip() -> Option<String> {
    let sock = tokio::net::UdpSocket::bind("0.0.0.0:0").await.ok()?;
    sock.connect("8.8.8.8:80").await.ok()?;
    Some(sock.local_addr().ok()?.ip().to_string())
}
