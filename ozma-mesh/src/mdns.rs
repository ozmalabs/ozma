//! mDNS peer discovery via libmdns.

use anyhow::Result;
use libmdns::Responder;
use tracing::info;

/// Advertise this node on the local network via mDNS.
///
/// `name`    — human-readable instance name (e.g. `"ozma-node-1"`)
/// `port`    — UDP port the node listens on
pub async fn advertise(name: &str, port: u16) -> Result<Responder> {
    let responder = Responder::new()?;
    let _svc = responder.register(
        "_ozma._udp".to_owned(),
        name.to_owned(),
        port,
        &["path=/"],
    );
    info!(name, port, "mDNS advertisement registered");
    // Keep _svc alive by returning the responder; caller must hold it.
    Ok(responder)
}
