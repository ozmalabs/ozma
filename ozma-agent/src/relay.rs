//! WireGuard relay tunnel via ozma-mesh.
//!
//! After registration the Connect API returns relay peer parameters.
//! This module configures the WireGuard tunnel using the `ozma-mesh` crate.

use anyhow::{Context, Result};
use serde::Deserialize;
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

/// Bring up (or reconfigure) the WireGuard relay tunnel.
///
/// Uses `ozma-mesh` to create/update the `ozma-relay` interface.
/// Returns the assigned overlay IP on success.
pub async fn setup(params: &RelayParams, private_key_b64: &str) -> Result<String> {
    use ozma_mesh::WgTunnel;

    info!(
        assigned_ip = %params.assigned_ip,
        endpoint    = %params.relay_endpoint,
        "Setting up WireGuard relay tunnel"
    );

    WgTunnel::configure(
        "ozma-relay",
        private_key_b64,
        &params.assigned_ip,
        &params.relay_public_key,
        &params.relay_endpoint,
        &params.allowed_ips,
        25, // persistent-keepalive seconds
    )
    .await
    .context("WireGuard relay tunnel setup failed")?;

    info!(assigned_ip = %params.assigned_ip, "Relay tunnel up");
    Ok(params.assigned_ip.clone())
}

/// Tear down the relay tunnel interface.
pub async fn teardown() {
    use ozma_mesh::WgTunnel;
    if let Err(e) = WgTunnel::remove("ozma-relay").await {
        warn!("Failed to remove relay tunnel: {e}");
    }
}
