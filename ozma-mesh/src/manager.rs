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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use boringtun::noise::{Tunn, TunnResult};
use libmdns::{Responder, Service};
use serde::{Deserialize, Serialize};
use tokio::net::UdpSocket as TokioUdpSocket;
use tokio::sync::mpsc;
use tokio::sync::{Notify, RwLock};
use tracing::{debug, info, warn};
use x25519_dalek::{PublicKey as X25519PublicKey, StaticSecret};

use crate::error::{MeshError, Result};
use crate::node::{MeshNode, WgPrivateKey};

// ── Per-peer state ────────────────────────────────────────────────────────────

/// Interval between `Tunn::update_timers` calls. boringtun rounds its internal
/// timers to seconds, so one tick per second is enough to drive keepalives,
/// rekeys, and session expiry.
const DEFAULT_TIMER_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// A handshake older than WireGuard's `REJECT_AFTER_TIME` cannot be used.
/// boringtun clears the session itself, but the manager also refuses to report
/// `established` past this age so a stale snapshot can never look reachable.
const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(180);

/// Minimal boringtun surface used by [`MeshManager`]. Extracted so timer
/// driving and expiry can be tested with a fake/clocked tunnel.
pub(crate) trait PeerTunnel: Send + Sync {
    fn decapsulate<'a>(
        &mut self,
        src_addr: Option<IpAddr>,
        datagram: &[u8],
        dst: &'a mut [u8],
    ) -> TunnResult<'a>;

    fn encapsulate<'a>(&mut self, src: &[u8], dst: &'a mut [u8]) -> TunnResult<'a>;

    fn update_timers<'a>(&mut self, dst: &'a mut [u8]) -> TunnResult<'a>;

    fn format_handshake_initiation<'a>(
        &mut self,
        dst: &'a mut [u8],
        force_resend: bool,
    ) -> TunnResult<'a>;

    fn stats(&self) -> (Option<Duration>, usize, usize, f32, Option<u32>);
}

impl PeerTunnel for Tunn {
    fn decapsulate<'a>(
        &mut self,
        src_addr: Option<IpAddr>,
        datagram: &[u8],
        dst: &'a mut [u8],
    ) -> TunnResult<'a> {
        Tunn::decapsulate(self, src_addr, datagram, dst)
    }

    fn encapsulate<'a>(&mut self, src: &[u8], dst: &'a mut [u8]) -> TunnResult<'a> {
        Tunn::encapsulate(self, src, dst)
    }

    fn update_timers<'a>(&mut self, dst: &'a mut [u8]) -> TunnResult<'a> {
        Tunn::update_timers(self, dst)
    }

    fn format_handshake_initiation<'a>(
        &mut self,
        dst: &'a mut [u8],
        force_resend: bool,
    ) -> TunnResult<'a> {
        Tunn::format_handshake_initiation(self, dst, force_resend)
    }

    fn stats(&self) -> (Option<Duration>, usize, usize, f32, Option<u32>) {
        Tunn::stats(self)
    }
}

pub(crate) struct PeerState {
    node: MeshNode,
    /// boringtun userspace WireGuard tunnel for this peer.
    tunnel: Box<dyn PeerTunnel>,
}

/// Read-only snapshot of one peer's tunnel, returned by
/// [`MeshManager::peer_statuses`].
///
/// `established` is only true after boringtun reports a completed handshake;
/// merely adding a peer never claims an active tunnel.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PeerStatus {
    /// Peer identity as registered.
    pub node: MeshNode,
    /// Whether a WireGuard handshake has completed.
    pub established: bool,
    /// Seconds since the last handshake (`None` before the first one).
    pub last_handshake_age_secs: Option<u64>,
    /// Data bytes sent through the tunnel.
    pub tx_bytes: usize,
    /// Data bytes received through the tunnel.
    pub rx_bytes: usize,
    /// boringtun's estimated packet loss in `0.0..=1.0`.
    pub loss: f32,
    /// Last handshake RTT in milliseconds, when known.
    pub rtt_ms: Option<u32>,
}

