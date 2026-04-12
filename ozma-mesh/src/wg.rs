//! WireGuard tunnel management via boringtun.

use anyhow::Result;
use boringtun::noise::{Tunn, TunnResult};
use std::sync::Arc;
use tokio::net::UdpSocket;
use tracing::{debug, warn};

/// A single WireGuard peer tunnel.
pub struct WgTunnel {
    tunn: Arc<Tunn>,
    socket: Arc<UdpSocket>,
}

impl WgTunnel {
    /// Wrap an existing boringtun [`Tunn`] and UDP socket.
    pub fn new(tunn: Tunn, socket: UdpSocket) -> Self {
        Self {
            tunn: Arc::new(tunn),
            socket: Arc::new(socket),
        }
    }

    /// Encapsulate `plaintext` and send it to `peer_addr`.
    pub async fn send(&self, plaintext: &[u8], peer_addr: std::net::SocketAddr) -> Result<()> {
        let mut dst = vec![0u8; plaintext.len() + 148]; // WG overhead
        match self.tunn.encapsulate(plaintext, &mut dst) {
            TunnResult::WriteToNetwork(pkt) => {
                self.socket.send_to(pkt, peer_addr).await?;
                debug!(bytes = pkt.len(), "WG encapsulated → sent");
            }
            other => warn!(?other, "unexpected encapsulate result"),
        }
        Ok(())
    }
}
