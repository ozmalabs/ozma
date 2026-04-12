//! [`MeshManager`] — WireGuard peer management + mDNS advertising.
//!
//! # Design
//!
//! Each `MeshManager` owns:
//!   - A WireGuard keypair (generated at construction).
//!   - An in-memory peer table (`id → PeerState`).
//!   - A boringtun `Tunn` per peer for userspace WireGuard packet processing.
//!   - A non-blocking UDP socket for WireGuard traffic.
//!   - An mDNS responder that advertises `_ozma._udp.local`.
//!
//! Packet I/O is synchronous and non-blocking so the manager can be used
//! from both sync and async contexts.  The integration test drives it from
//! plain `#[test]` functions.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::{Arc, RwLock};

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use boringtun::noise::{Tunn, TunnResult};
use libmdns::{Responder, Service};
use tracing::{debug, info, warn};
use x25519_dalek::PublicKey;

use crate::error::MeshError;
use crate::node::{generate_keypair, MeshNode, WgPrivateKey, WgPublicKey};

// ── Per-peer state ────────────────────────────────────────────────────────────

struct PeerState {
    node: MeshNode,
    /// boringtun tunnel for this peer.
    tunnel: Box<Tunn>,
    /// UDP endpoint we send WireGuard packets to.
    endpoint: SocketAddr,
}

// ── Lock-protected inner state ────────────────────────────────────────────────

struct Inner {
    local_node: MeshNode,
    private_key: WgPrivateKey,
    peers: HashMap<String, PeerState>,
}

// ── MeshManager ───────────────────────────────────────────────────────────────

/// Manages WireGuard peers and mDNS discovery for an ozma mesh node.
///
/// # Example
///
/// ```no_run
/// use ozma_mesh::MeshManager;
///
/// let mgr = MeshManager::new("my-node", "10.200.1.1", 51820).unwrap();
/// println!("Public key: {}", mgr.local_node().wg_pubkey);
/// ```
pub struct MeshManager {
    pub(crate) inner: Arc<RwLock<Inner>>,
    /// UDP socket used to send/receive WireGuard-encapsulated packets.
    socket: Arc<UdpSocket>,
    /// mDNS responder — kept alive for the lifetime of the manager.
    _mdns_responder: Responder,
    /// mDNS service registration handle.
    _mdns_service: Service,
}

impl MeshManager {
    /// Create a new `MeshManager` with a freshly generated WireGuard keypair.
    ///
    /// `node_id`  — stable identifier for this node (e.g. `"living-room-node"`).
    /// `mesh_ip`  — IPv4 address assigned to this node on the mesh.
    /// `wg_port`  — UDP port to listen on (pass `0` for OS-assigned).
    pub fn new(node_id: &str, mesh_ip: &str, wg_port: u16) -> Result<Self, MeshError> {
        let (private_key, public_key) = generate_keypair();
        Self::new_with_key(node_id, mesh_ip, wg_port, private_key, public_key)
    }

