//! [`MeshManager`] — WireGuard tunnel management + mDNS peer discovery.
//!
//! # Design
//!
//! Each `MeshManager` instance represents one node in the ozma mesh.  It:
//!
//! 1. Owns a userspace WireGuard tunnel per peer (via `boringtun`).
//! 2. Advertises itself over mDNS (`_ozma-mesh._udp.local`) so other nodes on
//!    the same LAN segment discover each other without a central registry.
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
use std::time::{Duration, Instant, SystemTime};

use boringtun::noise::{Tunn, TunnResult};
use libmdns::{Responder, Service};
use serde::{Deserialize, Serialize};
use tokio::net::UdpSocket as TokioUdpSocket;
use tokio::sync::mpsc;
use tokio::sync::{Notify, RwLock};
use tracing::{debug, info, warn};
use x25519_dalek::{PublicKey as X25519PublicKey, StaticSecret};

use crate::discovery::{normalize_capabilities, validate_peer_node, MDNS_SERVICE_TYPE};
use crate::error::{MeshError, Result};
use crate::node::{MeshNode, WgPrivateKey, WgPublicKey};
use crate::rotation::{validate_grace, KeyRotation, RotationSchedule, RotationStatus};

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

/// Creates the userspace tunnel for one peer.  Injectable so rotation and
/// rollback can be tested without real key material or a network.
pub(crate) trait TunnelFactory: Send + Sync {
    fn create(
        &self,
        local_key: &WgPrivateKey,
        peer: &MeshNode,
        index: u32,
    ) -> Result<Box<dyn PeerTunnel>>;
}

/// Real boringtun factory used in production.
struct BoringtunTunnelFactory;

impl TunnelFactory for BoringtunTunnelFactory {
    fn create(
        &self,
        local_key: &WgPrivateKey,
        peer: &MeshNode,
        index: u32,
    ) -> Result<Box<dyn PeerTunnel>> {
        let local_sk = StaticSecret::from(local_key.to_bytes());
        let peer_pk_bytes = peer.wg_pubkey.to_bytes()?;
        let peer_pk = X25519PublicKey::from(peer_pk_bytes);
        // Tunn::new(static_private, peer_public, preshared_key, keepalive, index, rate_limiter)
        let tunnel = Tunn::new(
            local_sk,
            peer_pk,
            None,     // no preshared key
            Some(25), // persistent keepalive every 25 s
            index,
            None,
        )
        .map_err(|e| MeshError::TunnelError(format!("Tunn::new for {}: {e}", peer.id)))?;
        Ok(Box::new(tunnel))
    }
}

pub(crate) struct PeerState {
    node: MeshNode,
    /// WireGuard UDP endpoint actually used for this peer. Static peers use
    /// `mesh_ip:wg_port`; discovered peers use the observed underlay address
    /// when one was advertised, keeping transport separate from the overlay.
    endpoint: SocketAddr,
    /// boringtun userspace WireGuard tunnel for this peer, using the current
    /// local private key.
    tunnel: Box<dyn PeerTunnel>,
    /// Old-key tunnel kept only for the bounded rotation grace window so
    /// in-flight handshakes can complete.
    grace_tunnel: Option<Box<dyn PeerTunnel>>,
    /// Monotonic deadline for `grace_tunnel`.
    grace_expires: Option<Instant>,
    /// Whether this peer came from static configuration or discovery.
    origin: PeerOrigin,
    /// Capabilities advertised by a discovered peer.
    capabilities: Vec<String>,
}

/// Where a peer entry came from.
///
/// Static peers (`--mesh-peer`) are authoritative. Discovered peers are
/// additive and can only be added/updated/removed by the discovery reconciler.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PeerOrigin {
    #[default]
    Static,
    Discovered,
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
    /// Whether the peer was statically configured or discovered.
    #[serde(default)]
    pub origin: PeerOrigin,
    /// Capabilities advertised by a discovered peer.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// UDP endpoint used for this peer's WireGuard transport.
    #[serde(default)]
    pub endpoint: String,
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

/// Result of adding a discovered peer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PeerChange {
    /// A new discovered peer and tunnel were created.
    Added,
    /// The advertisement was identical; nothing changed.
    Unchanged,
    /// The same authorized peer changed endpoint; its tunnel was replaced.
    Updated { previous: MeshNode },
}

// ── Lock-protected inner state ────────────────────────────────────────────────

pub(crate) struct Inner {
    pub(crate) peers: HashMap<String, PeerState>,
    pub(crate) next_peer_index: u32,
}

// ── mDNS advertising ─────────────────────────────────────────────────────────

/// A registered `_ozma-mesh._udp.local` advertisement for one node.
///
/// Keeping the responder and service handles together lets both the userspace
/// manager and the kernel backend advertise the same identity without owning a
/// userspace WireGuard socket.
pub struct MeshAdvertiser {
    _responder: Responder,
    _service: Service,
    port: u16,
}

impl MeshAdvertiser {
    /// Register this node's mesh identity on the local network.
    pub fn new(node: &MeshNode, port: u16, capabilities: &[String]) -> Result<Self> {
        Self::new_with_previous(node, &[], port, capabilities)
    }

