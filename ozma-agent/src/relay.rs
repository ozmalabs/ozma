//! WireGuard relay tunnel management.
//!
//! After registration the Connect API returns relay peer parameters.
//! This module writes a `wg-quick` config file and brings up the interface.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::PathBuf;
use tokio::fs;
use tokio::process::Command;
use tracing::{info, warn};

/// Relay parameters returned by the Connect `/agents/register` endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct RelayParams {
    /// WireGuard IP assigned to this agent on the mesh overlay.
    pub assigned_ip: String,
    /// Relay server endpoint, e.g. `relay.ozma.dev:51820`.
    pub relay_endpoint: String,
    /// Relay server WireGuard public key (base64).
    pub relay_public_key: String,
    /// Allowed IP ranges to route through the tunnel, e.g. `10.100.0.0/16`.
    pub allowed_ips: String,
}

fn config_path() -> PathBuf {
    PathBuf::from("/etc/wireguard/ozma-relay.conf")
}

/// Bring up (or reconfigure) the WireGuard relay tunnel via wg-quick.
///
/// Writes a `wg-quick` config file and calls `wg-quick up ozma-relay`.
/// Returns the assigned overlay IP on success.
pub async fn setup(params: &RelayParams, private_key_b64: &str) -> Result<String> {
    info!(
        assigned_ip = %params.assigned_ip,
        endpoint    = %params.relay_endpoint,
        "Setting up WireGuard relay tunnel"
    );

    let config = format!(
        "[Interface]\nPrivateKey = {}\nAddress = {}\n\n[Peer]\nPublicKey = {}\nEndpoint = {}\nAllowedIPs = {}\nPersistentKeepalive = 25\n",
        private_key_b64,
        params.assigned_ip,
        params.relay_public_key,
        params.relay_endpoint,
        params.allowed_ips,
    );

    fs::write(config_path(), config)
        .await
        .context("Write WireGuard relay config")?;

    // Tear down first in case it was already up, ignore errors.
    let _ = Command::new("wg-quick")
        .args(["down", "ozma-relay"])
        .output()
        .await;

    let out = Command::new("wg-quick")
        .args(["up", "ozma-relay"])
        .output()
        .await
        .context("wg-quick up ozma-relay")?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("wg-quick up failed: {stderr}");
    }

    info!(assigned_ip = %params.assigned_ip, "Relay tunnel up");
    Ok(params.assigned_ip.clone())
}

/// Tear down the relay tunnel interface.
pub async fn teardown() {
    let out = Command::new("wg-quick")
        .args(["down", "ozma-relay"])
        .output()
        .await;

    match out {
        Ok(o) if !o.status.success() => {
            warn!("wg-quick down ozma-relay: {}", String::from_utf8_lossy(&o.stderr));
        }
        Err(e) => warn!("Failed to run wg-quick: {e}"),
        _ => {}
    }

    let _ = fs::remove_file(config_path()).await;
}