    /// Create a `MeshManager` with an explicit keypair (useful for key
    /// persistence across restarts).
    pub fn new_with_key(
        node_id: &str,
        mesh_ip: &str,
        wg_port: u16,
        private_key: WgPrivateKey,
        public_key: WgPublicKey,
    ) -> Result<Self, MeshError> {
        // Bind a non-blocking UDP socket for WireGuard traffic.
        let bind_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), wg_port);
        let socket = UdpSocket::bind(bind_addr)?;
        socket.set_nonblocking(true)?;
        let socket = Arc::new(socket);

        let actual_port = socket.local_addr()?.port();

        let local_node = MeshNode {
            id: node_id.to_string(),
            wg_pubkey: public_key,
            mesh_ip: mesh_ip.to_string(),
        };

        // Advertise via mDNS so peers on the same LAN can discover us.
        let responder = Responder::new().map_err(|e| MeshError::MdnsError(e.to_string()))?;
        let service = responder.register(
            "_ozma._udp".to_string(),
            node_id.to_string(),
            actual_port,
            &[&format!("mesh_ip={mesh_ip}")],
        );

        info!(
            node_id = %node_id,
            mesh_ip = %mesh_ip,
            wg_port = actual_port,
            pubkey = %local_node.wg_pubkey,
            "MeshManager started",
        );

        Ok(Self {
            inner: Arc::new(RwLock::new(Inner {
                local_node,
                private_key,
                peers: HashMap::new(),
            })),
            socket,
            _mdns_responder: responder,
            _mdns_service: service,
        })
    }

    // ── Accessors ─────────────────────────────────────────────────────────────

    /// Return this node's [`MeshNode`] identity.
    pub fn local_node(&self) -> MeshNode {
        self.inner.read().unwrap().local_node.clone()
    }

    /// Return the UDP port this manager is listening on.
    pub fn wg_port(&self) -> u16 {
        self.socket.local_addr().unwrap().port()
    }

    // ── Peer management ───────────────────────────────────────────────────────

    /// Add a remote peer and establish a WireGuard tunnel to it.
    ///
    /// `peer`     — the remote node's identity (must include its public key).
    /// `endpoint` — the UDP address where the peer's WireGuard listener is
    ///              reachable (e.g. `"127.0.0.1:51821"`).
    pub fn add_peer(&self, peer: MeshNode, endpoint: SocketAddr) -> Result<(), MeshError> {
        let mut inner = self.inner.write().unwrap();

        if inner.peers.contains_key(&peer.id) {
            return Err(MeshError::PeerAlreadyExists(peer.id.clone()));
        }

        // Decode the peer's public key bytes.
        let peer_pubkey_bytes = peer.wg_pubkey.to_bytes()?;
        let peer_pubkey = PublicKey::from(peer_pubkey_bytes);

        // boringtun requires the private key as a raw [u8; 32].
        let static_private: [u8; 32] = inner.private_key.to_bytes();

        let tunnel = Tunn::new(
            static_private.into(),
            peer_pubkey,
            None,                           // no preshared key
            None,                           // default persistent keepalive
            inner.peers.len() as u32,       // tunnel index (must be unique)
            None,                           // no rate limiter
        )
        .map_err(|e| MeshError::TunnelError(e.to_string()))?;

        info!(
            peer_id = %peer.id,
            peer_ip = %peer.mesh_ip,
            %endpoint,
            "WireGuard peer added",
        );

        inner.peers.insert(
            peer.id.clone(),
            PeerState { node: peer, tunnel: Box::new(tunnel), endpoint },
        );

        Ok(())
    }

    /// Remove a peer and tear down its WireGuard tunnel.
    pub fn remove_peer(&self, peer_id: &str) -> Result<(), MeshError> {
        let mut inner = self.inner.write().unwrap();
        if inner.peers.remove(peer_id).is_some() {
            info!(peer_id = %peer_id, "WireGuard peer removed");
            Ok(())
        } else {
            Err(MeshError::PeerNotFound(peer_id.to_string()))
        }
    }

    /// Return the [`MeshNode`] identities of all currently registered peers.
    pub fn list_peers(&self) -> Vec<MeshNode> {
        self.inner
            .read()
            .unwrap()
            .peers
            .values()
            .map(|p| p.node.clone())
            .collect()
    }

    // ── Packet I/O ────────────────────────────────────────────────────────────

    /// Encapsulate a plaintext payload and send it to `peer_id` over WireGuard.
    ///
    /// Returns the number of bytes written to the wire.
    pub fn send_to_peer(&self, peer_id: &str, plaintext: &[u8]) -> Result<usize, MeshError> {
        let mut inner = self.inner.write().unwrap();
        let peer = inner
            .peers
            .get_mut(peer_id)
            .ok_or_else(|| MeshError::PeerNotFound(peer_id.to_string()))?;

        // WireGuard overhead is at most 148 bytes (header + auth tag).
        let mut buf = vec![0u8; plaintext.len() + 148];
        match peer.tunnel.encapsulate(plaintext, &mut buf) {
            TunnResult::WriteToNetwork(packet) => {
                let sent = self.socket.send_to(packet, peer.endpoint)?;
                debug!(peer_id = %peer_id, bytes = sent, "WG packet sent");
                Ok(sent)
            }
            TunnResult::Err(e) => Err(MeshError::TunnelError(format!("{e:?}"))),
            other => {
                warn!(peer_id = %peer_id, result = ?other, "Unexpected encapsulate result");
                Ok(0)
            }
        }
    }

    /// Non-blocking receive: read one UDP packet from the socket, find the
    /// matching peer tunnel, and decapsulate it.
    ///
    /// Returns `Ok(Some((peer_id, plaintext)))` when a data packet is ready,
    /// `Ok(None)` when no packet is available or only a handshake was processed.
    pub fn recv_packet(&self) -> Result<Option<(String, Vec<u8>)>, MeshError> {
        let mut wire_buf = vec![0u8; 65535];
        match self.socket.recv_from(&mut wire_buf) {
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
            Err(e) => return Err(MeshError::Io(e)),
            Ok((n, _src)) => wire_buf.truncate(n),
        }

        let mut inner = self.inner.write().unwrap();
        let mut plain_buf = vec![0u8; 65535];

        for (peer_id, peer) in inner.peers.iter_mut() {
            match peer.tunnel.decapsulate(None, &wire_buf, &mut plain_buf) {
                TunnResult::WriteToTunnelV4(payload, _) | TunnResult::WriteToTunnelV6(payload, _) => {
                    let result = payload.to_vec();
                    debug!(peer_id = %peer_id, bytes = result.len(), "WG packet decapsulated");
                    return Ok(Some((peer_id.clone(), result)));
                }
                TunnResult::WriteToNetwork(handshake_resp) => {
                    // Handshake response — send it back automatically.
                    let endpoint = peer.endpoint;
                    let _ = self.socket.send_to(handshake_resp, endpoint);
                    return Ok(None);
                }
                TunnResult::Err(e) => {
                    warn!(peer_id = %peer_id, err = ?e, "Decapsulate error");
                }
                _ => {}
            }
        }

        Ok(None)
    }
}
