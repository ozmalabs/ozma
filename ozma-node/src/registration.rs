//! Controller registration and heartbeat.
//!
//! Mirrors the `_direct_register` method and heartbeat loop in `node/node.py`.

use std::collections::HashMap;
use std::time::Duration;

use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::{error, info, warn};

/// Environment variable set by `ozma-multiseat` for a supervised seat node.
///
/// The planner (`ozma-multiseat/src/planner.rs`) passes it to every child so the
/// node can register an explicit identity instead of relying only on the
/// `<machine>-seat-<n>` node-name convention.
pub const SEAT_ID_ENV: &str = "OZMA_SEAT_ID";

/// Controller endpoints this node really serves.
///
/// `NotConfigured` is a typed absence, not an empty endpoint string: the fields
/// are omitted from mDNS and direct registration, and `/api/v1/connection`
/// reports the omission via [`RegistrationExtras::unsupported_controller_fields`].
///
/// The endpoint is either:
///
/// * the node's own opt-in RFB listener (`--vnc-server`), whose real bound
///   host/port is reported only while listening; or
/// * an external QEMU VNC endpoint selected with `--vnc-host`/`--vnc-port` when
///   no internal listener is running.
///
/// When both are configured the internal listener wins, so the controller
/// connects to the node rather than to a guest-local endpoint that may not be
/// reachable from the controller.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum VncRegistration {
    /// No VNC endpoint is configured or served by this node.
    #[default]
    NotConfigured,
    /// A complete, configured external VNC endpoint.
    Configured { host: String, port: u16 },
}

impl VncRegistration {
    /// The configured endpoint, if any.
    pub fn endpoint(&self) -> Option<(&str, u16)> {
        match self {
            Self::NotConfigured => None,
            Self::Configured { host, port } => Some((host, *port)),
        }
    }

    /// Whether a complete endpoint is configured.
    pub fn is_configured(&self) -> bool {
        matches!(self, Self::Configured { .. })
    }
}

/// Controller-compatibility fields the node can honestly advertise.
///
/// Only fields backed by real configuration are present. Everything else is
/// omitted (mDNS/direct registration) instead of being sent as `""`, which the
/// controller would otherwise treat as a configured-but-empty endpoint.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistrationExtras {
    /// Explicit seat identity, when `OZMA_SEAT_ID` is set and non-empty.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seat_id: Option<String>,
    /// Guest IP for VM nodes. Mirrors Python's mDNS `vm_ip` key; the direct
    /// registration copy is also emitted as `vm_guest_ip` for old consumers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vm_ip: Option<String>,
    /// External VNC endpoint, or a typed not-configured state.
    pub vnc: VncRegistration,
    /// Controller registration fields this node does not implement. The values
    /// are field names, never secret or fabricated endpoint data.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unsupported_controller_fields: Vec<String>,
}

impl RegistrationExtras {
    /// Resolve the optional compatibility fields from configuration.
    ///
    /// - `seat_id` is trimmed; empty means "not applicable" and is omitted.
    /// - VNC requires both a non-empty host and a non-zero port. A half
    ///   configured pair is never advertised (the config validator normally
    ///   rejects it before this runs).
    /// - `vm_ip` is trimmed; empty means no guest IP is known.
    pub fn resolve(
        seat_id: Option<&str>,
        vnc_host: Option<&str>,
        vnc_port: Option<u16>,
        vm_ip: Option<&str>,
    ) -> Self {
        Self::resolve_with_served_vnc(seat_id, None, vnc_host, vnc_port, vm_ip)
    }