    /// Register this node's mesh identity, optionally publishing the previous
    /// public key while a rotation grace window is open.
    ///
    /// Only public keys are advertised; the private key never leaves the
    /// backend.
    pub fn new_with_previous(
        node: &MeshNode,
        previous_public_keys: &[String],
        port: u16,
        capabilities: &[String],
    ) -> Result<Self> {
        let responder = Responder::new().map_err(|e| MeshError::Mdns(e.to_string()))?;
        let instance = mdns_instance_name(&node.id);
        let capability_text = mdns_capability_text(capabilities);
        let mut properties = vec![
            format!("id={}", node.id),
            format!("mesh_ip={}", node.mesh_ip),
            format!("pubkey={}", node.wg_pubkey),
            format!("wg_port={port}"),
            "proto=1".to_owned(),
        ];
        let previous: Vec<&str> = previous_public_keys
            .iter()
            .map(String::as_str)
            .filter(|key| !key.is_empty() && !key.chars().any(char::is_control))
            .take(2)
            .collect();
        if !previous.is_empty() {
            // Bounded: the whole TXT record must stay well below one DNS
            // character-string limit for practical responders.
            properties.push(format!("prev_pubkey={}", previous.join(",")));
        }
        if !capability_text.is_empty() {
            properties.push(format!("cap={capability_text}"));
        }
        let property_refs: Vec<&str> = properties.iter().map(String::as_str).collect();
        let service =
            responder.register(MDNS_SERVICE_TYPE.to_owned(), instance, port, &property_refs);
        Ok(Self {
            _responder: responder,
            _service: service,
            port,
        })
    }

    /// UDP port advertised for this node.
    pub fn port(&self) -> u16 {
        self.port
    }
}

// ── MeshManager ───────────────────────────────────────────────────────────────

/// Manages WireGuard peers and mDNS advertising for one ozma mesh node.
pub struct MeshManager {
    /// Current node identity. The public key changes on rotation.
    identity: Arc<StdMutex<MeshNode>>,

    pub(crate) inner: Arc<RwLock<Inner>>,

    /// UDP socket used to send/receive WireGuard-encapsulated packets.
    pub socket: Arc<UdpSocket>,

    /// mDNS responder + service registration handles, kept alive for the
    /// lifetime of the manager.
    ///
    /// `libmdns::Responder` contains a `RefCell` and is therefore `Send` but
    /// not `Sync`. Wrapping the handles in a `Mutex` keeps `MeshManager`
    /// `Sync`, so node services can call it from `Send` async trait methods.
    _mdns: Arc<StdMutex<Option<MeshAdvertiser>>>,

    /// Capabilities included in every mDNS (re-)registration.
    capabilities: Arc<Vec<String>>,

    /// Current and grace-window key lineage.
    rotation: Arc<StdMutex<KeyRotation>>,

    /// Serialises rotations (the std key mutex cannot be held across awaits).
    rotation_op: Arc<tokio::sync::Mutex<()>>,

    /// Tunnel constructor seam (real boringtun in production).
    factory: Arc<dyn TunnelFactory>,

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
    /// `_ozma-mesh._udp.local` advertising the node's id, mesh IP, public key,
    /// and WireGuard endpoint.
    pub async fn new(node: MeshNode, private_key: WgPrivateKey) -> Result<Self> {
        Self::new_with_capabilities(node, private_key, Vec::new()).await
    }

    /// Like [`MeshManager::new`], with node capabilities included in the mDNS
    /// TXT record.
    pub async fn new_with_capabilities(
        node: MeshNode,
        private_key: WgPrivateKey,
        capabilities: Vec<String>,
    ) -> Result<Self> {
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

        // Advertise over a dedicated mesh service type so the node's
        // controller-facing `_ozma._udp` record (HTTP port/TXT) is untouched.
        let advertiser = MeshAdvertiser::new(&node, actual_port, &capabilities)?;

        info!(
            node_id = %node.id,
            "MeshManager: mDNS service registered (_ozma-mesh._udp.local)"
        );

        let (packet_tx, packet_rx) = mpsc::channel(DEFAULT_PACKET_QUEUE);
        let rotation = Arc::new(StdMutex::new(KeyRotation::new(private_key)));

        Ok(Self {
            identity: Arc::new(StdMutex::new(node)),
            inner: Arc::new(RwLock::new(Inner {
                peers: HashMap::new(),
                next_peer_index: 0,
            })),
            socket: Arc::new(socket),
            _mdns: Arc::new(StdMutex::new(Some(advertiser))),
            capabilities: Arc::new(capabilities),
            rotation,
            rotation_op: Arc::new(tokio::sync::Mutex::new(())),
            factory: Arc::new(BoringtunTunnelFactory),
            started: Arc::new(AtomicBool::new(false)),
            stop_notify: Arc::new(Notify::new()),
            packet_tx,
            packet_rx: StdMutex::new(Some(packet_rx)),
            packet_drops: Arc::new(AtomicU64::new(0)),
            timer_poll_interval: DEFAULT_TIMER_POLL_INTERVAL,
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
        })
    }

