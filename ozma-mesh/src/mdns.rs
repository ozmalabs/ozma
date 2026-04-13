//! mDNS advertising and browsing for `_ozma._udp.local`.
//!
//! Each node advertises its [`MeshNode`] identity as a TXT record so that
//! peers on the same LAN segment can discover it without a central server.

use std::time::Duration;

use libmdns::{Responder, Service};
use tracing::{debug, info};

use crate::error::MeshError;
use crate::node::MeshNode;

/// mDNS service type for ozma mesh nodes.
pub const MDNS_SERVICE_TYPE: &str = "_ozma._udp";

/// Advertise a [`MeshNode`] over mDNS until the returned [`MdnsHandle`] is
/// dropped.
pub struct MdnsHandle {
    /// Keep the libmdns service alive.
    _service: Service,
    /// Keep the libmdns responder alive.
    _responder: Responder,
}

impl MdnsHandle {
    /// Start advertising `node` on the local network.
    pub fn advertise(node: &MeshNode) -> Result<Self, MeshError> {
        let responder = Responder::new()
            .map_err(|e| MeshError::MdnsError(format!("responder: {e}")))?;

        // Encode the node identity as TXT record properties.
        // libmdns takes &[&str] of "key=value" pairs.
        let txt_id = format!("id={}", node.id);
        let txt_pubkey = format!("pubkey={}", node.wg_pubkey);
        let txt_ip = format!("mesh_ip={}", node.mesh_ip);
        let properties: &[&str] = &[&txt_id, &txt_pubkey, &txt_ip];

        let service = responder.register(
            MDNS_SERVICE_TYPE.to_owned(),
            node.id.clone(),
            node.wg_port,
            properties,
        );

        info!(
            node_id = %node.id,
            mesh_ip = %node.mesh_ip,
            port = node.wg_port,
            "mDNS: advertising {}.{}.local",
            node.id,
            MDNS_SERVICE_TYPE,
        );

        Ok(Self {
            _service: service,
            _responder: responder,
        })
    }
}

/// Browse for `_ozma._udp.local` peers and call `on_peer` for each one found.
///
/// This is a **blocking** browse that runs for `duration` then returns.
/// In production, run this in a `tokio::task::spawn_blocking` loop.
pub fn browse_peers(
    duration: Duration,
    mut on_peer: impl FnMut(MeshNode),
) -> Result<(), MeshError> {
    use libmdns::browse;

    let receiver = browse(MDNS_SERVICE_TYPE)
        .map_err(|e| MeshError::MdnsError(format!("browse: {e}")))?;

    let deadline = std::time::Instant::now() + duration;

    while std::time::Instant::now() < deadline {
        match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(event) => {
                debug!("mDNS browse event: {:?}", event);
                if let Some(node) = parse_mdns_event(event) {
                    on_peer(node);
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    Ok(())
}

/// Parse a libmdns browse event into a [`MeshNode`] if it contains the
/// required TXT properties.
fn parse_mdns_event(event: libmdns::BrowseEvent) -> Option<MeshNode> {
    use libmdns::BrowseEvent;
    match event {
        BrowseEvent::Resolved {
            name,
            port,
            txt,
            ..
        } => {
            let mut id = None;
            let mut pubkey = None;
            let mut mesh_ip = None;

            for entry in &txt {
                if let Some(v) = entry.strip_prefix("id=") {
                    id = Some(v.to_owned());
                } else if let Some(v) = entry.strip_prefix("pubkey=") {
                    pubkey = Some(v.to_owned());
                } else if let Some(v) = entry.strip_prefix("mesh_ip=") {
                    mesh_ip = Some(v.to_owned());
                }
            }

            match (id, pubkey, mesh_ip) {
                (Some(id), Some(pk), Some(ip)) => {
                    debug!(node_id = %id, "mDNS: resolved peer");
                    Some(MeshNode::new(
                        id,
                        crate::keys::WgPublicKey(pk),
                        ip,
                        port,
                    ))
                }
                _ => {
                    debug!(name = %name, "mDNS: resolved service missing TXT fields, skipping");
                    None
                }
            }
        }
        _ => None,
    }
}
