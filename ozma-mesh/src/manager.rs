//! [`MeshManager`] — WireGuard tunnel management + mDNS peer discovery.
//!
//! # Design
//!
//! Each `MeshManager` instance represents one node in the ozma mesh.  It:
//!
//! 1. Owns a userspace WireGuard tunnel per peer (via `boringtun`).
//! 2. Advertises itself over mDNS (`_ozma._udp.local`) so other nodes on the
//!    same LAN segment discover each other without a central registry.
//! 3. Maintains a peer table — add/remove peers at runtime.
//! 4. Runs an async receive loop (started by [`MeshManager::start`]) that
//!    decapsulates incoming WireGuard packets and auto-replies to handshakes.
//!
//! # Thread / async safety
//!
//! `MeshManager` is `Send + Sync`.  All mutable peer state is protected by a
//! `tokio::sync::RwLock`.
//!
//! # Example
//!
//! ```rust,no_run
//! use ozma_mesh::{MeshManager, MeshNode};
//!
//! #[tokio::main]
//! async fn main() -> ozma_mesh::error::Result<()> {
//!     let (node, sk) = MeshNode::generate("my-node", "10.200.1.1", 51820);
//!     let mgr = MeshManager::new(node, sk).await?;
//!     mgr.start().await?;
//!     Ok(())
//! }
//! ```

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::Arc;

use boringtun::noise::{Tunn, TunnResult};
use libmdns::{Responder, Service};
use tokio::net::UdpSocket as TokioUdpSocket;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};
use x25519_dalek::{PublicKey as X25519PublicKey, StaticSecret};

use crate::error::{MeshError, Result};
use crate::node::{MeshNode, WgPrivateKey};

// ── Per-peer state ────────────────────────────────────────────────────────────

struct PeerState {
    node: MeshNode,
    /// boringtun userspace WireGuard tunnel for this peer.
    tunnel: Box<Tunn>,
}

// ── Lock-protected inner state ────────────────────────────────────────────────

pub(crate) struct Inner {
    pub(crate) peers: HashMap<String, PeerState>,
}

// ── MeshManager ───────────────────────────────────────────────────────────────

/// Manages WireGuard peers and mDNS advertising for one ozma mesh node.
pub struct MeshManager {
    /// This node's public identity.
    pub node: MeshNode,

    pub(crate) inner: Arc<RwLock<Inner>>,

    /// UDP socket used to send/receive WireGuard-encapsulated packets.
    pub socket: Arc<UdpSocket>,

    /// mDNS responder — kept alive for the lifetime of the manager.
    _mdns_responder: Responder,

    /// mDNS service registration handle.
    _mdns_service: Service,

    /// This node's private key (needed to create per-peer boringtun tunnels).
    private_key: WgPrivateKey,
}

