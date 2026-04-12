//! WireGuard tunnel management via boringtun.
//!
//! Each [`WgTunnel`] wraps a boringtun [`Tunn`] instance and a UDP socket.
//! Packets are read from the socket, decapsulated by boringtun, and the
//! plaintext is returned to the caller (and vice-versa for outbound traffic).
//!
//! In production the plaintext side is wired to a TUN interface; in tests
//! the plaintext side is inspected directly.

use std::net::SocketAddr;
use std::sync::Arc;

use boringtun::noise::{Tunn, TunnResult};
use tokio::net::UdpSocket;
use tracing::{debug, trace, warn};

use crate::error::MeshError;
use crate::keys::WgKeypair;
use crate::node::MeshNode;

/// Maximum buffer size for WireGuard packets.
const WG_BUF_SIZE: usize = 65535;

/// A single WireGuard peer tunnel (one per remote peer).
pub struct WgTunnel {
    /// boringtun tunnel state machine.
    inner: Box<Tunn>,
    /// Shared UDP socket (all peers share one socket on the local node).
    socket: Arc<UdpSocket>,
    /// Remote endpoint address.
    peer_addr: SocketAddr,
    /// Remote peer's mesh node info.
    pub peer: MeshNode,
}

impl WgTunnel {
    /// Create a new tunnel to `peer` using `keypair` as the local identity.
    ///
    /// `socket` must already be bound to the local WireGuard port.
    pub fn new(
        keypair: &WgKeypair,
        peer: MeshNode,
        peer_addr: SocketAddr,
        socket: Arc<UdpSocket>,
    ) -> Result<Self, MeshError> {
        let peer_pubkey_bytes = peer.wg_pubkey.to_bytes()?;
        let peer_static_public = x25519_dalek::PublicKey::from(peer_pubkey_bytes);

        let tunn = Tunn::new(
            keypair.private_bytes().into(),
            peer_static_public,
            None,  // no pre-shared key
            None,  // default keepalive
            0,     // index
            None,  // rate limiter
        )
        .map_err(|e| MeshError::TunnelError(format!("boringtun init: {e}")))?;

        Ok(Self {
            inner: tunn,
            socket,
            peer_addr,
            peer,
        })
    }

    /// Encapsulate a plaintext IP packet and send it to the peer.
    pub async fn send_plaintext(&mut self, plaintext: &[u8]) -> Result<(), MeshError> {
        let mut dst = vec![0u8; WG_BUF_SIZE];
        match self.inner.encapsulate(plaintext, &mut dst) {
            TunnResult::WriteToNetwork(packet) => {
                self.socket.send_to(packet, self.peer_addr).await?;
                trace!(peer = %self.peer.id, bytes = packet.len(), "WG encapsulated → sent");
            }
            TunnResult::Err(e) => {
                return Err(MeshError::TunnelError(format!("encapsulate: {e:?}")));
            }
            other => {
                debug!(peer = %self.peer.id, result = ?other, "encapsulate: unexpected result");
            }
        }
        Ok(())
    }

    /// Feed a raw UDP packet received from the network into boringtun.
    ///
    /// Returns the decapsulated plaintext IP packet if one is ready, or
    /// `None` if the packet was a handshake/keepalive with no payload yet.
    pub async fn receive_packet(
        &mut self,
        udp_packet: &[u8],
    ) -> Result<Option<Vec<u8>>, MeshError> {
        let mut dst = vec![0u8; WG_BUF_SIZE];
        let mut out_buf = vec![0u8; WG_BUF_SIZE];

        match self.inner.decapsulate(None, udp_packet, &mut dst) {
            TunnResult::WriteToNetwork(reply) => {
                // Handshake response — send it back, no plaintext yet.
                self.socket.send_to(reply, self.peer_addr).await?;
                // Drain any queued packets now that the handshake completed.
                loop {
                    match self.inner.decapsulate(None, &[], &mut out_buf) {
                        TunnResult::WriteToTunnelV4(pkt, _) | TunnResult::WriteToTunnelV6(pkt, _) => {
                            return Ok(Some(pkt.to_vec()));
                        }
                        TunnResult::WriteToNetwork(pkt) => {
                            self.socket.send_to(pkt, self.peer_addr).await?;
                        }
                        _ => break,
                    }
                }
                Ok(None)
            }
            TunnResult::WriteToTunnelV4(pkt, _src) => {
                trace!(peer = %self.peer.id, bytes = pkt.len(), "WG decapsulated IPv4");
                Ok(Some(pkt.to_vec()))
            }
            TunnResult::WriteToTunnelV6(pkt, _src) => {
                trace!(peer = %self.peer.id, bytes = pkt.len(), "WG decapsulated IPv6");
                Ok(Some(pkt.to_vec()))
            }
            TunnResult::Err(e) => {
                warn!(peer = %self.peer.id, error = ?e, "WG decapsulate error");
                Err(MeshError::TunnelError(format!("decapsulate: {e:?}")))
            }
            TunnResult::Done => Ok(None),
        }
    }

    /// Drive boringtun's timer (call periodically, e.g. every 100 ms).
    ///
    /// Sends keepalive / handshake-initiation packets as needed.
    pub async fn tick(&mut self) -> Result<(), MeshError> {
        let mut dst = vec![0u8; WG_BUF_SIZE];
        match self.inner.update_timers(&mut dst) {
            TunnResult::WriteToNetwork(pkt) => {
                self.socket.send_to(pkt, self.peer_addr).await?;
            }
            TunnResult::Err(e) => {
                return Err(MeshError::TunnelError(format!("timer: {e:?}")));
            }
            _ => {}
        }
        Ok(())
    }
}
