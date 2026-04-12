//! Controller registration and heartbeat.
//!
//! Mirrors the `_direct_register` method and heartbeat loop in `node/node.py`.

use std::collections::HashMap;
use std::time::Duration;

use reqwest::Client;
use serde_json::json;
use tracing::{error, info, warn};

/// POST `/api/v1/nodes/register` to the controller.
///
/// The JSON payload mirrors the `DirectRegisterRequest` Pydantic model in
/// `controller/api.py`.  Unknown extra fields are ignored by the controller.
pub async fn register(
    base_url: &str,
    node_id: &str,
    host: &str,
    hid_udp_port: u16,
    txt: &HashMap<String, String>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let url = format!("{}/api/v1/nodes/register", base_url.trim_end_matches('/'));

    // Start with the required fields.
    let mut payload = json!({
        "id":   node_id,
        "host": host,
        "port": hid_udp_port,
    });

    // Merge TXT record fields — mirrors how node.py builds the payload dict:
    //   payload = {"id": ..., "host": ..., "port": ..., **txt}
    if let Some(obj) = payload.as_object_mut() {
        for (k, v) in txt {
            // "host" is already set above; skip to avoid overwriting with the
            // TXT copy (which may be the --register-host override value).
            if k != "host" {
                obj.insert(k.clone(), json!(v));
            }
        }
    }

    let client = Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;

    let resp = client.post(&url).json(&payload).send().await?;

    if resp.status().is_success() {
        info!(node_id, url = %url, "Registration accepted by controller");
        Ok(())
    } else {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        Err(format!("Controller returned {status}: {body}").into())
    }
}

/// Heartbeat loop — POSTs to `/api/v1/nodes/heartbeat` every `interval_secs`.
///
/// Mirrors the implicit keep-alive behaviour in `node.py` (the controller
/// expires nodes that stop sending heartbeats).  Runs indefinitely; cancel by
/// aborting the spawned task.
pub async fn heartbeat_loop(base_url: &str, node_id: &str, interval_secs: u64) {
    let url = format!("{}/api/v1/nodes/heartbeat", base_url.trim_end_matches('/'));

    let client = match Client::builder().timeout(Duration::from_secs(5)).build() {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to build HTTP client for heartbeat: {e}");
            return;
        }
    };

    let payload = json!({ "node_id": node_id });
    let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs));
    // Consume the first (immediate) tick so we don't fire right after startup.
    ticker.tick().await;

    loop {
        ticker.tick().await;
        match client.post(&url).json(&payload).send().await {
            Ok(resp) if resp.status().is_success() => {
                info!(node_id, "Heartbeat OK");
            }
            Ok(resp) => {
                warn!(node_id, status = %resp.status(), "Heartbeat rejected by controller");
            }
            Err(e) => {
                warn!(node_id, error = %e, "Heartbeat request failed");
            }
        }
    }
}
