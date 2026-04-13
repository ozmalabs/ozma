//! WireGuard mesh task.
//!
//! Periodically fetches the peer list from the controller and reconciles the
//! local WireGuard interface so every agent in the cluster can reach every
//! other agent directly (full-mesh topology).
//!
//! The heavy lifting (key exchange, tunnel I/O) is delegated to `ozma-mesh`.

use anyhow::Result;
use tracing::{debug, info, warn};

const RECONCILE_INTERVAL: tokio::time::Duration = tokio::time::Duration::from_secs(30);

/// Run the WireGuard mesh reconciliation loop indefinitely.
pub async fn run(controller_url: String, wg_port: u16) -> Result<()> {
    info!(controller_url, wg_port, "mesh task starting");

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;

    loop {
        match reconcile(&client, &controller_url, wg_port).await {
            Ok(peers) => debug!(peers, "mesh reconciled"),
            Err(e)    => warn!("mesh reconcile failed: {e:#}"),
        }
        tokio::time::sleep(RECONCILE_INTERVAL).await;
    }
}

/// Fetch peers from the controller and apply them to the local WG interface.
async fn reconcile(
    client: &reqwest::Client,
    controller_url: &str,
    _wg_port: u16,
) -> Result<usize> {
    let url = format!("{controller_url}/api/v1/mesh/peers");
    let resp = client.get(&url).send().await?;

    if !resp.status().is_success() {
        anyhow::bail!("controller returned {}", resp.status());
    }

    // TODO: deserialise peer list, call ozma_mesh::wg to upsert tunnels.
    let body: serde_json::Value = resp.json().await?;
    let peers = body
        .get("peers")
        .and_then(|p| p.as_array())
        .map(|a| a.len())
        .unwrap_or(0);

    Ok(peers)
}
