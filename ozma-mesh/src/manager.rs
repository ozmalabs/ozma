//! [`MeshManager`] — top-level lifecycle manager for an ozma mesh node.
//!
//! Responsibilities:
//! - Generate and hold the local WireGuard keypair.
//! - Maintain the local [`MeshNode`] identity.
//! - Add / remove peers (creates / tears down [`WgTunnel`] instances).
//! - Drive tunnel timers.
//! - Advertise via mDNS and optionally browse for peers.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::sync::RwLock;
use tokio::time;
use tracing::{debug, info, warn};

use crate::error::MeshError;
use crate::keys::WgKeypair;
use crate::mdns::MdnsHandle;
use crate::node::MeshNode;
use crate::tunnel::WgTunnel;

/// Interval at which tunnel timers are ticked (keepalive / handshake).
const TICK_INTERVAL: Duration = Duration::from_millis(100);

/// Inner state, protected by an `RwLock` so the manager can be shared across
/// async tasks.
struct Inner {
    keypair: WgKeypair,
    local_node: MeshNode,
    peers: HashMap<String, WgTunnel>,   // node_id → tunnel
    socket: Arc<UdpSocket>,
    _mdns: Option<MdnsHandle>,
}

/// Top-level mesh manager for a single ozma node.
///
/// # Example
///
/// ```no_run
/// # use ozma_mesh::MeshManager;
/// # #[tokio::main] async fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let mgr = MeshManager::new("my-node", "10.200.1.1", 51820).await?;
/// println!("Public key: {}", mgr.local_node().await.wg_pubkey);
/// # Ok(()) }
/// ```
pub struct MeshManager {
    pub(crate) inner: Arc<RwLock<Inner>>,
}

impl MeshManager {
    /// Create a new [`MeshManager`], bind a UDP socket on `wg_port`, generate
    /// a WireGuard keypair, and start mDNS advertising.
    pub async fn new(
        node_id: impl Into<String>,
        mesh_ip: impl Into<String>,
        wg_port: u16,
    ) -> Result<Self, MeshError> {
        let node_id = node_id.into();
        let mesh_ip = mesh_ip.into();

        let keypair = WgKeypair::generate();
        let local_node = MeshNode::new(
            node_id.clone(),
            keypair.public.clone(),
            mesh_ip.clone(),
            wg_port,
        );

        let bind_addr = format!("0.0.0.0:{wg_port}");
        let socket = UdpSocket::bind(&bind_addr).await?;
        let socket = Arc::new(socket);

        info!(
            node_id = %node_id,
            mesh_ip = %mesh_ip,
            wg_port = wg_port,
            pubkey = %keypair.public,
            "MeshManager: node initialised",
        );

        // Advertise via mDNS (best-effort — log error but don't fail).
        let mdns = MdnsHandle::advertise(&local_node)
            .map_err(|e| warn!("mDNS advertise failed: {e}"))
            .ok();

        let inner = Inner {
            keypair,
            local_node,
            peers: HashMap::new(),
            socket,
            _mdns: mdns,
        };

        Ok(Self {
            inner: Arc::new(RwLock::new(inner)),
        })
    }

    /// Return a clone of the local node's identity.
    pub async fn local_node(&self) -> MeshNode {
        self.inner.read().await.local_node.clone()
    }

    /// Add a peer and establish a WireGuard tunnel to it.
    ///
    /// `peer_endpoint` is the real (underlay) UDP address of the peer's WG
    /// socket, e.g. `"127.0.0.1:51821"`.
    pub async fn add_peer(
        &self,
        peer: MeshNode,
        peer_endpoint: SocketAddr,
    ) -> Result<(), MeshError> {
        let mut inner = self.inner.write().await;

        if inner.peers.contains_key(&peer.id) {
            return Err(MeshError::DuplicatePeer(peer.id.clone()));
        }

        let tunnel = WgTunnel::new(
            &inner.keypair,
            peer.clone(),
            peer_endpoint,
            Arc::clone(&inner.socket),
        )?;

        info!(
            peer_id = %peer.id,
            peer_ip = %peer.mesh_ip,
            endpoint = %peer_endpoint,
            "MeshManager: peer added",
        );

        inner.peers.insert(peer.id.clone(), tunnel);
        Ok(())
    }