impl MeshManager {
    /// Create a new `MeshManager` for `node`, using `private_key` as the local
    /// WireGuard identity.
    ///
    /// Binds a non-blocking UDP socket on `node.wg_port` (use `0` for an
    /// OS-assigned ephemeral port) and registers an mDNS service
    /// `_ozma._udp.local` advertising the node's id, mesh IP, and public key.
    pub async fn new(node: MeshNode, private_key: WgPrivateKey) -> Result<Self> {
        // Bind the WireGuard UDP socket on all interfaces.
        // Use port 0 to let the OS pick an ephemeral port when wg_port == 0.
        let bind_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), node.wg_port);
        let socket = UdpSocket::bind(bind_addr)
            .map_err(|e| MeshError::TunnelError(format!("bind {bind_addr}: {e}")))?;
        socket
            .set_nonblocking(true)
            .map_err(|e| MeshError::TunnelError(format!("set_nonblocking: {e}")))?;

        let actual_port = socket.local_addr()?.port();

        info!(
            node_id = %node.id,
            mesh_ip = %node.mesh_ip,
            wg_port = actual_port,
            pubkey = %node.wg_pubkey,
            "MeshManager: WireGuard socket bound"
        );

        // Advertise over mDNS so peers on the same LAN can discover us.
        let responder = Responder::new().map_err(|e| MeshError::Mdns(e.to_string()))?;
        let svc = responder.register(
            "_ozma._udp".into(),
            node.id.clone(),
            actual_port,
            &[
                &format!("mesh_ip={}", node.mesh_ip),
                &format!("pubkey={}", node.wg_pubkey),
            ],
        );

        info!(node_id = %node.id, "MeshManager: mDNS service registered (_ozma._udp.local)");

        Ok(Self {
            node,
            inner: Arc::new(RwLock::new(Inner {
                peers: HashMap::new(),
            })),
            socket: Arc::new(socket),
            _mdns_responder: responder,
            _mdns_service: svc,
            private_key,
        })
    }

    /// Return the UDP port this manager is actually listening on.
    ///
    /// Useful when `wg_port = 0` was passed to [`MeshNode::generate`] and the
    /// OS assigned an ephemeral port.
    pub fn wg_port(&self) -> u16 {
        self.socket.local_addr().unwrap().port()
    }

    /// Start the background async receive loop (spawns a Tokio task).
    ///
    /// The loop reads WireGuard-encapsulated UDP datagrams, routes each packet
    /// to the matching peer tunnel for decapsulation, and automatically replies
    /// to WireGuard handshake messages.
    pub async fn start(&self) -> Result<()> {
        let socket_clone = self
            .socket
            .try_clone()
            .map_err(|e| MeshError::TunnelError(format!("socket clone for recv loop: {e}")))?;
        // from_std requires the socket to already be non-blocking (set in new())
        let async_sock = TokioUdpSocket::from_std(socket_clone)
            .map_err(|e| MeshError::TunnelError(format!("tokio UdpSocket::from_std: {e}")))?;
        let inner = Arc::clone(&self.inner);

        tokio::spawn(async move {
            let mut buf = vec![0u8; 65535];
            let mut out = vec![0u8; 65535];

            loop {
                match async_sock.recv_from(&mut buf).await {
                    Err(e) => {
                        warn!("WireGuard recv loop error: {e}");
                        break;
                    }
                    Ok((n, src)) => {
                        let pkt = &buf[..n];
                        let mut guard = inner.write().await;

                        // Find the peer whose endpoint matches `src`.
                        let peer = guard.peers.values_mut().find(|p| {
                            p.node
                                .mesh_ip
                                .parse::<Ipv4Addr>()
                                .map(|ip| {
                                    SocketAddr::new(IpAddr::V4(ip), p.node.wg_port) == src
                                })
                                .unwrap_or(false)
                        });

                        if let Some(peer) = peer {
                            match peer.tunnel.decapsulate(None, pkt, &mut out) {
                                TunnResult::WriteToTunnelV4(data, _) => {
                                    debug!(src = %src, bytes = data.len(),
                                           "WireGuard: decapsulated IPv4 packet");
                                }
                                TunnResult::WriteToTunnelV6(data, _) => {
                                    debug!(src = %src, bytes = data.len(),
                                           "WireGuard: decapsulated IPv6 packet");
                                }
                                TunnResult::WriteToNetwork(data) => {
                                    // Handshake response — send it back.
                                    if let Err(e) = async_sock.send_to(data, src).await {
                                        warn!("WireGuard: failed to send handshake response: {e}");
                                    }
                                }
                                TunnResult::Err(e) => {
                                    warn!(src = %src, "WireGuard tunnel error: {e:?}");
                                }
                                TunnResult::Done => {}
                                _ => {}
                            }
                        } else {
                            debug!(src = %src, "WireGuard: packet from unknown peer, ignoring");
                        }
                    }
                }
            }
        });

        Ok(())
    }

    // ── Peer management ───────────────────────────────────────────────────────

    /// Add a remote peer and create a WireGuard tunnel to it.
    ///
    /// `peer_node.mesh_ip` and `peer_node.wg_port` are used as the UDP
    /// endpoint for outgoing packets.
    ///
    /// Returns [`MeshError::PeerAlreadyExists`] if a peer with the same id is
    /// already registered.
    pub async fn add_peer(&self, peer_node: MeshNode) -> Result<()> {
        let mut guard = self.inner.write().await;

        if guard.peers.contains_key(&peer_node.id) {
            return Err(MeshError::PeerAlreadyExists(peer_node.id.clone()));
        }

        // Decode keys for boringtun.
        let local_sk = StaticSecret::from(self.private_key.to_bytes());
        let peer_pk_bytes = peer_node.wg_pubkey.to_bytes()?;
        let peer_pk = X25519PublicKey::from(peer_pk_bytes);

        // Tunn::new(static_private, peer_public, preshared_key, keepalive, index, rate_limiter)
        let tunnel = Tunn::new(
            local_sk,
            peer_pk,
            None,       // no preshared key
            Some(25),   // persistent keepalive every 25 s
            guard.peers.len() as u32,
            None,
        )
        .map_err(|e| MeshError::TunnelError(format!("Tunn::new for {}: {e}", peer_node.id)))?;

        info!(
            peer_id    = %peer_node.id,
            peer_ip    = %peer_node.mesh_ip,
            peer_port  = peer_node.wg_port,
            peer_pubkey = %peer_node.wg_pubkey,
            "MeshManager: peer added"
        );

        guard.peers.insert(
            peer_node.id.clone(),
            PeerState { node: peer_node, tunnel: Box::new(tunnel) },
        );

        Ok(())
    }

    /// Remove a peer and tear down its WireGuard tunnel.
    ///
    /// Returns [`MeshError::PeerNotFound`] if no such peer exists.
    pub async fn remove_peer(&self, peer_id: &str) -> Result<()> {
        let mut guard = self.inner.write().await;
        if guard.peers.remove(peer_id).is_none() {
            return Err(MeshError::PeerNotFound(peer_id.to_owned()));
        }
        info!(peer_id = %peer_id, "MeshManager: peer removed");
        Ok(())
    }

    /// Return a snapshot of all currently registered peers.
    pub async fn list_peers(&self) -> Vec<MeshNode> {
        let guard = self.inner.read().await;
        guard.peers.values().map(|p| p.node.clone()).collect()
    }

    // ── Packet I/O ────────────────────────────────────────────────────────────

    /// Initiate a WireGuard handshake with `peer_id`.
    ///
    /// Generates a handshake-initiation packet via boringtun and sends it to
    /// the peer's UDP endpoint.  The background receive loop (started by
    /// [`start`]) will process the response automatically.
    ///
    /// [`start`]: MeshManager::start
    pub async fn initiate_handshake(&self, peer_id: &str) -> Result<()> {
        // Resolve the peer's UDP endpoint.
        let peer_addr = {
            let guard = self.inner.read().await;
            let peer = guard
                .peers
                .get(peer_id)
                .ok_or_else(|| MeshError::PeerNotFound(peer_id.to_owned()))?;
            let ip: Ipv4Addr = peer
                .node
                .mesh_ip
                .parse()
                .map_err(|_| MeshError::InvalidIp(peer.node.mesh_ip.clone()))?;
            SocketAddr::new(IpAddr::V4(ip), peer.node.wg_port)
        };

        // Generate the handshake initiation packet (148 bytes).
        let mut out = vec![0u8; 148];
        let result = {
            let mut guard = self.inner.write().await;
            let peer = guard
                .peers
                .get_mut(peer_id)
                .ok_or_else(|| MeshError::PeerNotFound(peer_id.to_owned()))?;
            peer.tunnel.format_handshake_initiation(&mut out, false)
        };

        match result {
            TunnResult::WriteToNetwork(data) => {
                let async_sock = TokioUdpSocket::from_std(
                    self.socket.try_clone().map_err(|e| {
                        MeshError::TunnelError(format!("socket clone: {e}"))
                    })?,
                )
                .map_err(|e| MeshError::TunnelError(format!("from_std: {e}")))?;
                async_sock.send_to(data, peer_addr).await.map_err(|e| {
                    MeshError::TunnelError(format!("send handshake to {peer_addr}: {e}"))
                })?;
                debug!(peer_id = %peer_id, endpoint = %peer_addr,
                       "WireGuard: handshake initiation sent");
            }
            TunnResult::Err(e) => {
                return Err(MeshError::TunnelError(format!(
                    "format_handshake_initiation for {peer_id}: {e:?}"
                )));
            }
            _ => {}
        }

        Ok(())
    }

    /// Encapsulate a plaintext payload and send it to `peer_id` over WireGuard.
    ///
    /// Returns the number of bytes written to the wire.
    pub async fn send_to_peer(&self, peer_id: &str, plaintext: &[u8]) -> Result<usize> {
        let peer_addr = {
            let guard = self.inner.read().await;
            let peer = guard
                .peers
                .get(peer_id)
                .ok_or_else(|| MeshError::PeerNotFound(peer_id.to_owned()))?;
            let ip: Ipv4Addr = peer
                .node
                .mesh_ip
                .parse()
                .map_err(|_| MeshError::InvalidIp(peer.node.mesh_ip.clone()))?;
            SocketAddr::new(IpAddr::V4(ip), peer.node.wg_port)
        };

        // WireGuard overhead is at most 148 bytes (header + auth tag).
        let mut buf = vec![0u8; plaintext.len() + 148];
        let result = {
            let mut guard = self.inner.write().await;
            let peer = guard
                .peers
                .get_mut(peer_id)
                .ok_or_else(|| MeshError::PeerNotFound(peer_id.to_owned()))?;
            peer.tunnel.encapsulate(plaintext, &mut buf)
        };

        match result {
            TunnResult::WriteToNetwork(packet) => {
                let async_sock = TokioUdpSocket::from_std(
                    self.socket.try_clone().map_err(|e| {
                        MeshError::TunnelError(format!("socket clone: {e}"))
                    })?,
                )
                .map_err(|e| MeshError::TunnelError(format!("from_std: {e}")))?;
                let sent = async_sock.send_to(packet, peer_addr).await?;
                debug!(peer_id = %peer_id, bytes = sent, "WireGuard: packet sent");
                Ok(sent)
            }
            TunnResult::Err(e) => Err(MeshError::TunnelError(format!("{e:?}"))),
            other => {
                warn!(peer_id = %peer_id, result = ?other, "WireGuard: unexpected encapsulate result");
                Ok(0)
            }
        }
    }
}
