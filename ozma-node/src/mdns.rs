//! mDNS advertisement for `_ozma._udp.local.`
//!
//! Wraps `libmdns` to advertise the node's service record with TXT properties.

use std::collections::HashMap;
use std::net::IpAddr;

use libmdns::Responder;
use tracing::debug;

/// Fully-qualified DNS-SD service type used by the controller and Python nodes.
pub const SERVICE_TYPE: &str = "_ozma._udp.local.";

/// `libmdns::Responder::register` appends `.local` itself, so it must be given
/// the *unqualified* type. Passing [`SERVICE_TYPE`] here produces the malformed
/// name `_ozma._udp.local..local`, whose embedded empty label corrupts every
/// emitted DNS record (browsers drop the response; libmdns also logs
/// `type 1388 is invalid` when it loops back its own packet).
const LIBMDNS_SERVICE_TYPE: &str = "_ozma._udp";

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
    advertise_on(name, port, txt, None)
}

/// Advertise with an explicit interface address.
///
/// `libmdns` otherwise publishes every non-loopback interface, including
/// Docker/libvirt bridges. The controller takes the first resolved address, so
/// an unreachable bridge address (for example `192.168.122.1`) can be chosen
/// instead of the node's LAN address. Pass the address the node uses for its
/// own API (`--register-host` or `local_ip()`) to publish only that A record.
///
/// Loopback is ignored: `libmdns` never emits loopback A records, so a
/// loopback-only host still gets no address record.
pub fn advertise_on(
    name: &str,
    port: u16,
    txt: &HashMap<String, String>,
    address: Option<IpAddr>,
) -> Result<MdnsHandle, Box<dyn std::error::Error + Send + Sync>> {
    // Secrets (API tokens, credentials) must never be published in a TXT
    // record, and the debug log below prints the properties. Reject before
    // formatting or logging anything.
    if let Some((key, _)) = txt
        .iter()
        .find(|(key, _)| crate::registration::is_secret_field(key))
    {
        return Err(format!(
            "refusing to advertise credential-shaped mDNS TXT key `{key}`; \
             secrets must not leave the node in discovery records"
        )
        .into());
    }

    // Build TXT record properties: ["key=value", ...]
    let properties: Vec<String> = txt.iter().map(|(k, v)| format!("{k}={v}")).collect();

    // DNS-SD TXT character-strings are length-prefixed by a single byte.
    // libmdns panics on a longer entry, so reject it here with a typed error.
    if let Some(entry) = properties.iter().find(|entry| entry.len() > 255) {
        return Err(format!(
            "mDNS TXT entry is {} bytes (limit 255): {}",
            entry.len(),
            entry.split('=').next().unwrap_or(entry)
        )
        .into());
    }

    // libmdns expects &[&str]
    let prop_refs: Vec<&str> = properties.iter().map(|s| s.as_str()).collect();

    debug!(
        service = %format!("{name}.{SERVICE_TYPE}"),
        port,
        address = ?address,
        txt = ?properties,
        "Registering mDNS service"
    );

    let responder = match address {
        Some(address) if !address.is_loopback() => Responder::new_with_ip_list(vec![address])?,
        _ => Responder::new()?,
    };
    let service = responder.register(
        LIBMDNS_SERVICE_TYPE.to_string(),
        name.to_string(),
        port,
        &prop_refs,
    );

    Ok(MdnsHandle {
        _service: service,
        _responder: responder,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: `libmdns` appends `.local` to both the type and instance
    /// arguments. Passing the FQDN `SERVICE_TYPE` produced
    /// `_ozma._udp.local..local`, an embedded empty DNS label that made every
    /// advertised record unparseable.
    #[test]
    fn libmdns_names_match_the_dns_sd_wire_format() {
        assert_eq!(
            format!("{LIBMDNS_SERVICE_TYPE}.local"),
            SERVICE_TYPE.trim_end_matches('.')
        );
        assert_eq!(
            format!("probe.{LIBMDNS_SERVICE_TYPE}.local"),
            "probe._ozma._udp.local"
        );
        assert!(!LIBMDNS_SERVICE_TYPE.ends_with(".local"));
        assert!(!format!("probe.{LIBMDNS_SERVICE_TYPE}.local").contains(".."));
    }

    #[test]
    fn oversized_txt_entry_is_rejected_before_advertising() {
        let mut txt = HashMap::new();
        txt.insert("display_outputs".to_owned(), "x".repeat(256));
        let error = advertise("oversized", 7331, &txt)
            .err()
            .expect("oversized TXT must be rejected");
        assert!(error.to_string().contains("limit 255"), "{error}");
    }

    #[test]
    fn credential_shaped_txt_keys_are_rejected_before_advertising() {
        let mut txt = HashMap::new();
        txt.insert("proto".to_owned(), "1".to_owned());
        txt.insert("api_token".to_owned(), "s3cret".to_owned());
        let error = advertise("secret-node", 7331, &txt)
            .err()
            .expect("secret TXT key must be rejected");
        let message = error.to_string();
        assert!(message.contains("api_token"), "{message}");
        assert!(
            !message.contains("s3cret"),
            "the rejection must not echo the secret value: {message}"
        );
    }

    #[test]
    fn txt_entries_use_python_compatible_key_equals_value_form() {
        let mut txt = HashMap::new();
        txt.insert("proto".to_owned(), "1".to_owned());
        txt.insert("cap".to_owned(), "hid,video".to_owned());
        let properties: Vec<String> = txt.iter().map(|(k, v)| format!("{k}={v}")).collect();
        assert!(properties.contains(&"proto=1".to_owned()));
        assert!(properties.contains(&"cap=hid,video".to_owned()));
    }
}