    /// Remove a peer and tear down its WireGuard tunnel.
    pub async fn remove_peer(&self, node_id: &str) -> Result<(), MeshError> {
        let mut inner = self.inner.write().await;
        if inner.peers.remove(node_id).is_some() {
            info!(peer_id = %node_id, "MeshManager: peer removed");
            Ok(())
        } else {
            Err(MeshError::PeerNotFound(node_id.to_owned()))
        }
    }

    /// Return the IDs of all currently connected peers.
    pub async fn peer_ids(&self) -> Vec<String> {
        self.inner.read().await.peers.keys().cloned().collect()
    }

    /// Send a plaintext IP packet to a specific peer (by node ID).
    pub async fn send_to_peer(
        &self,
        node_id: &str,
        plaintext: &[u8],
    ) -> Result<(), MeshError> {
        let mut inner = self.inner.write().await;
        let tunnel = inner
            .peers
            .get_mut(node_id)
            .ok_or_else(|| MeshError::PeerNotFound(node_id.to_owned()))?;
        tunnel.send_plaintext(plaintext).await
    }

    /// Feed a raw UDP packet (received on the WG socket) to the appropriate
    /// peer tunnel.  Returns the decapsulated plaintext if available.
    pub async fn receive_udp(
        &self,
        from: SocketAddr,
        packet: &[u8],
    ) -> Result<Option<Vec<u8>>, MeshError> {
        let mut inner = self.inner.write().await;
        // Find the peer whose endpoint matches `from`.
        for tunnel in inner.peers.values_mut() {
            if tunnel.peer.wg_port == from.port() {
                return tunnel.receive_packet(packet).await;
            }
        }
        debug!(from = %from, "MeshManager: received UDP from unknown peer, ignoring");
        Ok(None)
    }

    /// Tick all peer tunnels (keepalive / handshake timers).
    ///
    /// Call this periodically (every ~100 ms).  The [`MeshManager::run`]
    /// helper does this automatically.
    pub async fn tick_all(&self) -> Result<(), MeshError> {
        let mut inner = self.inner.write().await;
        for tunnel in inner.peers.values_mut() {
            if let Err(e) = tunnel.tick().await {
                warn!(peer = %tunnel.peer.id, error = %e, "tick error");
            }
        }
        Ok(())
    }

    /// Spawn a background task that ticks all tunnels every [`TICK_INTERVAL`]
    /// and reads incoming UDP packets, dispatching them to the correct tunnel.
    ///
    /// Returns a [`tokio::task::JoinHandle`] — drop or abort it to stop.
    pub fn run(&self) -> tokio::task::JoinHandle<()> {
        let mgr = Self {
            inner: Arc::clone(&self.inner),
        };

        tokio::spawn(async move {
            let mut ticker = time::interval(TICK_INTERVAL);
            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        if let Err(e) = mgr.tick_all().await {
                            warn!("tick_all error: {e}");
                        }
                    }
                    result = async {
                        let socket = {
                            let inner = mgr.inner.read().await;
                            Arc::clone(&inner.socket)
                        };
                        let mut buf = vec![0u8; 65535];
                        socket.recv_from(&mut buf).await.map(|(n, addr)| (buf[..n].to_vec(), addr))
                    } => {
                        match result {
                            Ok((packet, from)) => {
                                if let Err(e) = mgr.receive_udp(from, &packet).await {
                                    debug!("receive_udp error: {e}");
                                }
                            }
                            Err(e) => warn!("UDP recv error: {e}"),
                        }
                    }
                }
            }
        })
    }
}