    /// Resolve compatibility fields with an optional endpoint served by this
    /// node's own RFB listener.
    ///
    /// The served endpoint wins over an external QEMU VNC host/port when both
    /// exist: the internal listener is reachable wherever the node is, while
    /// `--vnc-host` may point at a guest-local address. `served` is supplied
    /// only after the listener has actually bound, so a configured-but-failed
    /// VNC server keeps the typed `not-configured` state (or falls back to the
    /// external endpoint).
    pub fn resolve_with_served_vnc(
        seat_id: Option<&str>,
        served: Option<(&str, u16)>,
        vnc_host: Option<&str>,
        vnc_port: Option<u16>,
        vm_ip: Option<&str>,
    ) -> Self {
        let seat_id = non_empty(seat_id).map(str::to_owned);
        let vm_ip = non_empty(vm_ip).map(str::to_owned);
        let served = served.and_then(|(host, port)| {
            some_endpoint(host, port).map(|(host, port)| VncRegistration::Configured {
                host: host.to_owned(),
                port,
            })
        });
        let vnc = served.unwrap_or_else(|| {
            match some_endpoint(vnc_host.unwrap_or(""), vnc_port.unwrap_or(0)) {
                Some((host, port)) => VncRegistration::Configured {
                    host: host.to_owned(),
                    port,
                },
                None => VncRegistration::NotConfigured,
            }
        });

        let mut unsupported_controller_fields = vec![
            "audio_vban_port".to_owned(),
            "mic_vban_port".to_owned(),
            "camera_streams".to_owned(),
            "frigate_host".to_owned(),
            "frigate_port".to_owned(),
            "sunshine_port".to_owned(),
        ];
        if !vnc.is_configured() {
            unsupported_controller_fields.insert(0, "vnc_port".to_owned());
            unsupported_controller_fields.insert(0, "vnc_host".to_owned());
        }

        Self {
            seat_id,
            vm_ip,
            vnc,
            unsupported_controller_fields,
        }
    }

    /// Insert the resolved fields into the shared mDNS/direct-registration map.
    ///
    /// `vm_ip` and `vm_guest_ip` are both emitted: `controller/discovery.py`
    /// reads the former from mDNS TXT, while `DirectRegisterRequest` reads the
    /// latter from the JSON body. Older consumers keep working.
    pub fn apply_to_txt(&self, txt: &mut HashMap<String, String>) {
        if let Some(seat_id) = &self.seat_id {
            txt.insert("seat_id".to_owned(), seat_id.clone());
        }
        if let VncRegistration::Configured { host, port } = &self.vnc {
            txt.insert("vnc_host".to_owned(), host.clone());
            txt.insert("vnc_port".to_owned(), port.to_string());
        }
        if let Some(vm_ip) = &self.vm_ip {
            txt.insert("vm_ip".to_owned(), vm_ip.clone());
            txt.insert("vm_guest_ip".to_owned(), vm_ip.clone());
        }
    }
}

/// Read the explicit seat identity from the environment.
///
/// The node never invents a seat id: without `OZMA_SEAT_ID` (or with an empty
/// value) the field is omitted and controller grouping falls back to the node
/// name convention.
pub fn seat_id_from_env() -> Option<String> {
    std::env::var(SEAT_ID_ENV).ok().and_then(|value| {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_owned())
    })
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn some_endpoint<'a>(host: &'a str, port: u16) -> Option<(&'a str, u16)> {
    let host = host.trim();
    (!host.is_empty() && port != 0).then_some((host, port))
}

