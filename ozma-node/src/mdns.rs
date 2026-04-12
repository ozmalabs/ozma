//! mDNS advertisement for `_ozma._udp.local.`
//!
//! Wraps `libmdns` to advertise the node's service record with TXT properties.

use std::collections::HashMap;

use libmdns::Responder;
use tracing::debug;

pub const SERVICE_TYPE: &str = "_ozma._udp.local.";

/// Live mDNS handle — keeps the responder and service registration alive.
/// Dropping this unregisters the mDNS record.
pub struct MdnsHandle {
    // Both fields must be kept alive for the advertisement to remain active.
    _service: libmdns::Service,
    _responder: Responder,
}

/// Unified handle returned by [`advertise`] so callers don't need to match on
/// `Result` just to store the value.
pub enum Handle {
    Real(MdnsHandle),
    Dummy,
}

/// Advertise `<name>._ozma._udp.local.` on the local network.
///
/// `txt` entries are encoded as `key=value` strings in the DNS-SD TXT record,
/// matching the format used by `node.py`'s `ServiceInfo(properties=txt)`.
pub fn advertise(
    name: &str,
    port: u16,
    txt: &HashMap<String, String>,
) -> Result<MdnsHandle, Box<dyn std::error::Error + Send + Sync>> {
    // Build TXT record properties: ["key=value", ...]
    let properties: Vec<String> = txt
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();

    // libmdns expects &[&str]
    let prop_refs: Vec<&str> = properties.iter().map(|s| s.as_str()).collect();

    debug!(
        service = %format!("{name}.{SERVICE_TYPE}"),
        port,
        txt = ?properties,
        "Registering mDNS service"
    );

    let responder = Responder::new()?;
    let service = responder.register(
        SERVICE_TYPE.to_string(),
        name.to_string(),
        port,
        &prop_refs,
    );

    Ok(MdnsHandle {
        _service: service,
        _responder: responder,
    })
}