/// Bound on decapsulated packets waiting for a packet pump.
pub const DEFAULT_PACKET_QUEUE: usize = 256;

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

    /// mDNS responder + service registration handles, kept alive for the
    /// lifetime of the manager.
    ///
    /// `libmdns::Responder` contains a `RefCell` and is therefore `Send` but
    /// not `Sync`. Wrapping the handles in a `Mutex` keeps `MeshManager`
    /// `Sync`, so node services can call it from `Send` async trait methods.
    _mdns: std::sync::Mutex<Option<(Responder, Service)>>,

    /// This node's private key (needed to create per-peer boringtun tunnels).
    private_key: WgPrivateKey,

    /// Whether the receive loop is currently running.
    started: Arc<AtomicBool>,

    /// Wakes the receive loop so [`MeshManager::stop`] can end it.
    stop_notify: Arc<Notify>,

    /// Bounded channel of decapsulated IPv4 packets for the packet pump.
    packet_tx: mpsc::Sender<Vec<u8>>,
    /// Taken once by [`MeshManager::take_packet_receiver`].
    packet_rx: StdMutex<Option<mpsc::Receiver<Vec<u8>>>>,
    /// Packets dropped because the packet queue was full or no pump subscribed.
    packet_drops: Arc<AtomicU64>,

    /// How often every peer tunnel's timers are driven.
    timer_poll_interval: Duration,

    /// Age after which a handshake is no longer reported as established.
    handshake_timeout: Duration,
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

        let (packet_tx, packet_rx) = mpsc::channel(DEFAULT_PACKET_QUEUE);

        Ok(Self {
            node,
            inner: Arc::new(RwLock::new(Inner {
                peers: HashMap::new(),
            })),
            socket: Arc::new(socket),
            _mdns: std::sync::Mutex::new(Some((responder, svc))),
            private_key,
            started: Arc::new(AtomicBool::new(false)),
            stop_notify: Arc::new(Notify::new()),
            packet_tx,
            packet_rx: StdMutex::new(Some(packet_rx)),
            packet_drops: Arc::new(AtomicU64::new(0)),
            timer_poll_interval: DEFAULT_TIMER_POLL_INTERVAL,
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
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
        // Starting twice must not spawn a second reader on the same socket.
        if self.started.swap(true, Ordering::SeqCst) {
            return Ok(());
        }

        let socket_clone = match self.socket.try_clone() {
            Ok(socket) => socket,
            Err(e) => {
                self.started.store(false, Ordering::SeqCst);
                return Err(MeshError::TunnelError(format!(
                    "socket clone for recv loop: {e}"
                )));
            }
        };
        // from_std requires the socket to already be non-blocking (set in new())
        let async_sock = match TokioUdpSocket::from_std(socket_clone) {
            Ok(socket) => socket,
            Err(e) => {
                self.started.store(false, Ordering::SeqCst);
                return Err(MeshError::TunnelError(format!(
                    "tokio UdpSocket::from_std: {e}"
                )));
            }
        };
        let inner = Arc::clone(&self.inner);
        let started = Arc::clone(&self.started);
        let stop_notify = Arc::clone(&self.stop_notify);
        let packet_tx = self.packet_tx.clone();
        let packet_drops = Arc::clone(&self.packet_drops);
        let timer_poll_interval = self.timer_poll_interval;

        tokio::spawn(async move {
            let mut buf = vec![0u8; 65535];
            let mut out = vec![0u8; 65535];
            let mut ticker = tokio::time::interval(timer_poll_interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

            loop {
                let received = tokio::select! {
                    _ = stop_notify.notified() => break,
                    _ = ticker.tick() => {
                        drive_timers(&inner, &async_sock, &mut out).await;
                        continue;
                    }
                    received = async_sock.recv_from(&mut buf) => received,
                };
                match received {
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
                                .map(|ip| SocketAddr::new(IpAddr::V4(ip), p.node.wg_port) == src)
                                .unwrap_or(false)
                        });

                        if let Some(peer) = peer {
                            match peer.tunnel.decapsulate(None, pkt, &mut out) {
                                TunnResult::WriteToTunnelV4(data, _) => {
                                    debug!(src = %src, bytes = data.len(),
                                           "WireGuard: decapsulated IPv4 packet");
                                    match packet_tx.try_send(data.to_vec()) {
                                        Ok(()) => {}
                                        Err(mpsc::error::TrySendError::Full(_)) => {
                                            packet_drops.fetch_add(1, Ordering::Relaxed);
                                        }
                                        Err(mpsc::error::TrySendError::Closed(_)) => {}
                                    }
                                }
                                TunnResult::WriteToTunnelV6(data, _) => {
                                    debug!(src = %src, bytes = data.len(),
                                           "WireGuard: decapsulated IPv6 packet (pump is IPv4-only)");
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
                            }
                        } else {
                            debug!(src = %src, "WireGuard: packet from unknown peer, ignoring");
                        }
                    }
                }
            }
            started.store(false, Ordering::SeqCst);
        });

        Ok(())
    }

    /// Stop the background receive loop.
    ///
    /// This does not remove peers or drop the mDNS registration; callers that
    /// want the manager fully gone should drop it. It exists so a node service
    /// can report an honest stopped state without abandoning a running task.
    pub fn stop(&self) {
        self.started.store(false, Ordering::SeqCst);
        self.stop_notify.notify_waiters();
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
            None,     // no preshared key
            Some(25), // persistent keepalive every 25 s
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
            PeerState {
                node: peer_node,
                tunnel: Box::new(tunnel),
            },
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

    /// Return a snapshot of each peer with boringtun's tunnel statistics.
    ///
    /// `established` reflects a completed WireGuard handshake only. A peer that
    /// was added but has not completed a handshake is reported as not
    /// established, never as a fabricated connected tunnel.
    pub async fn peer_statuses(&self) -> Vec<PeerStatus> {
        let guard = self.inner.read().await;
        let timeout = self.handshake_timeout;
        guard
            .peers
            .values()
            .map(|peer| {
                let (age, tx_bytes, rx_bytes, loss, rtt_ms) = peer.tunnel.stats();
                PeerStatus {
                    node: peer.node.clone(),
                    // A stale handshake beyond WireGuard's reject time must not
                    // be reported as established, even if boringtun has not been
                    // polled yet.
                    established: age.is_some_and(|age| age <= timeout),
                    last_handshake_age_secs: age.map(|duration| duration.as_secs()),
                    tx_bytes,
                    rx_bytes,
                    loss,
                    rtt_ms,
                }
            })
            .collect()
    }

    // ── Packet API (used by the optional TUN packet pump) ─────────────────────

    /// Take the bounded channel of decapsulated IPv4 packets.
    ///
    /// The channel is created with the manager and can be taken once, normally
    /// by [`crate::pump::ManagerPacketIo`]. Until it is taken, decapsulated
    /// packets are counted in [`MeshManager::dropped_packets`] instead of being
    /// silently reported as delivered.
    pub fn take_packet_receiver(&self) -> Option<mpsc::Receiver<Vec<u8>>> {
        self.packet_rx
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
    }

    /// Number of decapsulated packets dropped because no pump was subscribed or
    /// the bounded packet queue was full.
    pub fn dropped_packets(&self) -> u64 {
        self.packet_drops.load(Ordering::Relaxed)
    }

    /// Encapsulate an IPv4 packet and send it to the peer owning its destination
    /// address.
    ///
    /// This is the outbound half of the minimal packet API used by the TUN pump;
    /// it never invents a peer. A malformed packet or an unknown destination is
    /// a typed error.
    pub async fn send_ipv4(&self, packet: &[u8]) -> Result<usize> {
        let destination = ipv4_destination(packet).ok_or_else(|| {
            MeshError::InvalidIp("packet is not a well-formed IPv4 packet".to_owned())
        })?;
        let peer_id = {
            let guard = self.inner.read().await;
            guard
                .peers
                .iter()
                .find(|(_, peer)| {
                    peer.node
                        .mesh_ip
                        .parse::<Ipv4Addr>()
                        .map(|ip| ip == destination)
                        .unwrap_or(false)
                })
                .map(|(id, _)| id.clone())
        }
        .ok_or_else(|| {
            MeshError::NoRoute(format!("no mesh peer owns IPv4 destination {destination}"))
        })?;
        self.send_to_peer(&peer_id, packet).await
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
                    self.socket
                        .try_clone()
                        .map_err(|e| MeshError::TunnelError(format!("socket clone: {e}")))?,
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
                    self.socket
                        .try_clone()
                        .map_err(|e| MeshError::TunnelError(format!("socket clone: {e}")))?,
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

/// Drive boringtun timers for every peer and send any resulting handshake or
/// keepalive packet to that peer's UDP endpoint.
///
/// `update_timers` returning `ConnectionExpired` is expected when a peer is
/// gone: boringtun clears the session, so `stats()` stops reporting a handshake
/// and the peer is no longer `established`. It must not stop the receive loop.
async fn drive_timers(inner: &Arc<RwLock<Inner>>, socket: &TokioUdpSocket, out: &mut [u8]) {
    let mut pending = Vec::new();
    {
        let mut guard = inner.write().await;
        for peer in guard.peers.values_mut() {
            let endpoint = peer
                .node
                .mesh_ip
                .parse::<Ipv4Addr>()
                .map(|ip| SocketAddr::new(IpAddr::V4(ip), peer.node.wg_port));
            let Ok(endpoint) = endpoint else {
                continue;
            };
            match peer.tunnel.update_timers(out) {
                TunnResult::WriteToNetwork(packet) => {
                    pending.push((peer.node.id.clone(), endpoint, packet.to_vec()));
                }
                TunnResult::Err(error) => {
                    // Expiry is a normal peer-down transition, not a transport
                    // failure for the local manager.
                    debug!(
                        peer_id = %peer.node.id,
                        error = ?error,
                        "WireGuard: peer timers expired the session"
                    );
                }
                _ => {}
            }
        }
    }

    for (peer_id, endpoint, packet) in pending {
        if let Err(error) = socket.send_to(&packet, endpoint).await {
            warn!(
                peer_id = %peer_id,
                endpoint = %endpoint,
                %error,
                "WireGuard: failed to send timer-driven packet"
            );
        }
    }
}

/// Validate the IPv4 header enough to trust the destination address.
fn ipv4_destination(packet: &[u8]) -> Option<Ipv4Addr> {
    if packet.len() < 20 || packet[0] >> 4 != 4 {
        return None;
    }
    let header_len = usize::from(packet[0] & 0x0f) * 4;
    if header_len < 20 || packet.len() < header_len {
        return None;
    }
    let total_len = usize::from(u16::from_be_bytes([packet[2], packet[3]]));
    if total_len < header_len || total_len > packet.len() {
        return None;
    }
    Some(Ipv4Addr::new(
        packet[16], packet[17], packet[18], packet[19],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64};

    use boringtun::noise::errors::WireGuardError;

    struct FakeState {
        update_calls: AtomicU64,
        age_secs: StdMutex<Option<u64>>,
        expire_on_update: AtomicBool,
        emit_packet: AtomicBool,
    }

    struct FakeTunnel {
        state: Arc<FakeState>,
    }

    impl FakeTunnel {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                state: Arc::new(FakeState {
                    update_calls: AtomicU64::new(0),
                    age_secs: StdMutex::new(None),
                    expire_on_update: AtomicBool::new(false),
                    emit_packet: AtomicBool::new(false),
                }),
            })
        }

        fn set_age(&self, age: Option<u64>) {
            *self
                .state
                .age_secs
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = age;
        }

        fn expire_on_update(&self) {
            self.state.expire_on_update.store(true, Ordering::SeqCst);
        }

        fn emit_packet(&self) {
            self.state.emit_packet.store(true, Ordering::SeqCst);
        }

        fn update_calls(&self) -> u64 {
            self.state.update_calls.load(Ordering::SeqCst)
        }
    }

    impl PeerTunnel for FakeTunnel {
        fn decapsulate<'a>(
            &mut self,
            _src_addr: Option<IpAddr>,
            _datagram: &[u8],
            _dst: &'a mut [u8],
        ) -> TunnResult<'a> {
            TunnResult::Done
        }

        fn encapsulate<'a>(&mut self, _src: &[u8], _dst: &'a mut [u8]) -> TunnResult<'a> {
            TunnResult::Done
        }

        fn update_timers<'a>(&mut self, dst: &'a mut [u8]) -> TunnResult<'a> {
            self.state.update_calls.fetch_add(1, Ordering::SeqCst);
            if self.state.expire_on_update.load(Ordering::SeqCst) {
                self.set_age(None);
                return TunnResult::Err(WireGuardError::ConnectionExpired);
            }
            if self.state.emit_packet.load(Ordering::SeqCst) {
                let packet = b"timer";
                dst[..packet.len()].copy_from_slice(packet);
                return TunnResult::WriteToNetwork(&mut dst[..packet.len()]);
            }
            TunnResult::Done
        }

        fn format_handshake_initiation<'a>(
            &mut self,
            _dst: &'a mut [u8],
            _force_resend: bool,
        ) -> TunnResult<'a> {
            TunnResult::Done
        }

        fn stats(&self) -> (Option<Duration>, usize, usize, f32, Option<u32>) {
            let age = *self
                .state
                .age_secs
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            (age.map(Duration::from_secs), 0, 0, 0.0, None)
        }
    }

    fn test_manager() -> MeshManager {
        let (node, private_key) = MeshNode::generate("self", "10.200.9.1", 0);
        let socket = UdpSocket::bind("127.0.0.1:0").expect("bind test manager socket");
        socket
            .set_nonblocking(true)
            .expect("test manager socket nonblocking");
        let (packet_tx, packet_rx) = mpsc::channel(8);
        MeshManager {
            node,
            inner: Arc::new(RwLock::new(Inner {
                peers: HashMap::new(),
            })),
            socket: Arc::new(socket),
            _mdns: StdMutex::new(None),
            private_key,
            started: Arc::new(AtomicBool::new(false)),
            stop_notify: Arc::new(Notify::new()),
            packet_tx,
            packet_rx: StdMutex::new(Some(packet_rx)),
            packet_drops: Arc::new(AtomicU64::new(0)),
            timer_poll_interval: Duration::from_millis(10),
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
        }
    }

    async fn insert_fake_peer(manager: &MeshManager, peer: MeshNode, tunnel: &Arc<FakeTunnel>) {
        manager.inner.write().await.peers.insert(
            peer.id.clone(),
            PeerState {
                node: peer,
                tunnel: Box::new(FakeTunnel {
                    state: Arc::clone(&tunnel.state),
                }),
            },
        );
    }

    #[tokio::test]
    async fn timer_poll_drives_tunnels_and_sends_their_packets() {
        let manager = test_manager();
        let receiver = UdpSocket::bind("127.0.0.1:0").expect("timer receiver");
        receiver
            .set_nonblocking(true)
            .expect("timer receiver nonblocking");
        let peer_port = receiver.local_addr().unwrap().port();
        let peer = MeshNode {
            id: "peer".to_owned(),
            wg_pubkey: MeshNode::generate("other", "10.200.9.2", 0).0.wg_pubkey,
            mesh_ip: "127.0.0.1".to_owned(),
            wg_port: peer_port,
        };
        let fake = FakeTunnel::new();
        fake.emit_packet();
        insert_fake_peer(&manager, peer, &fake).await;

        let socket = UdpSocket::bind("127.0.0.1:0").expect("timer sender");
        socket
            .set_nonblocking(true)
            .expect("timer sender nonblocking");
        let async_socket = TokioUdpSocket::from_std(socket).expect("async timer sender");
        let mut out = vec![0u8; 2048];
        drive_timers(&manager.inner, &async_socket, &mut out).await;
        drive_timers(&manager.inner, &async_socket, &mut out).await;

        assert_eq!(fake.update_calls(), 2, "timers run on every poll");
        let mut received = [0u8; 32];
        let (length, _) = receiver.recv_from(&mut received).expect("timer packet");
        assert_eq!(&received[..length], b"timer");
        let (length, _) = receiver.recv_from(&mut received).expect("timer packet");
        assert_eq!(&received[..length], b"timer");
    }

    #[tokio::test]
    async fn handshake_age_comes_from_the_tunnel_and_expires_after_timeout() {
        let manager = test_manager();
        let (peer, _) = MeshNode::generate("peer", "10.200.9.2", 51820);
        let fake = FakeTunnel::new();
        insert_fake_peer(&manager, peer, &fake).await;

        fake.set_age(Some(7));
        let status = &manager.peer_statuses().await[0];
        assert!(status.established, "a fresh handshake is established");
        assert_eq!(
            status.last_handshake_age_secs,
            Some(7),
            "the age is the real tunnel handshake age, not time since construction"
        );

        fake.set_age(Some(DEFAULT_HANDSHAKE_TIMEOUT.as_secs() + 1));
        let status = &manager.peer_statuses().await[0];
        assert!(
            !status.established,
            "a handshake past REJECT_AFTER_TIME must not report established"
        );
        assert_eq!(
            status.last_handshake_age_secs,
            Some(DEFAULT_HANDSHAKE_TIMEOUT.as_secs() + 1),
            "status still reports the real (stale) age"
        );

        fake.set_age(None);
        let status = &manager.peer_statuses().await[0];
        assert!(!status.established);
        assert_eq!(status.last_handshake_age_secs, None);
    }

    #[tokio::test]
    async fn timer_expiry_clears_established_after_the_peer_stops() {
        let manager = test_manager();
        let (peer, _) = MeshNode::generate("peer", "10.200.9.2", 51820);
        let fake = FakeTunnel::new();
        fake.set_age(Some(5));
        fake.expire_on_update();
        insert_fake_peer(&manager, peer, &fake).await;

        assert!(manager.peer_statuses().await[0].established);
        let socket = UdpSocket::bind("127.0.0.1:0").expect("expiry sender");
        socket
            .set_nonblocking(true)
            .expect("expiry sender nonblocking");
        let async_socket = TokioUdpSocket::from_std(socket).expect("async expiry sender");
        let mut out = vec![0u8; 2048];
        drive_timers(&manager.inner, &async_socket, &mut out).await;

        let status = &manager.peer_statuses().await[0];
        assert!(!status.established, "timer expiry must clear established");
        assert_eq!(status.last_handshake_age_secs, None);
        assert_eq!(
            manager.list_peers().await.len(),
            1,
            "the configured peer remains available for a future handshake"
        );
    }

    #[tokio::test]
    async fn send_ipv4_without_a_peer_is_a_no_route_drop() {
        let manager = test_manager();
        let mut packet = vec![0u8; 28];
        packet[0] = 0x45;
        packet[2..4].copy_from_slice(&(28u16).to_be_bytes());
        packet[16..20].copy_from_slice(&[10, 200, 9, 99]);

        let error = manager.send_ipv4(&packet).await.unwrap_err();
        assert!(
            matches!(error, MeshError::NoRoute(_)),
            "no peer must be a routable drop, not a malformed-packet error: {error:?}"
        );
    }
}