    /// Current node identity (public key changes after rotation).
    pub fn node(&self) -> MeshNode {
        self.identity
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// Current local public key.
    pub fn public_key(&self) -> WgPublicKey {
        self.node().wg_pubkey
    }

    /// Current local private key.
    ///
    /// Used by the node backend to recreate a stopped manager without losing a
    /// rotated identity.  The value is never serialised or logged.
    #[doc(hidden)]
    pub fn private_key_for_restart(&self) -> WgPrivateKey {
        self.rotation
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .current_private()
            .clone()
    }

    /// Secret-free key rotation status.
    pub fn rotation_status(&self) -> RotationStatus {
        let rotation = self
            .rotation
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        rotation.status(Instant::now())
    }

    /// Configure the reported scheduled-rotation policy (the node owns the
    /// timer; the manager only reports it).
    pub fn set_rotation_schedule(&self, schedule: Option<RotationSchedule>) {
        self.rotation
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .set_schedule(schedule);
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
        let rotation = Arc::clone(&self.rotation);
        let identity = Arc::clone(&self.identity);
        let mdns = Arc::clone(&self._mdns);
        let capabilities = Arc::clone(&self.capabilities);
        let port = self.wg_port();

        tokio::spawn(async move {
            let mut buf = vec![0u8; 65535];
            let mut out = vec![0u8; 65535];
            let mut ticker = tokio::time::interval(timer_poll_interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

            loop {
                let received = tokio::select! {
                    _ = stop_notify.notified() => break,
                    _ = ticker.tick() => {
                        expire_grace(
                            &inner,
                            &rotation,
                            &identity,
                            &mdns,
                            &capabilities,
                            port,
                        )
                        .await;
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

                        // Find the peer whose configured UDP endpoint matches
                        // `src`. Discovered peers may use an underlay address
                        // distinct from their overlay mesh IP.
                        let peer = guard.peers.values_mut().find(|p| p.endpoint == src);

                        if let Some(peer) = peer {
                            let mut result = peer.tunnel.decapsulate(None, pkt, &mut out);
                            // During rotation grace, a handshake from a peer
                            // that still knows the old public key is accepted by
                            // the old tunnel. Only consult it after the current
                            // tunnel rejects the datagram.
                            if matches!(result, TunnResult::Err(_)) {
                                let grace_live = peer
                                    .grace_expires
                                    .is_some_and(|deadline| Instant::now() < deadline);
                                if grace_live {
                                    if let Some(grace) = peer.grace_tunnel.as_mut() {
                                        result = grace.decapsulate(None, pkt, &mut out);
                                    }
                                }
                            }
                            match result {
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

    // ── Key rotation ─────────────────────────────────────────────────────────

    /// Rotate the local WireGuard keypair and rebuild every peer tunnel.
    ///
    /// The new public key is advertised over mDNS immediately, with the old
    /// public key carried as `prev_pubkey` for the grace window.  Every static
    /// and discovered peer is updated atomically: if a tunnel cannot be
    /// created, or the new mDNS registration fails, no key, tunnel, or
    /// advertisement changes and a typed [`MeshError::RotationFailed`] is
    /// returned.
    pub async fn rotate_keys(&self, grace: Duration) -> Result<RotationStatus> {
        let grace = validate_grace(grace).map_err(MeshError::RotationInvalid)?;
        let new_key = WgPrivateKey::generate();
        let now = Instant::now();
        let wall = SystemTime::now();

        // Serialise rotations so two calls cannot interleave tunnel swaps.
        let _operation = self.rotation_op.lock().await;

        // Drop an expired previous key first so its tunnels cannot linger.
        self.sweep_grace_at(now).await;

        let mut guard = self.inner.write().await;
        // Build all replacements before touching live state. This is the
        // rollback boundary: an error here leaves the old key fully in use.
        let peers: Vec<(String, MeshNode)> = guard
            .peers
            .iter()
            .map(|(id, peer)| (id.clone(), peer.node.clone()))
            .collect();
        let mut created: Vec<(String, Box<dyn PeerTunnel>)> = Vec::with_capacity(peers.len());
        for (id, node) in peers {
            let index = guard.next_peer_index;
            guard.next_peer_index = guard.next_peer_index.wrapping_add(1);
            match self.factory.create(&new_key, &node, index) {
                Ok(tunnel) => created.push((id, tunnel)),
                Err(error) => {
                    return Err(MeshError::RotationFailed(format!(
                        "peer {id} could not be updated; rotation rolled back: {error}"
                    )));
                }
            }
        }

        let old_public = self
            .identity
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .wg_pubkey
            .clone();
        let mut candidate = self.node();
        candidate.wg_pubkey = new_key.public_key();

        // Re-register the advertisement with the new public key before the
        // in-memory switch. If this fails the old key and tunnels are intact.
        let advertise = self
            ._mdns
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_some();
        if advertise {
            // Drop the old responder before binding a replacement on port 5353.
            self._mdns
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take();
        }
        let previous = vec![old_public.0.clone()];
        let advertiser = if advertise {
            match MeshAdvertiser::new_with_previous(
                &candidate,
                &previous,
                self.wg_port(),
                &self.capabilities,
            ) {
                Ok(advertiser) => Some(advertiser),
                Err(error) => {
                    if let Ok(restored) =
                        MeshAdvertiser::new(&self.node(), self.wg_port(), &self.capabilities)
                    {
                        *self._mdns.lock().unwrap_or_else(|error| error.into_inner()) =
                            Some(restored);
                    }
                    return Err(MeshError::RotationFailed(format!(
                        "mDNS advertisement for the new key failed; rotation rolled back: {error}"
                    )));
                }
            }
        } else {
            None
        };

        // Commit: every old tunnel becomes the bounded grace tunnel.
        let grace_expires = now + grace;
        for (id, tunnel) in created {
            if let Some(peer) = guard.peers.get_mut(&id) {
                let old_tunnel = std::mem::replace(&mut peer.tunnel, tunnel);
                peer.grace_tunnel = Some(old_tunnel);
                peer.grace_expires = Some(grace_expires);
            }
        }
        drop(guard);
        if let Some(advertiser) = advertiser {
            *self._mdns.lock().unwrap_or_else(|error| error.into_inner()) = Some(advertiser);
        }
        *self
            .identity
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = candidate;

        let status = self
            .rotation
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .begin(new_key, grace, now, wall);
        info!(
            key_id = status.key_id,
            fingerprint = %status.fingerprint,
            grace_secs = status.grace_secs,
            "MeshManager: rotated WireGuard key"
        );
        Ok(status)
    }

    /// Remove grace tunnels whose window elapsed. Returns true when state
    /// changed (tests/diagnostics).
    pub async fn expire_grace(&self) -> bool {
        self.sweep_grace_at(Instant::now()).await
    }

    async fn sweep_grace_at(&self, now: Instant) -> bool {
        let expired = self
            .rotation
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .expire(now);
        if !expired {
            return false;
        }
        {
            let mut guard = self.inner.write().await;
            for peer in guard.peers.values_mut() {
                peer.grace_tunnel = None;
                peer.grace_expires = None;
            }
        }
        // Stop advertising the old key now that it can no longer be used.
        let node = self.node();
        let advertised = self
            ._mdns
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_some();
        if !advertised {
            return true;
        }
        // Drop the old responder before binding a replacement.
        self._mdns
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        match MeshAdvertiser::new(&node, self.wg_port(), &self.capabilities) {
            Ok(advertiser) => {
                *self._mdns.lock().unwrap_or_else(|error| error.into_inner()) = Some(advertiser);
            }
            Err(error) => {
                warn!(%error, "MeshManager: could not re-advertise after grace expiry");
                if let Ok(restored) = MeshAdvertiser::new(&node, self.wg_port(), &self.capabilities)
                {
                    *self._mdns.lock().unwrap_or_else(|error| error.into_inner()) = Some(restored);
                }
            }
        }
        info!("MeshManager: rotation grace window elapsed; previous key removed");
        true
    }

    // ── Peer management ───────────────────────────────────────────────────────

    /// Add a remote peer and create a WireGuard tunnel to it.
    ///
    /// `peer_node.mesh_ip` and `peer_node.wg_port` are used as the UDP
    /// endpoint for outgoing packets. The peer is tagged [`PeerOrigin::Static`]
    /// and cannot be replaced by discovery.
    ///
    /// Returns [`MeshError::PeerAlreadyExists`] if a peer with the same id is
    /// already registered.
    pub async fn add_peer(&self, peer_node: MeshNode) -> Result<()> {
        let mut guard = self.inner.write().await;

        if guard.peers.contains_key(&peer_node.id) {
            return Err(MeshError::PeerAlreadyExists(peer_node.id.clone()));
        }

        let endpoint = peer_endpoint(&peer_node, None)?;
        let tunnel = self.create_tunnel(&peer_node, &mut guard)?;

        info!(
            peer_id    = %peer_node.id,
            peer_ip    = %peer_node.mesh_ip,
            peer_port  = peer_node.wg_port,
            peer_pubkey = %peer_node.wg_pubkey,
            "MeshManager: static peer added"
        );

        guard.peers.insert(
            peer_node.id.clone(),
            PeerState {
                node: peer_node,
                endpoint,
                tunnel,
                grace_tunnel: None,
                grace_expires: None,
                origin: PeerOrigin::Static,
                capabilities: Vec::new(),
            },
        );

        Ok(())
    }

    /// Add or update a peer learned through authorized mDNS discovery.
    ///
    /// The node fields are validated again here, so a caller cannot bypass
    /// discovery validation and install an unauthenticated key/IP. Static peers
    /// are authoritative: a discovered id that collides with one is a typed
    /// [`MeshError::DiscoveryConflict`] and the existing tunnel is untouched.
    /// Re-advertising an unchanged peer is a no-op; changing the mesh IP or
    /// WireGuard endpoint replaces the single tunnel for that id (never
    /// creates a duplicate).
    pub async fn add_discovered_peer(
        &self,
        peer_node: MeshNode,
        capabilities: Vec<String>,
    ) -> Result<PeerChange> {
        self.add_discovered_peer_with_endpoint(peer_node, capabilities, None)
            .await
    }

    /// Like [`MeshManager::add_discovered_peer`], with the observed underlay
    /// address used as the WireGuard UDP endpoint.
    pub async fn add_discovered_peer_with_endpoint(
        &self,
        peer_node: MeshNode,
        capabilities: Vec<String>,
        underlay_ip: Option<Ipv4Addr>,
    ) -> Result<PeerChange> {
        self.add_discovered_peer_inner(peer_node, capabilities, underlay_ip, &[])
            .await
    }

    /// Update a discovered peer whose WireGuard public key rotated.
    ///
    /// `previous_public_keys` comes from the peer's mDNS `prev_pubkey` grace
    /// field. The key change is accepted only when the currently registered key
    /// is one of those previous keys; otherwise it remains a typed conflict.
    pub async fn add_discovered_peer_rotated(
        &self,
        peer_node: MeshNode,
        capabilities: Vec<String>,
        underlay_ip: Option<Ipv4Addr>,
        previous_public_keys: Vec<String>,
    ) -> Result<PeerChange> {
        self.add_discovered_peer_inner(peer_node, capabilities, underlay_ip, &previous_public_keys)
            .await
    }

    async fn add_discovered_peer_inner(
        &self,
        peer_node: MeshNode,
        capabilities: Vec<String>,
        underlay_ip: Option<Ipv4Addr>,
        allowed_previous_keys: &[String],
    ) -> Result<PeerChange> {
        validate_peer_node(&peer_node)?;
        let capabilities = normalize_capabilities(&capabilities)?;
        let endpoint = peer_endpoint(&peer_node, underlay_ip)?;
        let mut guard = self.inner.write().await;

        if let Some(existing) = guard.peers.get(&peer_node.id) {
            if existing.origin == PeerOrigin::Static {
                return Err(MeshError::DiscoveryConflict(format!(
                    "discovered peer {} conflicts with a static --mesh-peer entry; \
                     static configuration is authoritative",
                    peer_node.id
                )));
            }
            if existing.node.wg_pubkey != peer_node.wg_pubkey {
                let authorized_previous = allowed_previous_keys
                    .iter()
                    .any(|key| key == &existing.node.wg_pubkey.0);
                if !authorized_previous {
                    return Err(MeshError::DiscoveryConflict(format!(
                        "discovered peer {} changed WireGuard public key while registered",
                        peer_node.id
                    )));
                }
            }
            if existing.node == peer_node
                && existing.capabilities == capabilities
                && existing.endpoint == endpoint
            {
                return Ok(PeerChange::Unchanged);
            }
        }
        if guard.peers.contains_key(&peer_node.id) {
            self.reject_other_owner(&guard, &peer_node)?;
            let previous = guard
                .peers
                .get(&peer_node.id)
                .expect("peer was present")
                .node
                .clone();
            let tunnel = self.create_tunnel(&peer_node, &mut guard)?;
            if previous.wg_pubkey == peer_node.wg_pubkey {
                info!(
                    peer_id = %peer_node.id,
                    old_ip = %previous.mesh_ip,
                    new_ip = %peer_node.mesh_ip,
                    %endpoint,
                    "MeshManager: discovered peer endpoint updated"
                );
            } else {
                info!(
                    peer_id = %peer_node.id,
                    old_fingerprint = %crate::rotation::fingerprint(&previous.wg_pubkey),
                    new_fingerprint = %crate::rotation::fingerprint(&peer_node.wg_pubkey),
                    "MeshManager: discovered peer public key rotated"
                );
            }
            guard.peers.insert(
                peer_node.id.clone(),
                PeerState {
                    node: peer_node,
                    endpoint,
                    tunnel,
                    grace_tunnel: None,
                    grace_expires: None,
                    origin: PeerOrigin::Discovered,
                    capabilities,
                },
            );
            return Ok(PeerChange::Updated { previous });
        }

        self.reject_other_owner(&guard, &peer_node)?;
        let tunnel = self.create_tunnel(&peer_node, &mut guard)?;
        info!(
            peer_id = %peer_node.id,
            peer_ip = %peer_node.mesh_ip,
            peer_port = peer_node.wg_port,
            %endpoint,
            "MeshManager: discovered peer added"
        );
        guard.peers.insert(
            peer_node.id.clone(),
            PeerState {
                node: peer_node,
                endpoint,
                tunnel,
                grace_tunnel: None,
                grace_expires: None,
                origin: PeerOrigin::Discovered,
                capabilities,
            },
        );
        Ok(PeerChange::Added)
    }

    /// Remove a discovered peer and its tunnel.
    ///
    /// `Ok(None)` means the peer was already absent (idempotent expiry).
    /// Static peers are never removed by discovery and return a typed conflict.
    pub async fn remove_discovered_peer(&self, peer_id: &str) -> Result<Option<MeshNode>> {
        let mut guard = self.inner.write().await;
        let Some(peer) = guard.peers.get(peer_id) else {
            return Ok(None);
        };
        if peer.origin == PeerOrigin::Static {
            return Err(MeshError::DiscoveryConflict(format!(
                "static peer {peer_id} cannot be removed by discovery"
            )));
        }
        let removed = guard.peers.remove(peer_id).expect("peer was present").node;
        info!(peer_id = %peer_id, "MeshManager: discovered peer removed");
        Ok(Some(removed))
    }

    /// Create a boringtun tunnel for `peer_node` with a fresh local index.
    fn create_tunnel(
        &self,
        peer_node: &MeshNode,
        guard: &mut Inner,
    ) -> Result<Box<dyn PeerTunnel>> {
        let index = guard.next_peer_index;
        guard.next_peer_index = guard.next_peer_index.wrapping_add(1);
        let local_key = self
            .rotation
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .current_private()
            .clone();
        self.factory.create(&local_key, peer_node, index)
    }

    /// Reject a discovered peer reusing another peer's mesh IP or public key.
    fn reject_other_owner(&self, guard: &Inner, candidate: &MeshNode) -> Result<()> {
        let identity = self.node();
        if candidate.mesh_ip == identity.mesh_ip {
            return Err(MeshError::DiscoveryConflict(format!(
                "advertised mesh IP {} is this node's own address",
                candidate.mesh_ip
            )));
        }
        if candidate.wg_pubkey == identity.wg_pubkey {
            return Err(MeshError::DiscoveryConflict(
                "advertised public key is this node's own key".to_owned(),
            ));
        }
        for (id, peer) in &guard.peers {
            if id == &candidate.id {
                continue;
            }
            if peer.node.mesh_ip == candidate.mesh_ip {
                return Err(MeshError::DiscoveryConflict(format!(
                    "advertised mesh IP {} is already owned by peer {id}",
                    candidate.mesh_ip
                )));
            }
            if peer.node.wg_pubkey == candidate.wg_pubkey {
                return Err(MeshError::DiscoveryConflict(format!(
                    "advertised public key is already owned by peer {id}"
                )));
            }
        }
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
                    origin: peer.origin,
                    capabilities: peer.capabilities.clone(),
                    endpoint: peer.endpoint.to_string(),
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
            peer.endpoint
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
            peer.endpoint
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

/// Drop grace tunnels and stop advertising the old public key once the window
/// elapsed. Safe to call on every timer tick.
async fn expire_grace(
    inner: &Arc<RwLock<Inner>>,
    rotation: &Arc<StdMutex<KeyRotation>>,
    identity: &Arc<StdMutex<MeshNode>>,
    mdns: &Arc<StdMutex<Option<MeshAdvertiser>>>,
    capabilities: &Arc<Vec<String>>,
    port: u16,
) {
    let expired = rotation
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .expire(Instant::now());
    if !expired {
        return;
    }
    {
        let mut guard = inner.write().await;
        for peer in guard.peers.values_mut() {
            peer.grace_tunnel = None;
            peer.grace_expires = None;
        }
    }
    let node = identity
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    let advertised = mdns
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .is_some();
    if !advertised {
        return;
    }
    // Drop the old responder before binding a replacement.
    mdns.lock()
        .unwrap_or_else(|error| error.into_inner())
        .take();
    match MeshAdvertiser::new(&node, port, capabilities) {
        Ok(advertiser) => {
            *mdns.lock().unwrap_or_else(|error| error.into_inner()) = Some(advertiser);
        }
        Err(error) => {
            warn!(%error, "MeshManager: could not re-advertise after grace expiry");
            if let Ok(restored) = MeshAdvertiser::new(&node, port, capabilities) {
                *mdns.lock().unwrap_or_else(|error| error.into_inner()) = Some(restored);
            }
        }
    }
    info!("MeshManager: rotation grace window elapsed; previous key removed");
}

/// Drive boringtun timers for every peer and send any resulting handshake or
/// keepalive packet to that peer's UDP endpoint.
/// `update_timers` returning `ConnectionExpired` is expected when a peer is
/// gone: boringtun clears the session, so `stats()` stops reporting a handshake
/// and the peer is no longer `established`. It must not stop the receive loop.
async fn drive_timers(inner: &Arc<RwLock<Inner>>, socket: &TokioUdpSocket, out: &mut [u8]) {
    let mut pending = Vec::new();
    {
        let mut guard = inner.write().await;
        for peer in guard.peers.values_mut() {
            let endpoint = peer.endpoint;
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

/// Resolve the WireGuard UDP endpoint for a peer.
///
/// Static peers use their mesh IP (existing behavior). Discovered peers use
/// the observed underlay address when the mDNS advertisement provided one,
/// keeping encrypted UDP off the overlay TUN.
fn peer_endpoint(node: &MeshNode, underlay_ip: Option<Ipv4Addr>) -> Result<SocketAddr> {
    let address = match underlay_ip {
        Some(address) => address,
        None => node
            .mesh_ip
            .parse::<Ipv4Addr>()
            .map_err(|_| MeshError::InvalidIp(node.mesh_ip.clone()))?,
    };
    Ok(SocketAddr::new(IpAddr::V4(address), node.wg_port))
}

/// Sanitize a node id into a DNS-SD instance label.
///
/// The TXT `id` field remains the authoritative identity; this only keeps the
/// SRV/PTR instance name a valid single DNS label.
fn mdns_instance_name(id: &str) -> String {
    let mut name: String = id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                character
            } else {
                '-'
            }
        })
        .take(63)
        .collect();
    if name.is_empty() {
        name.push_str("mesh-node");
    }
    name
}

/// Join capabilities into one bounded TXT value.
fn mdns_capability_text(capabilities: &[String]) -> String {
    let mut seen = std::collections::BTreeSet::new();
    for capability in capabilities {
        for entry in capability.split(',') {
            let entry = entry.trim();
            if !entry.is_empty() && !entry.chars().any(char::is_control) {
                seen.insert(entry.to_owned());
            }
        }
    }
    seen.into_iter()
        .collect::<Vec<_>>()
        .join(",")
        .chars()
        .take(240)
        .collect()
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

    use crate::node::WgPublicKey;

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
        test_manager_with_factory(Arc::new(BoringtunTunnelFactory))
    }

    fn test_manager_with_factory(factory: Arc<dyn TunnelFactory>) -> MeshManager {
        let (node, private_key) = MeshNode::generate("self", "10.200.9.1", 0);
        let socket = UdpSocket::bind("127.0.0.1:0").expect("bind test manager socket");
        socket
            .set_nonblocking(true)
            .expect("test manager socket nonblocking");
        let (packet_tx, packet_rx) = mpsc::channel(8);
        MeshManager {
            identity: Arc::new(StdMutex::new(node)),
            inner: Arc::new(RwLock::new(Inner {
                peers: HashMap::new(),
                next_peer_index: 0,
            })),
            socket: Arc::new(socket),
            _mdns: Arc::new(StdMutex::new(None)),
            capabilities: Arc::new(Vec::new()),
            rotation: Arc::new(StdMutex::new(KeyRotation::new(private_key))),
            rotation_op: Arc::new(tokio::sync::Mutex::new(())),
            factory,
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
        let endpoint = peer_endpoint(&peer, None).expect("test peer endpoint");
        manager.inner.write().await.peers.insert(
            peer.id.clone(),
            PeerState {
                node: peer,
                endpoint,
                tunnel: Box::new(FakeTunnel {
                    state: Arc::clone(&tunnel.state),
                }),
                grace_tunnel: None,
                grace_expires: None,
                origin: PeerOrigin::Static,
                capabilities: Vec::new(),
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

    #[tokio::test]
    async fn discovered_peer_requires_a_valid_wireguard_key() {
        let manager = test_manager();
        let mut peer = MeshNode::generate("discovered", "10.200.9.2", 51_820).0;
        peer.wg_pubkey = WgPublicKey("not-base64".to_owned());

        let error = manager
            .add_discovered_peer(peer, vec!["video".to_owned()])
            .await
            .unwrap_err();
        assert!(
            matches!(error, MeshError::KeyError(_) | MeshError::Base64(_)),
            "an invalid advertised key must be a typed validation error: {error:?}"
        );
        assert!(
            manager.list_peers().await.is_empty(),
            "no peer/tunnel may be installed before key validation"
        );
    }

    #[tokio::test]
    async fn discovered_peer_updates_endpoint_without_a_duplicate_tunnel() {
        let manager = test_manager();
        let peer = MeshNode::generate("peer", "10.200.9.2", 51_820).0;
        assert_eq!(
            manager
                .add_discovered_peer(peer.clone(), vec!["video".to_owned()])
                .await
                .unwrap(),
            PeerChange::Added
        );
        assert_eq!(
            manager
                .add_discovered_peer(peer.clone(), vec!["video".to_owned()])
                .await
                .unwrap(),
            PeerChange::Unchanged
        );

        let mut moved = peer.clone();
        moved.mesh_ip = "10.200.9.3".to_owned();
        let change = manager
            .add_discovered_peer(moved.clone(), vec!["video".to_owned()])
            .await
            .unwrap();
        assert_eq!(change, PeerChange::Updated { previous: peer });
        let peers = manager.list_peers().await;
        assert_eq!(peers.len(), 1, "one id must own exactly one tunnel");
        assert_eq!(peers[0].mesh_ip, "10.200.9.3");
        let statuses = manager.peer_statuses().await;
        assert_eq!(statuses[0].origin, PeerOrigin::Discovered);
        assert_eq!(statuses[0].capabilities, vec!["video"]);
    }

    #[tokio::test]
    async fn static_peer_is_never_overwritten_or_removed_by_discovery() {
        let manager = test_manager();
        let static_peer = MeshNode::generate("static", "10.200.9.2", 51_820).0;
        manager.add_peer(static_peer.clone()).await.unwrap();

        let mut conflict = static_peer.clone();
        conflict.mesh_ip = "10.200.9.99".to_owned();
        let error = manager
            .add_discovered_peer(conflict, Vec::new())
            .await
            .unwrap_err();
        assert!(
            matches!(error, MeshError::DiscoveryConflict(_)),
            "a static peer must win the conflict: {error:?}"
        );
        assert_eq!(manager.list_peers().await, vec![static_peer.clone()]);

        let error = manager.remove_discovered_peer("static").await.unwrap_err();
        assert!(matches!(error, MeshError::DiscoveryConflict(_)));
        assert_eq!(manager.list_peers().await, vec![static_peer]);
    }

    #[tokio::test]
    async fn discovered_mesh_ip_cannot_collide_with_another_peer() {
        let manager = test_manager();
        let first = MeshNode::generate("first", "10.200.9.2", 51_820).0;
        let second = MeshNode::generate("second", "10.200.9.2", 51_821).0;
        manager
            .add_discovered_peer(first, Vec::new())
            .await
            .unwrap();
        let error = manager
            .add_discovered_peer(second, Vec::new())
            .await
            .unwrap_err();
        assert!(
            matches!(error, MeshError::DiscoveryConflict(_)),
            "two peer ids must not share one mesh IP: {error:?}"
        );

        let own_ip = MeshNode::generate("intruder", "10.200.9.1", 51_822).0;
        let error = manager
            .add_discovered_peer(own_ip, Vec::new())
            .await
            .unwrap_err();
        assert!(
            matches!(error, MeshError::DiscoveryConflict(_)),
            "a discovered peer must not take this node's own mesh IP: {error:?}"
        );
    }

    /// Factory that can fail one peer on demand and records key generations.
    struct RotatingTestFactory {
        fail_peer: StdMutex<Option<String>>,
        created: StdMutex<Vec<String>>,
        keys: StdMutex<Vec<String>>,
    }

    impl RotatingTestFactory {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                fail_peer: StdMutex::new(None),
                created: StdMutex::new(Vec::new()),
                keys: StdMutex::new(Vec::new()),
            })
        }

        fn fail_for(&self, peer_id: &str) {
            *self
                .fail_peer
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some(peer_id.to_owned());
        }
    }

    impl TunnelFactory for RotatingTestFactory {
        fn create(
            &self,
            local_key: &WgPrivateKey,
            peer: &MeshNode,
            _index: u32,
        ) -> Result<Box<dyn PeerTunnel>> {
            if self
                .fail_peer
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .as_deref()
                == Some(peer.id.as_str())
            {
                return Err(MeshError::TunnelError(format!(
                    "injected failure for {}",
                    peer.id
                )));
            }
            self.created
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(peer.id.clone());
            self.keys
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(local_key.public_key().0);
            let fake = FakeTunnel::new();
            Ok(Box::new(FakeTunnel {
                state: fake.state.clone(),
            }))
        }
    }

    #[tokio::test]
    async fn rotation_updates_every_peer_and_reports_a_secret_free_status() {
        let factory = RotatingTestFactory::new();
        let manager = test_manager_with_factory(factory.clone());
        let peer_a = MeshNode::generate("peer-a", "10.200.9.2", 51_820).0;
        let peer_b = MeshNode::generate("peer-b", "10.200.9.3", 51_821).0;
        insert_fake_peer(&manager, peer_a, &FakeTunnel::new()).await;
        insert_fake_peer(&manager, peer_b, &FakeTunnel::new()).await;

        let old_public = manager.public_key();
        let status = manager
            .rotate_keys(Duration::from_secs(30))
            .await
            .expect("rotation succeeds");

        assert_eq!(status.key_id, 2);
        assert_ne!(status.public_key, old_public.0);
        assert_eq!(manager.public_key().0, status.public_key);
        assert_eq!(status.grace_secs, 30);
        assert!(status.grace_remaining_secs.is_some());
        assert_eq!(
            status.previous_fingerprint,
            Some(crate::rotation::fingerprint(&old_public))
        );
        assert_eq!(
            factory
                .created
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .len(),
            2,
            "every peer tunnel must be rebuilt with the new key"
        );
        let serialized = serde_json::to_string(&status).unwrap();
        assert!(!serialized.contains("private"));
        assert!(!serialized.contains("secret"));

        {
            let guard = manager.inner.read().await;
            for peer in guard.peers.values() {
                assert!(peer.grace_tunnel.is_some(), "old tunnel kept for grace");
                assert!(peer.grace_expires.is_some());
            }
        }
    }

    #[tokio::test]
    async fn rotation_failure_rolls_back_the_key_and_every_peer() {
        let factory = RotatingTestFactory::new();
        factory.fail_for("peer-b");
        let manager = test_manager_with_factory(factory);
        let peer_a = MeshNode::generate("peer-a", "10.200.9.2", 51_820).0;
        let peer_b = MeshNode::generate("peer-b", "10.200.9.3", 51_821).0;
        insert_fake_peer(&manager, peer_a, &FakeTunnel::new()).await;
        insert_fake_peer(&manager, peer_b, &FakeTunnel::new()).await;
        let before = manager.rotation_status();

        let error = manager
            .rotate_keys(Duration::from_secs(30))
            .await
            .expect_err("a peer that cannot be rebuilt must reject rotation");
        assert!(
            matches!(error, MeshError::RotationFailed(_)),
            "typed rotation failure, got {error:?}"
        );

        let after = manager.rotation_status();
        assert_eq!(after, before, "status/key must be unchanged after rollback");
        let guard = manager.inner.read().await;
        for peer in guard.peers.values() {
            assert!(
                peer.grace_tunnel.is_none(),
                "no peer may keep a half-applied grace tunnel"
            );
        }
    }

    #[tokio::test]
    async fn grace_expiry_removes_the_old_tunnels_and_previous_key() {
        let manager = test_manager();
        let peer = MeshNode::generate("peer", "10.200.9.2", 51_820).0;
        insert_fake_peer(&manager, peer, &FakeTunnel::new()).await;
        let status = manager.rotate_keys(Duration::from_secs(30)).await.unwrap();
        assert!(status.grace_remaining_secs.is_some());

        let future = Instant::now() + Duration::from_secs(31);
        assert!(
            manager.sweep_grace_at(future).await,
            "expiry must clear the grace window"
        );
        let after = manager.rotation_status();
        assert_eq!(after.grace_remaining_secs, None);
        assert_eq!(after.previous_fingerprint, None);
        let guard = manager.inner.read().await;
        for peer in guard.peers.values() {
            assert!(peer.grace_tunnel.is_none());
            assert!(peer.grace_expires.is_none());
        }
    }

    #[tokio::test]
    async fn rotation_accepts_only_an_authorized_previous_peer_key() {
        let manager = test_manager();
        let old = MeshNode::generate("peer", "10.200.9.2", 51_820).0;
        manager
            .add_discovered_peer(old.clone(), Vec::new())
            .await
            .unwrap();
        let mut rotated = old.clone();
        rotated.wg_pubkey = WgPrivateKey::generate().public_key();

        let error = manager
            .add_discovered_peer(rotated.clone(), Vec::new())
            .await
            .unwrap_err();
        assert!(
            matches!(error, MeshError::DiscoveryConflict(_)),
            "an unrelated key change remains a conflict: {error:?}"
        );

        let change = manager
            .add_discovered_peer_rotated(
                rotated.clone(),
                Vec::new(),
                None,
                vec![old.wg_pubkey.0.clone()],
            )
            .await
            .unwrap();
        assert_eq!(
            change,
            PeerChange::Updated {
                previous: old.clone()
            }
        );
        assert_eq!(manager.list_peers().await, vec![rotated]);
    }
}