/// Whether a TXT/registration key looks like a credential.
///
/// Secrets must never reach mDNS TXT records, registration payloads, or logs.
/// This is a defensive guard: the node has no configuration that inserts a
/// token into the TXT map today, and a future one must not leak it by accident.
pub fn is_secret_field(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    [
        "token",
        "secret",
        "password",
        "passwd",
        "credential",
        "api_key",
        "apikey",
        "private_key",
    ]
    .iter()
    .any(|marker| key.contains(marker))
}

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
            // `host` is the resolved LAN address; a TXT copy may be the
            // `--register-host` override. Credential-shaped keys are dropped
            // defensively so a future TXT change cannot leak a token.
            if key != "host" && !is_secret_field(key) {
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

    fn desktop_txt() -> HashMap<String, String> {
        HashMap::from([
            ("proto".to_owned(), "1".to_owned()),
            ("role".to_owned(), "compute".to_owned()),
            ("hw".to_owned(), "desktop-linux".to_owned()),
            ("fw".to_owned(), "1.0.0".to_owned()),
            ("cap".to_owned(), "softnode,screen".to_owned()),
            ("api_port".to_owned(), "7382".to_owned()),
            ("stream_port".to_owned(), "7382".to_owned()),
            ("stream_path".to_owned(), "/stream/stream.m3u8".to_owned()),
        ])
    }

    #[test]
    fn desktop_payload_is_exact_and_omits_unavailable_endpoints() {
        let extras = RegistrationExtras::resolve(None, None, None, None);
        let mut txt = desktop_txt();
        extras.apply_to_txt(&mut txt);

        // No seat, VNC, or VM guest identity is invented for a desktop node.
        assert!(!txt.contains_key("seat_id"));
        assert!(!txt.contains_key("vnc_host"));
        assert!(!txt.contains_key("vnc_port"));
        assert!(!txt.contains_key("vm_ip"));
        assert!(!txt.contains_key("vm_guest_ip"));

        let payload = registration_payload("desk-1._ozma._udp.local.", "192.0.2.10", 7331, &txt);
        assert_eq!(
            payload,
            json!({
                "id": "desk-1._ozma._udp.local.",
                "host": "192.0.2.10",
                "port": 7331,
                "proto": "1",
                "role": "compute",
                "hw": "desktop-linux",
                "fw": "1.0.0",
                "cap": "softnode,screen",
                "api_port": "7382",
                "stream_port": "7382",
                "stream_path": "/stream/stream.m3u8",
            })
        );
    }

    #[test]
    fn vm_payload_carries_vm_ip_alias_and_configured_vnc_endpoint() {
        let extras =
            RegistrationExtras::resolve(None, Some("127.0.0.1"), Some(5901), Some("10.0.0.42"));
        let mut txt = HashMap::from([
            ("proto".to_owned(), "1".to_owned()),
            ("role".to_owned(), "compute".to_owned()),
            ("hw".to_owned(), "soft".to_owned()),
            ("fw".to_owned(), "0.1.0".to_owned()),
            ("cap".to_owned(), "qmp,power".to_owned()),
            ("api_port".to_owned(), "7382".to_owned()),
        ]);
        extras.apply_to_txt(&mut txt);

        assert_eq!(txt["vm_ip"], "10.0.0.42");
        assert_eq!(txt["vm_guest_ip"], "10.0.0.42");
        assert_eq!(txt["vnc_host"], "127.0.0.1");
        assert_eq!(txt["vnc_port"], "5901");

        let payload = registration_payload("vm1._ozma._udp.local.", "192.0.2.20", 7331, &txt);
        assert_eq!(
            payload,
            json!({
                "id": "vm1._ozma._udp.local.",
                "host": "192.0.2.20",
                "port": 7331,
                "proto": "1",
                "role": "compute",
                "hw": "soft",
                "fw": "0.1.0",
                "cap": "qmp,power",
                "api_port": "7382",
                "vnc_host": "127.0.0.1",
                "vnc_port": "5901",
                "vm_ip": "10.0.0.42",
                "vm_guest_ip": "10.0.0.42",
            })
        );
    }

    #[test]
    fn served_vnc_endpoint_precedes_the_external_qemu_endpoint() {
        let extras = RegistrationExtras::resolve_with_served_vnc(
            None,
            Some(("192.0.2.44", 49152)),
            Some("127.0.0.1"),
            Some(5901),
            None,
        );
        assert_eq!(
            extras.vnc,
            VncRegistration::Configured {
                host: "192.0.2.44".to_owned(),
                port: 49152,
            }
        );

        // Without a running listener the external endpoint is still honest.
        let external = RegistrationExtras::resolve_with_served_vnc(
            None,
            None,
            Some("127.0.0.1"),
            Some(5901),
            None,
        );
        assert_eq!(
            external.vnc,
            VncRegistration::Configured {
                host: "127.0.0.1".to_owned(),
                port: 5901,
            }
        );

        // A zero/incomplete served or external pair is never advertised.
        let none = RegistrationExtras::resolve_with_served_vnc(
            None,
            Some(("192.0.2.44", 0)),
            Some(""),
            Some(5901),
            None,
        );
        assert_eq!(none.vnc, VncRegistration::NotConfigured);
    }

    #[test]
    fn multiseat_payload_carries_the_explicit_seat_id() {
        let extras = RegistrationExtras::resolve(Some(" rig-1-seat-0 "), None, None, None);
        let mut txt = desktop_txt();
        extras.apply_to_txt(&mut txt);

        assert_eq!(txt["seat_id"], "rig-1-seat-0");

        let payload =
            registration_payload("rig-1-seat-0._ozma._udp.local.", "192.0.2.30", 7351, &txt);
        assert_eq!(payload["seat_id"], "rig-1-seat-0");
        assert!(payload.get("vnc_host").is_none());
        assert!(payload.get("vm_ip").is_none());
    }

    #[test]
    fn empty_or_partial_values_are_omitted_not_sent_as_empty_strings() {
        let extras =
            RegistrationExtras::resolve(Some("   "), Some("127.0.0.1"), Some(0), Some(" "));
        let mut txt = desktop_txt();
        let before = txt.clone();
        extras.apply_to_txt(&mut txt);

        assert_eq!(txt, before);
        assert_eq!(extras.seat_id, None);
        assert_eq!(extras.vm_ip, None);
        assert_eq!(extras.vnc, VncRegistration::NotConfigured);
        assert!(extras
            .unsupported_controller_fields
            .contains(&"vnc_host".to_owned()));
        assert!(extras
            .unsupported_controller_fields
            .contains(&"audio_vban_port".to_owned()));
    }

    #[test]
    fn registration_status_is_typed_for_controller_consumers() {
        let missing = RegistrationExtras::resolve(None, None, None, None);
        assert_eq!(
            serde_json::to_value(&missing).unwrap(),
            json!({
                "vnc": {"state": "not-configured"},
                "unsupported_controller_fields": [
                    "vnc_host",
                    "vnc_port",
                    "audio_vban_port",
                    "mic_vban_port",
                    "camera_streams",
                    "frigate_host",
                    "frigate_port",
                    "sunshine_port",
                ],
            })
        );

        let configured = RegistrationExtras::resolve(
            Some("rig-1-seat-0"),
            Some("127.0.0.1"),
            Some(5901),
            Some("10.0.0.42"),
        );
        assert_eq!(
            serde_json::to_value(&configured).unwrap(),
            json!({
                "seat_id": "rig-1-seat-0",
                "vm_ip": "10.0.0.42",
                "vnc": {"state": "configured", "host": "127.0.0.1", "port": 5901},
                "unsupported_controller_fields": [
                    "audio_vban_port",
                    "mic_vban_port",
                    "camera_streams",
                    "frigate_host",
                    "frigate_port",
                    "sunshine_port",
                ],
            })
        );
    }

    #[test]
    fn secrets_never_reach_mdns_txt_or_registration_payloads() {
        assert!(is_secret_field("api_token"));
        assert!(is_secret_field("turn_password"));
        assert!(is_secret_field("vnc_password"));
        assert!(is_secret_field("private_key"));
        assert!(!is_secret_field("machine_class"));
        assert!(!is_secret_field("stream_path"));

        let mut txt = desktop_txt();
        txt.insert("api_token".to_owned(), "s3cret".to_owned());
        txt.insert("turn_password".to_owned(), "hunter2".to_owned());
        let payload = registration_payload("desk-1._ozma._udp.local.", "192.0.2.10", 7331, &txt);

        let object = payload.as_object().expect("payload object");
        assert!(!object.contains_key("api_token"));
        assert!(!object.contains_key("turn_password"));
        assert!(!payload.to_string().contains("s3cret"));
        assert!(!payload.to_string().contains("hunter2"));
    }
}
