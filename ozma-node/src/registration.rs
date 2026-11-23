//! Controller registration and heartbeat.
//!
//! Mirrors the `_direct_register` method and heartbeat loop in `node/node.py`.

use std::collections::HashMap;
use std::time::Duration;

use reqwest::Client;
use serde_json::{json, Value};
use tracing::{error, info, warn};

/// Build the direct-registration JSON body.
///
/// The required fields (`id`, `host`, `port`) come from the node itself. Every
/// TXT record field is merged afterwards so mDNS and direct registration carry
/// the same strings. The TXT `host` copy is deliberately ignored: `host` is the
/// resolved address the controller should use, while the TXT value may be the
/// `--register-host` override.
///
/// This is a pure function so registration parity can be tested without a
/// controller.
pub fn registration_payload(
    node_id: &str,
    host: &str,
    hid_udp_port: u16,
    txt: &HashMap<String, String>,
) -> Value {
    let mut payload = json!({
        "id":   node_id,
        "host": host,
        "port": hid_udp_port,
    });

    if let Some(object) = payload.as_object_mut() {
        for (key, value) in txt {
            if key != "host" {
                object.insert(key.clone(), json!(value));
            }
        }
    }

    payload
}

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

    let payload = registration_payload(node_id, host, hid_udp_port, txt);

    let client = Client::builder().timeout(Duration::from_secs(5)).build()?;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registration_payload_carries_controller_fields_as_strings() {
        let mut txt = HashMap::new();
        txt.insert("proto".into(), "1".into());
        txt.insert("role".into(), "compute".into());
        txt.insert("hw".into(), "rpi4".into());
        txt.insert("fw".into(), "0.1.0".into());
        txt.insert("cap".into(), "hid,video,audio".into());
        txt.insert("api_port".into(), "7382".into());
        txt.insert("quic_port".into(), "7384".into());
        txt.insert("machine_class".into(), "server".into());
        txt.insert("capture_device".into(), "/dev/video0".into());
        txt.insert("display_outputs".into(), "[]".into());
        txt.insert("pci_devices".into(), "[\"0000:01:00.0\"]".into());
        txt.insert("vm_guest_ip".into(), "10.0.0.42".into());
        txt.insert("audio_type".into(), "pipewire".into());
        txt.insert("audio_sink".into(), "ozma-node".into());
        txt.insert("stream_port".into(), "7382".into());
        txt.insert("stream_path".into(), "/stream/stream.m3u8".into());
        // The TXT copy must not overwrite the resolved host.
        txt.insert("host".into(), "txt-host-should-lose".into());

        let payload = registration_payload("node-a._ozma._udp.local.", "192.0.2.10", 7331, &txt);

        assert_eq!(payload["id"], "node-a._ozma._udp.local.");
        assert_eq!(payload["host"], "192.0.2.10");
        assert_eq!(payload["port"], 7331);
        for field in [
            "proto",
            "role",
            "hw",
            "fw",
            "cap",
            "api_port",
            "quic_port",
            "machine_class",
            "capture_device",
            "display_outputs",
            "pci_devices",
            "vm_guest_ip",
            "audio_type",
            "audio_sink",
            "stream_port",
            "stream_path",
        ] {
            assert!(
                payload[field].is_string(),
                "controller field {field} must be a string, got {}",
                payload[field]
            );
        }
        assert_eq!(payload["api_port"], "7382");
        assert_eq!(payload["quic_port"], "7384");
        assert_eq!(payload["machine_class"], "server");
        assert_eq!(payload["audio_type"], "pipewire");
        assert_eq!(payload["audio_sink"], "ozma-node");
        assert_eq!(payload["display_outputs"], "[]");
        assert_eq!(payload["pci_devices"], "[\"0000:01:00.0\"]");
        assert_eq!(payload["vm_guest_ip"], "10.0.0.42");
    }
}
