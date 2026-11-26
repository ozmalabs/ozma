// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Bounded node-side mesh service.
//!
//! This module is the honest bridge between the controller-facing node API and
//! the `ozma-mesh` WireGuard/mDNS crate. It deliberately separates:
//!
//! * **configuration** — opt-in `--mesh` plus a mesh IPv4 address, a WireGuard
//!   private-key file, and optional peers;
//! * **prerequisites** — a missing key, missing root for kernel WireGuard, or a
//!   missing `wg` binary is a typed [`MeshServiceError`], never a fake peer;
//! * **runtime state** — `not-configured`, `stopped`, `running`, `failed`, or
//!   `unsupported`, with a bounded event log.
//!
//! The default node behavior is unchanged: without `--mesh` the service is not
//! constructed, no socket is bound, and `/api/v1/connect/*` reports
//! `state:"not-configured"` without fabricated peers or tunnel counts.
//!
//! `--mesh-kernel` selects the kernel WireGuard backend
//! ([`ozma_mesh::KernelWireguard`]): it creates a kernel `wireguard` link,
//! configures keys/peers/routes through `ip`/`wg`, and exposes the kernel's own
//! handshake and byte counters. The default remains the userspace boringtun
//! backend in [`ozma_mesh::MeshManager`]. A missing root, module, or tool is a
//! typed prerequisite failure, never a fabricated tunnel.

use std::collections::{BTreeMap, VecDeque};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use async_trait::async_trait;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use ozma_mesh::dataplane::{
    CommandRunner, DataplaneConfig, DataplaneError, DataplaneState, DataplaneStatus, MeshDataplane,
    Sysfs, SystemCommandRunner, SystemSysfs,
};
#[cfg(test)]
use ozma_mesh::dataplane::{CommandSpec, FileOpener, NatConfig};
use ozma_mesh::pump::{
    ManagerPacketIo, PacketPump, PumpConfig, PumpError, PumpState, PumpStatus, SystemTunProvider,
    TunProvider, TunnelIo,
};
#[cfg(test)]
use ozma_mesh::pump::{PumpStats, TunIo};
use ozma_mesh::{
    ClusterSigningKey, DiscoveryAction, DiscoveryConfig, DiscoveryEngine, DiscoveryStatus,
    KernelWgSettings, KernelWgStatus, KernelWireguard, KeyRotation, MdnsBrowser, MeshManager,
    MeshNode, PeerChange, PeerOrigin, RawAdvertisement, RotationSchedule, RotationScheduleStatus,
    RotationStatus, WgPrivateKey, DEFAULT_MAX_SKEW,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// Maximum number of events retained by the bounded log.
pub const MESH_EVENT_CAPACITY: usize = 64;
/// Largest `?limit=` accepted by `GET /api/v1/connect/events`.
pub const MESH_EVENT_MAX_LIMIT: usize = MESH_EVENT_CAPACITY;
/// Controller-facing capability name for this service.
pub const MESH_CAPABILITY: &str = "mesh";
/// Advertised surfaces when the service is compiled in (even while disabled).
pub const MESH_SURFACES: &[&str] = &[
    "/api/v1/connect/status",
    "/api/v1/connect/peers",
    "/api/v1/connect/events",
    "/api/v1/connect/rotate",
];

/// Typed prerequisite/runtime failure.
///
/// The controller can distinguish an honest absence from a transport failure
/// by the stable `code`; no route ever turns a missing key or root into a fake
/// peer or tunnel.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeshServiceError {
    /// Stable machine-readable code, for example `mesh.key-missing`.
    pub code: String,
    /// Human-readable reason with no token or key material.
    pub reason: String,
}

impl MeshServiceError {
    pub fn new(code: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            reason: reason.into(),
        }
    }

    /// Whether this error is a typed "this node cannot support mesh as
    /// configured" state (501) rather than a transient runtime failure.
    pub fn is_unsupported(&self) -> bool {
        matches!(
            self.code.as_str(),
            "mesh.key-missing"
                | "mesh.key-invalid"
                | "mesh.root-required"
                | "mesh.wg-missing"
                | "mesh.wireguard-unavailable"
                | "mesh.not-configured"
                | "mesh.ip-invalid"
                | "mesh.peer-invalid"
                | "mesh.dataplane-unavailable"
                | "mesh.dataplane-invalid"
                | "mesh.dataplane-not-configured"
                | "mesh.tun-unsupported"
                | "mesh.discovery-invalid"
                | "mesh.cluster-key-invalid"
                | "mesh.rotation-invalid"
                | "mesh.rotation-unsupported"
        )
    }
}

impl From<DataplaneError> for MeshServiceError {
    fn from(error: DataplaneError) -> Self {
        Self {
            code: error.code,
            reason: error.reason,
        }
    }
}

impl From<PumpError> for MeshServiceError {
    fn from(error: PumpError) -> Self {
        Self {
            code: error.code,
            reason: error.reason,
        }
    }
}

/// Whether the mesh service is active.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MeshState {
    /// `--mesh`/`OZMA_MESH` was not supplied.
    NotConfigured,
    /// Configured but not started yet.
    Stopped,
    /// Startup is in progress.
    Starting,
    /// Manager started and the receive loop is active.
    Running,
    /// The node cannot support the requested mesh mode.
    Unsupported,
    /// A runtime operation failed.
    Failed,
}

/// Per-peer tunnel state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MeshTunnelState {
    /// Peer registered, no handshake observed.
    Idle,
    /// A handshake is in progress (reported by a backend that knows).
    Handshaking,
    /// boringtun reports a completed handshake.
    Established,
    /// The tunnel is unusable.
    Failed,
}

/// One known peer and its honest tunnel state.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MeshPeerView {
    pub id: String,
    pub mesh_ip: String,
    pub wg_port: u16,
    pub wg_pubkey: String,
    /// `static` or `discovered`.
    #[serde(default)]
    pub origin: PeerOrigin,
    /// Capabilities advertised by a discovered peer.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
    /// WireGuard UDP endpoint actually used for transport.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    pub tunnel: MeshTunnelState,
    pub established: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_handshake_age_secs: Option<u64>,
    pub tx_bytes: usize,
    pub rx_bytes: usize,
    pub loss: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rtt_ms: Option<u32>,
}

/// Local mesh identity. The private key is never part of this document.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeshIdentity {
    pub id: String,
    pub mesh_ip: String,
    pub wg_port: u16,
    pub wg_pubkey: String,
    /// `userspace-boringtun`, `kernel`, `fake`, or `unavailable`.
    pub wireguard: String,
}

/// Event severity/kind in the bounded log.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MeshEventKind {
    Started,
    Stopped,
    PeerAdded,
    PeerRemoved,
    PeerDiscovered,
    TunnelState,
    Dataplane,
    Route,
    Conflict,
    Rotation,
    Error,
    Surface,
}

/// One bounded event-log entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeshEvent {
    pub seq: u64,
    pub at_unix_ms: u64,
    pub kind: MeshEventKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer_id: Option<String>,
    pub message: String,
}

#[derive(Default)]
struct EventLogInner {
    next_seq: u64,
    events: VecDeque<MeshEvent>,
}

/// Cloneable bounded event log. It is shared by the service and its backend so
/// backend-driven handshake transitions appear in the same ordered stream.
#[derive(Clone)]
pub struct MeshEventLog {
    capacity: usize,
    inner: Arc<Mutex<EventLogInner>>,
    next_seq: Arc<AtomicU64>,
}

impl MeshEventLog {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            inner: Arc::new(Mutex::new(EventLogInner::default())),
            next_seq: Arc::new(AtomicU64::new(1)),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, EventLogInner> {
        self.inner.lock().unwrap_or_else(|error| error.into_inner())
    }

    /// Record one event, evicting the oldest entry beyond the bound.
    pub fn record(
        &self,
        kind: MeshEventKind,
        message: impl Into<String>,
        peer_id: Option<String>,
    ) -> MeshEvent {
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);
        let event = MeshEvent {
            seq,
            at_unix_ms: unix_millis(),
            kind,
            peer_id,
            message: message.into(),
        };
        let mut inner = self.lock();
        inner.next_seq = seq + 1;
        inner.events.push_back(event.clone());
        while inner.events.len() > self.capacity {
            inner.events.pop_front();
        }
        event
    }

    /// Snapshot the newest `limit` events in chronological order.
    pub fn snapshot(&self, limit: usize) -> Vec<MeshEvent> {
        let inner = self.lock();
        let skip = inner.events.len().saturating_sub(limit.min(self.capacity));
        inner.events.iter().skip(skip).cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.lock().events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// Mesh configuration built from the node CLI.
#[derive(Clone, Debug)]
pub struct MeshServiceConfig {
    /// Stable human-readable node identity.
    pub node_id: String,
    /// IPv4 address assigned to this node on the mesh.
    pub mesh_ip: String,
    /// WireGuard UDP port (must be non-zero).
    pub wg_port: u16,
    /// Path to a base64 X25519 private key file.
    pub key_file: Option<PathBuf>,
    /// Kernel WireGuard mode requires root.
    pub requires_root: bool,
    /// Kernel WireGuard mode requires the `wg` binary.
    pub requires_wg: bool,
    /// Select and configure the kernel WireGuard backend. `None` keeps the
    /// default userspace boringtun backend. In kernel mode the link itself is
    /// the data transport, so no TUN/packet pump is created.
    pub kernel: Option<KernelWgSettings>,
    /// Configured peers.
    pub peers: Vec<MeshNode>,
    /// Optional underlay UDP endpoints for static peers, keyed by node id.
    /// The peer's mesh IP remains its overlay identity and route.
    pub peer_endpoints: BTreeMap<String, SocketAddr>,
    /// Capabilities advertised in this node's mesh mDNS record.
    pub capabilities: Vec<String>,
    /// Opt-in mDNS peer discovery policy. `None` keeps discovery disabled.
    pub discovery: Option<DiscoveryConfig>,
    /// Optional shared Ed25519 cluster key. When set, every advertisement is
    /// signed and discovered advertisements must verify against the derived
    /// public key. The private key is never logged or serialised.
    pub cluster_key: Option<Arc<ClusterSigningKey>>,
    /// Optional automatic key rotation (`--mesh-rotate-hours`). Off by default.
    pub rotation_schedule: Option<RotationSchedule>,
    /// Optional kernel dataplane (TUN, routes, forwarding, NAT). `None` keeps
    /// the service at the userspace WireGuard layer only.
    pub dataplane: Option<DataplaneConfig>,
    /// Optional event-log capacity override (bounded by the constant default).
    pub event_capacity: Option<usize>,
}

impl Default for MeshServiceConfig {
    fn default() -> Self {
        Self {
            node_id: String::new(),
            mesh_ip: String::new(),
            wg_port: 0,
            key_file: None,
            requires_root: false,
            requires_wg: false,
            kernel: None,
            peers: Vec::new(),
            peer_endpoints: BTreeMap::new(),
            capabilities: Vec::new(),
            discovery: None,
            cluster_key: None,
            rotation_schedule: None,
            dataplane: None,
            event_capacity: None,
        }
    }
}

/// Host prerequisites. Kept injectable so tests never require root or a real
/// WireGuard interface.
#[derive(Clone, Copy, Debug)]
pub struct MeshEnvironment {
    pub is_root: bool,
    pub wg_binary: bool,
    pub userspace_wireguard: bool,
    /// `/dev/net/tun` is present.
    pub tun_available: bool,
    /// `ip` is on `PATH`.
    pub ip_binary: bool,
    /// `nft` is on `PATH` (only required for opt-in NAT).
    pub nft_binary: bool,
    /// `/sys/module/wireguard` exists (kernel WireGuard backend).
    pub wireguard_module: bool,
}

impl Default for MeshEnvironment {
    fn default() -> Self {
        Self::detect()
    }
}

impl MeshEnvironment {
    pub fn detect() -> Self {
        Self {
            is_root: unsafe { libc::geteuid() == 0 },
            wg_binary: binary_on_path("wg"),
            userspace_wireguard: true,
            tun_available: std::path::Path::new(ozma_mesh::dataplane::TUN_DEVICE).exists(),
            ip_binary: binary_on_path("ip"),
            nft_binary: binary_on_path("nft"),
            wireguard_module: std::path::Path::new(ozma_mesh::WIREGUARD_MODULE_PATH).exists(),
        }
    }
}

fn binary_on_path(name: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|directory| directory.join(name).is_file()))
        .unwrap_or(false)
}

impl MeshServiceConfig {
    /// Validate the configuration against the host prerequisites.
    pub fn preflight(&self, environment: &MeshEnvironment) -> Result<(), MeshServiceError> {
        let key_present = self.key_file.as_ref().is_some_and(|path| path.is_file());
        self.preflight_with(key_present, environment)
    }

    /// Validate with an explicit key-present flag (used by tests).
    pub fn preflight_with(
        &self,
        key_present: bool,
        environment: &MeshEnvironment,
    ) -> Result<(), MeshServiceError> {
        if self.node_id.trim().is_empty() {
            return Err(MeshServiceError::new(
                "mesh.not-configured",
                "mesh node identity is empty; set --name or OZMA_MESH_NODE_ID",
            ));
        }
        if self.mesh_ip.trim().is_empty() {
            return Err(MeshServiceError::new(
                "mesh.not-configured",
                "mesh IPv4 address is not configured; set --mesh-ip or OZMA_MESH_IP",
            ));
        }
        if self.mesh_ip.trim().parse::<Ipv4Addr>().is_err() {
            return Err(MeshServiceError::new(
                "mesh.ip-invalid",
                "--mesh-ip must be a valid IPv4 address",
            ));
        }
        if self.wg_port == 0 {
            return Err(MeshServiceError::new(
                "mesh.not-configured",
                "mesh WireGuard UDP port must be non-zero",
            ));
        }
        if !environment.userspace_wireguard && !environment.wg_binary {
            return Err(MeshServiceError::new(
                "mesh.wireguard-unavailable",
                "no userspace WireGuard backend and no wg/kernel support is available",
            ));
        }
        if !key_present {
            return Err(MeshServiceError::new(
                "mesh.key-missing",
                "no readable WireGuard private-key file is configured; set --mesh-key-file",
            ));
        }
        if self.requires_root && !environment.is_root {
            return Err(MeshServiceError::new(
                "mesh.root-required",
                "kernel WireGuard requires root; this node is not running as root",
            ));
        }
        if self.requires_wg && !environment.wg_binary {
            return Err(MeshServiceError::new(
                "mesh.wg-missing",
                "kernel WireGuard requires the wg binary, which is not on PATH",
            ));
        }
        if let Some(kernel) = &self.kernel {
            kernel.validate().map_err(MeshServiceError::from)?;
            if kernel.address.to_string() != self.mesh_ip.trim() {
                return Err(MeshServiceError::new(
                    "mesh.dataplane-invalid",
                    "kernel WireGuard address does not match --mesh-ip",
                ));
            }
            if kernel.listen_port != self.wg_port {
                return Err(MeshServiceError::new(
                    "mesh.dataplane-invalid",
                    "kernel WireGuard listen port does not match --mesh-port",
                ));
            }
            if !environment.is_root {
                return Err(MeshServiceError::new(
                    "mesh.root-required",
                    "kernel WireGuard requires root to create/config its link, addresses, \
                     peers, and routes",
                ));
            }
            if !environment.wireguard_module {
                return Err(MeshServiceError::new(
                    "mesh.dataplane-unavailable",
                    "the wireguard kernel module is not loaded; load it with \
                     `modprobe wireguard`",
                ));
            }
            if !environment.ip_binary {
                return Err(MeshServiceError::new(
                    "mesh.dataplane-unavailable",
                    "kernel WireGuard requires the ip binary on PATH",
                ));
            }
            if !environment.wg_binary {
                return Err(MeshServiceError::new(
                    "mesh.dataplane-unavailable",
                    "kernel WireGuard requires the wg binary; install wireguard-tools",
                ));
            }
            for (id, endpoint) in &self.peer_endpoints {
                if !self.peers.iter().any(|peer| &peer.id == id) {
                    return Err(MeshServiceError::new(
                        "mesh.peer-invalid",
                        format!("peer endpoint {id} has no matching configured peer"),
                    ));
                }
                if !endpoint.is_ipv4() || endpoint.port() == 0 {
                    return Err(MeshServiceError::new(
                        "mesh.peer-invalid",
                        format!("peer endpoint for {id} must be IPv4 with a non-zero port"),
                    ));
                }
            }
        }
        if let Some(dataplane) = &self.dataplane {
            dataplane.validate().map_err(MeshServiceError::from)?;
            if !environment.is_root {
                return Err(MeshServiceError::new(
                    "mesh.root-required",
                    "the mesh dataplane requires root to create its TUN link, routes, \
                     forwarding, and firewall state",
                ));
            }
            if !environment.tun_available {
                return Err(MeshServiceError::new(
                    "mesh.dataplane-unavailable",
                    "the mesh dataplane requires /dev/net/tun, which is not present",
                ));
            }
            if !environment.ip_binary {
                return Err(MeshServiceError::new(
                    "mesh.dataplane-unavailable",
                    "the mesh dataplane requires the ip binary on PATH",
                ));
            }
            if dataplane.nat.is_some() && !environment.nft_binary {
                return Err(MeshServiceError::new(
                    "mesh.dataplane-unavailable",
                    "mesh NAT is configured but the nft binary is not on PATH",
                ));
            }
        }
        if let Some(discovery) = &self.discovery {
            discovery.validate().map_err(|error| {
                MeshServiceError::new("mesh.discovery-invalid", error.to_string())
            })?;
        }
        if let Some(schedule) = &self.rotation_schedule {
            schedule
                .validate()
                .map_err(|reason| MeshServiceError::new("mesh.rotation-invalid", reason))?;
        }
        Ok(())
    }
}

/// Backend abstraction: the real userspace manager or an in-memory fake for
/// deterministic tests.
#[async_trait]
pub trait MeshBackend: Send + Sync {
    async fn start(&self) -> Result<(), MeshServiceError>;
    async fn stop(&self) -> Result<(), MeshServiceError>;
    async fn add_peer(&self, node: MeshNode) -> Result<(), MeshServiceError>;
    async fn remove_peer(&self, peer_id: &str) -> Result<(), MeshServiceError>;
    /// Add/update a peer learned from an authorized advertisement. Return the
    /// change so the service can program the matching `/32` route.
    async fn add_discovered_peer(
        &self,
        node: MeshNode,
        capabilities: Vec<String>,
        underlay_ip: Option<Ipv4Addr>,
    ) -> Result<PeerChange, MeshServiceError> {
        let _ = (capabilities, underlay_ip);
        self.add_peer(node).await.map(|_| PeerChange::Added)
    }
    /// Remove a discovered peer, returning its identity when it existed.
    async fn remove_discovered_peer(
        &self,
        peer_id: &str,
    ) -> Result<Option<MeshNode>, MeshServiceError> {
        self.remove_peer(peer_id).await.map(|_| None)
    }
    /// Start a WireGuard handshake. Discovery calls this only after validation.
    async fn initiate_handshake(&self, _peer_id: &str) -> Result<(), MeshServiceError> {
        Ok(())
    }

    /// Update a discovered peer whose public key rotated during a grace
    /// window. Backends that cannot prove the rotation keep the default
    /// conflict behavior.
    async fn add_discovered_peer_rotated(
        &self,
        node: MeshNode,
        capabilities: Vec<String>,
        underlay_ip: Option<Ipv4Addr>,
        previous_public_keys: Vec<String>,
    ) -> Result<PeerChange, MeshServiceError> {
        let _ = previous_public_keys;
        self.add_discovered_peer(node, capabilities, underlay_ip)
            .await
    }

    /// Rotate the local WireGuard key. A backend without rotation support
    /// returns a typed error rather than pretending it changed the key.
    async fn rotate_keys(&self, grace: Duration) -> Result<RotationStatus, MeshServiceError> {
        let _ = grace;
        Err(MeshServiceError::new(
            "mesh.rotation-unsupported",
            "this mesh backend does not support key rotation",
        ))
    }

    /// Secret-free rotation status, when the backend supports it.
    fn rotation_status(&self) -> Option<RotationStatus> {
        None
    }

    async fn peers(&self) -> Vec<MeshPeerView>;
    fn identity(&self) -> Option<MeshIdentity>;
    fn state(&self) -> MeshState;
    fn error(&self) -> Option<MeshServiceError>;

    /// Packet API exposed by the running tunnel, if it has one. `None` means no
    /// pump can be started; the service never fabricates a tunnel packet path.
    fn tunnel_io(&self) -> Option<Arc<dyn TunnelIo>> {
        None
    }

    /// Refresh a signed advertisement before its signature goes stale.
    ///
    /// Unsigned backends treat this as a no-op.
    async fn refresh_advertisement(&self) -> Result<(), MeshServiceError> {
        Ok(())
    }

    /// Kernel WireGuard link status, present only for the kernel backend.
    fn kernel_status(&self) -> Option<KernelWgStatus> {
        None
    }
}

/// Explicit unsupported backend used when preflight fails.
struct UnsupportedMeshBackend {
    error: MeshServiceError,
    identity: Option<MeshIdentity>,
}

#[async_trait]
impl MeshBackend for UnsupportedMeshBackend {
    async fn start(&self) -> Result<(), MeshServiceError> {
        Err(self.error.clone())
    }

    async fn stop(&self) -> Result<(), MeshServiceError> {
        Ok(())
    }

    async fn add_peer(&self, _node: MeshNode) -> Result<(), MeshServiceError> {
        Err(self.error.clone())
    }

    async fn remove_peer(&self, _peer_id: &str) -> Result<(), MeshServiceError> {
        Err(self.error.clone())
    }

    async fn peers(&self) -> Vec<MeshPeerView> {
        Vec::new()
    }

    fn identity(&self) -> Option<MeshIdentity> {
        self.identity.clone()
    }

    fn state(&self) -> MeshState {
        MeshState::Unsupported
    }

    fn error(&self) -> Option<MeshServiceError> {
        Some(self.error.clone())
    }
}

/// Real backend around [`ozma_mesh::MeshManager`] (userspace boringtun).
pub struct BoringtunMeshBackend {
    identity: Mutex<MeshNode>,
    private_key: Mutex<WgPrivateKey>,
    configured_peers: Vec<MeshNode>,
    capabilities: Vec<String>,
    /// Optional shared cluster key used to sign advertisements.
    cluster_key: Option<Arc<ClusterSigningKey>>,
    /// `MeshManager` is `Send` but not `Sync` (libmdns uses an internal
    /// `RefCell`), so it lives behind a Tokio mutex. The mutex is shared with
    /// the packet-pump adapter so outbound packets use the same peer tunnels.
    manager: Arc<tokio::sync::Mutex<Option<MeshManager>>>,
    /// Packet API exposed to the pump while the tunnel is running.
    tunnel_io: Mutex<Option<Arc<dyn TunnelIo>>>,
    state: Mutex<MeshState>,
    error: Mutex<Option<MeshServiceError>>,
    peer_states: Mutex<BTreeMap<String, MeshTunnelState>>,
    /// Last rotation snapshot, also available while the manager is stopped.
    rotation: Mutex<Option<RotationStatus>>,
    log: MeshEventLog,
}

impl BoringtunMeshBackend {
    pub fn new(
        identity: MeshNode,
        private_key: WgPrivateKey,
        configured_peers: Vec<MeshNode>,
        capabilities: Vec<String>,
        cluster_key: Option<Arc<ClusterSigningKey>>,
        log: MeshEventLog,
    ) -> Self {
        Self {
            identity: Mutex::new(identity),
            private_key: Mutex::new(private_key),
            configured_peers,
            capabilities,
            cluster_key,
            manager: Arc::new(tokio::sync::Mutex::new(None)),
            tunnel_io: Mutex::new(None),
            state: Mutex::new(MeshState::Stopped),
            error: Mutex::new(None),
            peer_states: Mutex::new(BTreeMap::new()),
            rotation: Mutex::new(None),
            log,
        }
    }

    fn identity_snapshot(&self) -> MeshNode {
        self.identity
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    fn set_state(&self, state: MeshState) {
        *self.state.lock().unwrap_or_else(|error| error.into_inner()) = state;
    }

    fn set_error(&self, error: Option<MeshServiceError>) {
        *self.error.lock().unwrap_or_else(|value| value.into_inner()) = error;
    }

    fn map_peer_state(&self, peer: &ozma_mesh::PeerStatus) -> MeshTunnelState {
        let advertised = if peer.established {
            MeshTunnelState::Established
        } else {
            MeshTunnelState::Idle
        };
        let mut states = self
            .peer_states
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let previous = states.insert(peer.node.id.clone(), advertised);
        if previous != Some(advertised) {
            self.log.record(
                MeshEventKind::TunnelState,
                format!(
                    "peer {} tunnel is {}",
                    peer.node.id,
                    tunnel_label(advertised)
                ),
                Some(peer.node.id.clone()),
            );
        }
        advertised
    }
}

#[async_trait]
impl MeshBackend for BoringtunMeshBackend {
    async fn start(&self) -> Result<(), MeshServiceError> {
        if self.state() == MeshState::Running {
            return Ok(());
        }
        self.set_state(MeshState::Starting);
        self.set_error(None);
        *self
            .tunnel_io
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = None;

        let mut manager = self.manager.lock().await;
        if manager.is_none() {
            let identity = self.identity_snapshot();
            let private_key = self
                .private_key
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone();
            match MeshManager::new_with_capabilities_and_signer(
                identity,
                private_key,
                self.capabilities.clone(),
                self.cluster_key.clone(),
            )
            .await
            {
                Ok(created) => *manager = Some(created),
                Err(error) => {
                    let typed = MeshServiceError::new(
                        "mesh.start-failed",
                        format!("WireGuard manager could not start: {error}"),
                    );
                    self.set_state(MeshState::Failed);
                    self.set_error(Some(typed.clone()));
                    self.log
                        .record(MeshEventKind::Error, typed.reason.clone(), None);
                    return Err(typed);
                }
            }
        }
        let manager = manager.as_ref().expect("manager was just created");

        for peer in &self.configured_peers {
            if let Err(error) = manager.add_peer(peer.clone()).await {
                if !matches!(error, ozma_mesh::MeshError::PeerAlreadyExists(_)) {
                    let typed = MeshServiceError::new(
                        "mesh.peer-invalid",
                        format!("peer {} could not be added: {error}", peer.id),
                    );
                    self.set_state(MeshState::Failed);
                    self.set_error(Some(typed.clone()));
                    self.log
                        .record(MeshEventKind::Error, typed.reason.clone(), None);
                    return Err(typed);
                }
            }
        }

        if let Err(error) = manager.start().await {
            let typed = MeshServiceError::new(
                "mesh.start-failed",
                format!("WireGuard receive loop could not start: {error}"),
            );
            self.set_state(MeshState::Failed);
            self.set_error(Some(typed.clone()));
            self.log
                .record(MeshEventKind::Error, typed.reason.clone(), None);
            return Err(typed);
        }

        if let Some(receiver) = manager.take_packet_receiver() {
            let io: Arc<dyn TunnelIo> =
                Arc::new(ManagerPacketIo::new(Arc::clone(&self.manager), receiver));
            *self
                .tunnel_io
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some(io);
        }

        self.set_state(MeshState::Running);
        self.set_error(None);
        Ok(())
    }

    async fn stop(&self) -> Result<(), MeshServiceError> {
        *self
            .tunnel_io
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = None;
        let manager = self.manager.lock().await.take();
        if let Some(manager) = manager {
            manager.stop();
        }
        self.set_state(MeshState::Stopped);
        self.set_error(None);
        Ok(())
    }

    async fn refresh_advertisement(&self) -> Result<(), MeshServiceError> {
        let manager = self.manager.lock().await;
        let Some(manager) = manager.as_ref() else {
            return Ok(());
        };
        manager
            .refresh_advertisement()
            .map(|_| ())
            .map_err(|error| {
                MeshServiceError::new(
                    "mesh.advertisement-refresh-failed",
                    format!("signed advertisement refresh failed: {error}"),
                )
            })
    }

    async fn add_peer(&self, node: MeshNode) -> Result<(), MeshServiceError> {
        let manager = self.manager.lock().await;
        let Some(manager) = manager.as_ref() else {
            return Err(MeshServiceError::new(
                "mesh.not-running",
                "mesh service is not running; start it before adding peers",
            ));
        };
        manager.add_peer(node.clone()).await.map_err(|error| {
            let code = match error {
                ozma_mesh::MeshError::PeerAlreadyExists(_) => "mesh.peer-already-exists",
                ozma_mesh::MeshError::PeerNotFound(_) => "mesh.peer-not-found",
                _ => "mesh.peer-invalid",
            };
            MeshServiceError::new(code, error.to_string())
        })
    }

    async fn remove_peer(&self, peer_id: &str) -> Result<(), MeshServiceError> {
        let manager = self.manager.lock().await;
        let Some(manager) = manager.as_ref() else {
            return Err(MeshServiceError::new(
                "mesh.not-running",
                "mesh service is not running; there is no peer table to change",
            ));
        };
        manager.remove_peer(peer_id).await.map_err(|error| {
            let code = match error {
                ozma_mesh::MeshError::PeerNotFound(_) => "mesh.peer-not-found",
                _ => "mesh.peer-invalid",
            };
            MeshServiceError::new(code, error.to_string())
        })
    }

    async fn add_discovered_peer(
        &self,
        node: MeshNode,
        capabilities: Vec<String>,
        underlay_ip: Option<Ipv4Addr>,
    ) -> Result<PeerChange, MeshServiceError> {
        let manager = self.manager.lock().await;
        let Some(manager) = manager.as_ref() else {
            return Err(MeshServiceError::new(
                "mesh.not-running",
                "mesh service is not running; there is no peer table to change",
            ));
        };
        manager
            .add_discovered_peer_with_endpoint(node, capabilities, underlay_ip)
            .await
            .map_err(map_mesh_peer_error)
    }

    async fn remove_discovered_peer(
        &self,
        peer_id: &str,
    ) -> Result<Option<MeshNode>, MeshServiceError> {
        let manager = self.manager.lock().await;
        let Some(manager) = manager.as_ref() else {
            return Err(MeshServiceError::new(
                "mesh.not-running",
                "mesh service is not running; there is no peer table to change",
            ));
        };
        manager
            .remove_discovered_peer(peer_id)
            .await
            .map_err(map_mesh_peer_error)
    }

    async fn initiate_handshake(&self, peer_id: &str) -> Result<(), MeshServiceError> {
        let manager = self.manager.lock().await;
        let Some(manager) = manager.as_ref() else {
            return Err(MeshServiceError::new(
                "mesh.not-running",
                "mesh service is not running; cannot initiate a handshake",
            ));
        };
        manager
            .initiate_handshake(peer_id)
            .await
            .map_err(map_mesh_peer_error)
    }

    async fn add_discovered_peer_rotated(
        &self,
        node: MeshNode,
        capabilities: Vec<String>,
        underlay_ip: Option<Ipv4Addr>,
        previous_public_keys: Vec<String>,
    ) -> Result<PeerChange, MeshServiceError> {
        let manager = self.manager.lock().await;
        let Some(manager) = manager.as_ref() else {
            return Err(MeshServiceError::new(
                "mesh.not-running",
                "mesh service is not running; there is no peer table to change",
            ));
        };
        manager
            .add_discovered_peer_rotated(node, capabilities, underlay_ip, previous_public_keys)
            .await
            .map_err(map_mesh_peer_error)
    }

    async fn rotate_keys(&self, grace: Duration) -> Result<RotationStatus, MeshServiceError> {
        let manager = self.manager.lock().await;
        let Some(manager) = manager.as_ref() else {
            return Err(MeshServiceError::new(
                "mesh.not-running",
                "mesh service is not running; there is no key to rotate",
            ));
        };
        let status = manager.rotate_keys(grace).await.map_err(|error| {
            let code = match error {
                ozma_mesh::MeshError::RotationInvalid(_) => "mesh.rotation-invalid",
                ozma_mesh::MeshError::RotationFailed(_) => "mesh.rotation-failed",
                _ => "mesh.rotation-failed",
            };
            MeshServiceError::new(code, error.to_string())
        })?;
        // Persist the new identity/key for a future stop/start cycle.
        {
            let mut identity = self
                .identity
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            identity.wg_pubkey = ozma_mesh::WgPublicKey(status.public_key.clone());
        }
        *self
            .private_key
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = manager.private_key_for_restart();
        *self
            .rotation
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(status.clone());
        Ok(status)
    }

    fn rotation_status(&self) -> Option<RotationStatus> {
        if let Ok(manager) = self.manager.try_lock() {
            if let Some(manager) = manager.as_ref() {
                return Some(manager.rotation_status());
            }
        }
        let identity = self.identity_snapshot();
        let mut status = match self
            .rotation
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
        {
            Some(status) => status,
            None => ozma_mesh::KeyRotation::new(
                self.private_key
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .clone(),
            )
            .status(Instant::now()),
        };
        status.public_key = identity.wg_pubkey.0;
        Some(status)
    }

    async fn peers(&self) -> Vec<MeshPeerView> {
        let manager = self.manager.lock().await;
        let Some(manager) = manager.as_ref() else {
            return Vec::new();
        };
        manager
            .peer_statuses()
            .await
            .into_iter()
            .map(|peer| {
                let tunnel = self.map_peer_state(&peer);
                MeshPeerView {
                    id: peer.node.id.clone(),
                    mesh_ip: peer.node.mesh_ip.clone(),
                    wg_port: peer.node.wg_port,
                    wg_pubkey: peer.node.wg_pubkey.0.clone(),
                    origin: peer.origin,
                    capabilities: peer.capabilities.clone(),
                    endpoint: Some(peer.endpoint.clone()),
                    tunnel,
                    established: peer.established,
                    last_handshake_age_secs: peer.last_handshake_age_secs,
                    tx_bytes: peer.tx_bytes,
                    rx_bytes: peer.rx_bytes,
                    loss: peer.loss,
                    rtt_ms: peer.rtt_ms,
                }
            })
            .collect()
    }

    fn identity(&self) -> Option<MeshIdentity> {
        let identity = self.identity_snapshot();
        Some(MeshIdentity {
            id: identity.id.clone(),
            mesh_ip: identity.mesh_ip.clone(),
            wg_port: identity.wg_port,
            wg_pubkey: identity.wg_pubkey.0.clone(),
            wireguard: "userspace-boringtun".to_owned(),
        })
    }

    fn state(&self) -> MeshState {
        *self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn error(&self) -> Option<MeshServiceError> {
        self.error
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    fn tunnel_io(&self) -> Option<Arc<dyn TunnelIo>> {
        self.tunnel_io
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
}

/// Real backend around [`ozma_mesh::KernelWireguard`] (Linux kernel module).
pub struct KernelMeshBackend {
    identity: Mutex<MeshNode>,
    capabilities: Vec<String>,
    /// Optional shared cluster key used to sign advertisements.
    cluster_key: Option<Arc<ClusterSigningKey>>,
    kernel: KernelWireguard,
    advertiser: Mutex<Option<ozma_mesh::MeshAdvertiser>>,
    state: Mutex<MeshState>,
    error: Mutex<Option<MeshServiceError>>,
    peer_states: Mutex<BTreeMap<String, MeshTunnelState>>,
    log: MeshEventLog,
}

impl KernelMeshBackend {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        identity: MeshNode,
        private_key: WgPrivateKey,
        settings: KernelWgSettings,
        configured_peers: Vec<MeshNode>,
        peer_endpoints: BTreeMap<String, SocketAddr>,
        capabilities: Vec<String>,
        cluster_key: Option<Arc<ClusterSigningKey>>,
        runner: Arc<dyn CommandRunner>,
        sysfs: Arc<dyn Sysfs>,
        log: MeshEventLog,
    ) -> Self {
        Self {
            identity: Mutex::new(identity),
            capabilities,
            cluster_key,
            kernel: KernelWireguard::new_with_peer_endpoints(
                settings,
                private_key,
                configured_peers,
                peer_endpoints,
                runner,
                sysfs,
            ),
            advertiser: Mutex::new(None),
            state: Mutex::new(MeshState::Stopped),
            error: Mutex::new(None),
            peer_states: Mutex::new(BTreeMap::new()),
            log,
        }
    }

    fn identity_snapshot(&self) -> MeshNode {
        self.identity
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    fn set_state(&self, state: MeshState) {
        *self.state.lock().unwrap_or_else(|error| error.into_inner()) = state;
    }

    fn set_error(&self, error: Option<MeshServiceError>) {
        *self.error.lock().unwrap_or_else(|value| value.into_inner()) = error;
    }

    fn map_peer_state(&self, peer: &ozma_mesh::PeerStatus) -> MeshTunnelState {
        let advertised = if peer.established {
            MeshTunnelState::Established
        } else {
            MeshTunnelState::Idle
        };
        let mut states = self
            .peer_states
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let previous = states.insert(peer.node.id.clone(), advertised);
        if previous != Some(advertised) {
            self.log.record(
                MeshEventKind::TunnelState,
                format!(
                    "peer {} tunnel is {}",
                    peer.node.id,
                    tunnel_label(advertised)
                ),
                Some(peer.node.id.clone()),
            );
        }
        advertised
    }
}

#[async_trait]
impl MeshBackend for KernelMeshBackend {
    async fn start(&self) -> Result<(), MeshServiceError> {
        if self.state() == MeshState::Running {
            return Ok(());
        }
        self.set_state(MeshState::Starting);
        self.set_error(None);

        let status = match self.kernel.start() {
            Ok(status) => status,
            Err(error) => {
                let typed = MeshServiceError::from(error);
                self.set_state(MeshState::Failed);
                self.set_error(Some(typed.clone()));
                self.log
                    .record(MeshEventKind::Error, typed.reason.clone(), None);
                return Err(typed);
            }
        };

        let port = status.listen_port;
        let identity = self.identity_snapshot();
        match ozma_mesh::MeshAdvertiser::new_with_previous_and_signer(
            &identity,
            &[],
            port,
            &self.capabilities,
            self.cluster_key.clone(),
        ) {
            Ok(advertiser) => {
                *self
                    .advertiser
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = Some(advertiser);
            }
            Err(error) => {
                let _ = self.kernel.stop();
                let typed = MeshServiceError::new(
                    "mesh.start-failed",
                    format!("kernel WireGuard mDNS advertisement could not start: {error}"),
                );
                self.set_state(MeshState::Failed);
                self.set_error(Some(typed.clone()));
                self.log
                    .record(MeshEventKind::Error, typed.reason.clone(), None);
                return Err(typed);
            }
        }

        self.set_state(MeshState::Running);
        self.set_error(None);
        Ok(())
    }

    async fn stop(&self) -> Result<(), MeshServiceError> {
        self.advertiser
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        match self.kernel.stop() {
            Ok(()) => {
                self.set_state(MeshState::Stopped);
                self.set_error(None);
                Ok(())
            }
            Err(error) => {
                let typed = MeshServiceError::from(error);
                self.set_state(MeshState::Failed);
                self.set_error(Some(typed.clone()));
                self.log
                    .record(MeshEventKind::Error, typed.reason.clone(), None);
                Err(typed)
            }
        }
    }

    async fn add_peer(&self, node: MeshNode) -> Result<(), MeshServiceError> {
        self.kernel.add_peer(node).map_err(MeshServiceError::from)
    }

    async fn remove_peer(&self, peer_id: &str) -> Result<(), MeshServiceError> {
        self.kernel
            .remove_peer(peer_id)
            .map_err(MeshServiceError::from)
    }

    async fn refresh_advertisement(&self) -> Result<(), MeshServiceError> {
        let mut advertiser = self
            .advertiser
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(advertiser) = advertiser.as_mut() {
            advertiser.refresh(Instant::now()).map_err(|error| {
                MeshServiceError::new(
                    "mesh.advertisement-refresh-failed",
                    format!("signed advertisement refresh failed: {error}"),
                )
            })?;
        }
        Ok(())
    }

    async fn add_discovered_peer(
        &self,
        node: MeshNode,
        capabilities: Vec<String>,
        underlay_ip: Option<Ipv4Addr>,
    ) -> Result<PeerChange, MeshServiceError> {
        self.kernel
            .add_discovered_peer(node, capabilities, underlay_ip)
            .map_err(MeshServiceError::from)
    }

    async fn remove_discovered_peer(
        &self,
        peer_id: &str,
    ) -> Result<Option<MeshNode>, MeshServiceError> {
        self.kernel
            .remove_discovered_peer(peer_id)
            .map_err(MeshServiceError::from)
    }

    async fn initiate_handshake(&self, peer_id: &str) -> Result<(), MeshServiceError> {
        self.kernel
            .initiate_handshake(peer_id)
            .map_err(MeshServiceError::from)
    }

    async fn add_discovered_peer_rotated(
        &self,
        node: MeshNode,
        capabilities: Vec<String>,
        underlay_ip: Option<Ipv4Addr>,
        previous_public_keys: Vec<String>,
    ) -> Result<PeerChange, MeshServiceError> {
        self.kernel
            .add_discovered_peer_rotated(node, capabilities, underlay_ip, previous_public_keys)
            .map_err(MeshServiceError::from)
    }

    async fn rotate_keys(&self, grace: Duration) -> Result<RotationStatus, MeshServiceError> {
        let old_public = self.identity_snapshot().wg_pubkey.0;
        let status = self
            .kernel
            .rotate_keys(grace)
            .map_err(MeshServiceError::from)?;
        let mut identity = self.identity_snapshot();
        identity.wg_pubkey = ozma_mesh::WgPublicKey(status.public_key.clone());
        let port = self.kernel.settings().listen_port;
        // Drop the old responder before binding a replacement on port 5353.
        self.advertiser
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        let advertiser = ozma_mesh::MeshAdvertiser::new_with_previous_and_signer(
            &identity,
            &[old_public],
            port,
            &self.capabilities,
            self.cluster_key.clone(),
        )
        .map_err(|error| {
            MeshServiceError::new(
                "mesh.rotation-failed",
                format!(
                    "the kernel key rotated but its mDNS advertisement could not be \
                     re-registered: {error}"
                ),
            )
        })?;
        *self
            .advertiser
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(advertiser);
        *self
            .identity
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = identity;
        Ok(status)
    }

    fn rotation_status(&self) -> Option<RotationStatus> {
        Some(self.kernel.rotation_status())
    }

    async fn peers(&self) -> Vec<MeshPeerView> {
        self.kernel
            .peer_statuses()
            .into_iter()
            .map(|peer| {
                let tunnel = self.map_peer_state(&peer);
                MeshPeerView {
                    id: peer.node.id.clone(),
                    mesh_ip: peer.node.mesh_ip.clone(),
                    wg_port: peer.node.wg_port,
                    wg_pubkey: peer.node.wg_pubkey.0.clone(),
                    origin: peer.origin,
                    capabilities: peer.capabilities.clone(),
                    endpoint: Some(peer.endpoint.clone()),
                    tunnel,
                    established: peer.established,
                    last_handshake_age_secs: peer.last_handshake_age_secs,
                    tx_bytes: peer.tx_bytes,
                    rx_bytes: peer.rx_bytes,
                    loss: peer.loss,
                    rtt_ms: peer.rtt_ms,
                }
            })
            .collect()
    }

    fn identity(&self) -> Option<MeshIdentity> {
        let identity = self.identity_snapshot();
        Some(MeshIdentity {
            id: identity.id.clone(),
            mesh_ip: identity.mesh_ip.clone(),
            wg_port: identity.wg_port,
            wg_pubkey: identity.wg_pubkey.0.clone(),
            wireguard: "kernel".to_owned(),
        })
    }

    fn state(&self) -> MeshState {
        *self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn error(&self) -> Option<MeshServiceError> {
        self.error
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    fn kernel_status(&self) -> Option<KernelWgStatus> {
        Some(self.kernel.status())
    }
}

/// In-memory backend used by tests. It never binds a socket, touches root, or
/// invents a handshake.
pub struct FakeMeshBackend {
    identity: Mutex<MeshNode>,
    rotation: Mutex<KeyRotation>,
    rotation_failure: Mutex<Option<MeshServiceError>>,
    state: Mutex<MeshState>,
    peers: Mutex<BTreeMap<String, MeshPeerView>>,
    error: Mutex<Option<MeshServiceError>>,
    start_error: Mutex<Option<MeshServiceError>>,
    tunnel_io: Mutex<Option<Arc<dyn TunnelIo>>>,
    handshakes: Mutex<Vec<String>>,
    log: MeshEventLog,
}

impl FakeMeshBackend {
    pub fn new(identity: MeshNode, log: MeshEventLog) -> Self {
        // Align the fake identity with a real generated key so rotation and
        // identity assertions behave like both production backends.
        let private_key = WgPrivateKey::generate();
        let mut identity = identity;
        identity.wg_pubkey = private_key.public_key();
        Self {
            identity: Mutex::new(identity),
            rotation: Mutex::new(KeyRotation::new(private_key)),
            rotation_failure: Mutex::new(None),
            state: Mutex::new(MeshState::Stopped),
            peers: Mutex::new(BTreeMap::new()),
            error: Mutex::new(None),
            start_error: Mutex::new(None),
            tunnel_io: Mutex::new(None),
            handshakes: Mutex::new(Vec::new()),
            log,
        }
    }

    fn identity_snapshot(&self) -> MeshNode {
        self.identity
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// Force the next rotation to fail with a typed error (tests).
    pub fn set_rotation_failure(&self, error: MeshServiceError) {
        *self
            .rotation_failure
            .lock()
            .unwrap_or_else(|value| value.into_inner()) = Some(error);
    }

    /// Handshake initiations requested by the discovery reconciler (tests).
    pub fn handshakes(&self) -> Vec<String> {
        self.handshakes
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// Force the next `start()` to fail with a typed runtime error.
    pub fn set_start_error(&self, error: MeshServiceError) {
        *self
            .start_error
            .lock()
            .unwrap_or_else(|value| value.into_inner()) = Some(error);
    }

    /// Expose a fake tunnel packet API while the backend is running (tests).
    pub fn set_tunnel_io(&self, io: Arc<dyn TunnelIo>) {
        *self
            .tunnel_io
            .lock()
            .unwrap_or_else(|value| value.into_inner()) = Some(io);
    }

    fn set_state(&self, state: MeshState) {
        *self.state.lock().unwrap_or_else(|error| error.into_inner()) = state;
    }

    /// Drive a tunnel transition for one peer (test hook).
    pub fn set_tunnel_state(
        &self,
        peer_id: &str,
        state: MeshTunnelState,
    ) -> Result<(), MeshServiceError> {
        let mut peers = self.peers.lock().unwrap_or_else(|error| error.into_inner());
        let peer = peers.get_mut(peer_id).ok_or_else(|| {
            MeshServiceError::new("mesh.peer-not-found", format!("peer {peer_id} is unknown"))
        })?;
        peer.tunnel = state;
        peer.established = state == MeshTunnelState::Established;
        if !peer.established {
            peer.last_handshake_age_secs = None;
            peer.rtt_ms = None;
        } else {
            peer.last_handshake_age_secs = Some(0);
        }
        drop(peers);
        self.log.record(
            MeshEventKind::TunnelState,
            format!("peer {peer_id} tunnel is {}", tunnel_label(state)),
            Some(peer_id.to_owned()),
        );
        Ok(())
    }

    async fn upsert_discovered(
        &self,
        node: MeshNode,
        capabilities: Vec<String>,
        underlay_ip: Option<Ipv4Addr>,
        allowed_previous_keys: &[String],
    ) -> Result<PeerChange, MeshServiceError> {
        if self.state() != MeshState::Running {
            return Err(MeshServiceError::new(
                "mesh.not-running",
                "mesh service is not running; start it before adding peers",
            ));
        }
        let mut peers = self.peers.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(existing) = peers.get(&node.id) {
            if existing.origin == PeerOrigin::Static {
                return Err(MeshServiceError::new(
                    "mesh.peer-conflict",
                    format!("static peer {} is authoritative", node.id),
                ));
            }
            if existing.wg_pubkey != node.wg_pubkey.0
                && !allowed_previous_keys
                    .iter()
                    .any(|key| key == &existing.wg_pubkey)
            {
                return Err(MeshServiceError::new(
                    "mesh.peer-conflict",
                    format!("discovered peer {} changed public key", node.id),
                ));
            }
            let endpoint = underlay_ip
                .map(|ip| format!("{ip}:{}", node.wg_port))
                .or_else(|| Some(format!("{}:{}", node.mesh_ip, node.wg_port)));
            if existing.mesh_ip == node.mesh_ip
                && existing.wg_port == node.wg_port
                && existing.wg_pubkey == node.wg_pubkey.0
                && existing.capabilities == capabilities
                && existing.endpoint == endpoint
            {
                return Ok(PeerChange::Unchanged);
            }
            let previous = MeshNode {
                id: existing.id.clone(),
                mesh_ip: existing.mesh_ip.clone(),
                wg_port: existing.wg_port,
                wg_pubkey: ozma_mesh::WgPublicKey(existing.wg_pubkey.clone()),
            };
            let view = peers.get_mut(&node.id).expect("peer was present");
            view.mesh_ip = node.mesh_ip;
            view.wg_port = node.wg_port;
            view.wg_pubkey = node.wg_pubkey.0;
            view.capabilities = capabilities;
            view.endpoint = endpoint;
            view.tunnel = MeshTunnelState::Idle;
            view.established = false;
            view.last_handshake_age_secs = None;
            view.rtt_ms = None;
            return Ok(PeerChange::Updated { previous });
        }
        peers.insert(
            node.id.clone(),
            MeshPeerView {
                id: node.id.clone(),
                mesh_ip: node.mesh_ip.clone(),
                wg_port: node.wg_port,
                wg_pubkey: node.wg_pubkey.0,
                origin: PeerOrigin::Discovered,
                capabilities,
                endpoint: underlay_ip
                    .map(|ip| format!("{ip}:{}", node.wg_port))
                    .or_else(|| Some(format!("{}:{}", node.mesh_ip, node.wg_port))),
                tunnel: MeshTunnelState::Idle,
                established: false,
                last_handshake_age_secs: None,
                tx_bytes: 0,
                rx_bytes: 0,
                loss: 0.0,
                rtt_ms: None,
            },
        );
        Ok(PeerChange::Added)
    }
}

#[async_trait]
impl MeshBackend for FakeMeshBackend {
    async fn start(&self) -> Result<(), MeshServiceError> {
        if let Some(error) = self
            .start_error
            .lock()
            .unwrap_or_else(|value| value.into_inner())
            .clone()
        {
            self.set_state(MeshState::Failed);
            *self.error.lock().unwrap_or_else(|value| value.into_inner()) = Some(error.clone());
            self.log
                .record(MeshEventKind::Error, error.reason.clone(), None);
            return Err(error);
        }
        self.set_state(MeshState::Running);
        *self.error.lock().unwrap_or_else(|value| value.into_inner()) = None;
        Ok(())
    }

    async fn stop(&self) -> Result<(), MeshServiceError> {
        self.set_state(MeshState::Stopped);
        *self.error.lock().unwrap_or_else(|value| value.into_inner()) = None;
        Ok(())
    }

    async fn add_peer(&self, node: MeshNode) -> Result<(), MeshServiceError> {
        if self.state() != MeshState::Running {
            return Err(MeshServiceError::new(
                "mesh.not-running",
                "mesh service is not running; start it before adding peers",
            ));
        }
        let mut peers = self.peers.lock().unwrap_or_else(|error| error.into_inner());
        if peers.contains_key(&node.id) {
            return Err(MeshServiceError::new(
                "mesh.peer-already-exists",
                format!("peer {} already exists", node.id),
            ));
        }
        peers.insert(
            node.id.clone(),
            MeshPeerView {
                id: node.id.clone(),
                mesh_ip: node.mesh_ip.clone(),
                wg_port: node.wg_port,
                wg_pubkey: node.wg_pubkey.0,
                origin: PeerOrigin::Static,
                capabilities: Vec::new(),
                endpoint: Some(format!("{}:{}", node.mesh_ip, node.wg_port)),
                tunnel: MeshTunnelState::Idle,
                established: false,
                last_handshake_age_secs: None,
                tx_bytes: 0,
                rx_bytes: 0,
                loss: 0.0,
                rtt_ms: None,
            },
        );
        Ok(())
    }

    async fn add_discovered_peer(
        &self,
        node: MeshNode,
        capabilities: Vec<String>,
        underlay_ip: Option<Ipv4Addr>,
    ) -> Result<PeerChange, MeshServiceError> {
        self.upsert_discovered(node, capabilities, underlay_ip, &[])
            .await
    }

    async fn add_discovered_peer_rotated(
        &self,
        node: MeshNode,
        capabilities: Vec<String>,
        underlay_ip: Option<Ipv4Addr>,
        previous_public_keys: Vec<String>,
    ) -> Result<PeerChange, MeshServiceError> {
        self.upsert_discovered(node, capabilities, underlay_ip, &previous_public_keys)
            .await
    }

    async fn rotate_keys(&self, grace: Duration) -> Result<RotationStatus, MeshServiceError> {
        if self.state() != MeshState::Running {
            return Err(MeshServiceError::new(
                "mesh.not-running",
                "mesh service is not running; there is no key to rotate",
            ));
        }
        if let Some(error) = self
            .rotation_failure
            .lock()
            .unwrap_or_else(|value| value.into_inner())
            .clone()
        {
            return Err(error);
        }
        let new_key = WgPrivateKey::generate();
        let status = self
            .rotation
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .begin(new_key, grace, Instant::now(), std::time::SystemTime::now());
        {
            let mut identity = self
                .identity
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            identity.wg_pubkey = ozma_mesh::WgPublicKey(status.public_key.clone());
        }
        // A rotated key invalidates established sessions; report idle until a
        // real handshake completes again.
        let mut peers = self.peers.lock().unwrap_or_else(|error| error.into_inner());
        for peer in peers.values_mut() {
            peer.tunnel = MeshTunnelState::Idle;
            peer.established = false;
            peer.last_handshake_age_secs = None;
            peer.rtt_ms = None;
        }
        Ok(status)
    }

    fn rotation_status(&self) -> Option<RotationStatus> {
        Some(
            self.rotation
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .status(Instant::now()),
        )
    }

    async fn remove_discovered_peer(
        &self,
        peer_id: &str,
    ) -> Result<Option<MeshNode>, MeshServiceError> {
        let mut peers = self.peers.lock().unwrap_or_else(|error| error.into_inner());
        let Some(peer) = peers.get(peer_id) else {
            return Ok(None);
        };
        if peer.origin == PeerOrigin::Static {
            return Err(MeshServiceError::new(
                "mesh.peer-conflict",
                format!("static peer {peer_id} cannot be removed by discovery"),
            ));
        }
        let peer = peers.remove(peer_id).expect("peer was present");
        Ok(Some(MeshNode {
            id: peer.id,
            mesh_ip: peer.mesh_ip,
            wg_port: peer.wg_port,
            wg_pubkey: ozma_mesh::WgPublicKey(peer.wg_pubkey),
        }))
    }

    async fn initiate_handshake(&self, peer_id: &str) -> Result<(), MeshServiceError> {
        if !self
            .peers
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .contains_key(peer_id)
        {
            return Err(MeshServiceError::new(
                "mesh.peer-not-found",
                format!("peer {peer_id} is unknown"),
            ));
        }
        self.handshakes
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(peer_id.to_owned());
        Ok(())
    }

    async fn remove_peer(&self, peer_id: &str) -> Result<(), MeshServiceError> {
        let mut peers = self.peers.lock().unwrap_or_else(|error| error.into_inner());
        if peers.remove(peer_id).is_none() {
            return Err(MeshServiceError::new(
                "mesh.peer-not-found",
                format!("peer {peer_id} is unknown"),
            ));
        }
        Ok(())
    }

    async fn peers(&self) -> Vec<MeshPeerView> {
        self.peers
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .values()
            .cloned()
            .collect()
    }

    fn identity(&self) -> Option<MeshIdentity> {
        let identity = self.identity_snapshot();
        Some(MeshIdentity {
            id: identity.id.clone(),
            mesh_ip: identity.mesh_ip.clone(),
            wg_port: identity.wg_port,
            wg_pubkey: identity.wg_pubkey.0.clone(),
            wireguard: "fake".to_owned(),
        })
    }

    fn state(&self) -> MeshState {
        *self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn error(&self) -> Option<MeshServiceError> {
        self.error
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    fn tunnel_io(&self) -> Option<Arc<dyn TunnelIo>> {
        if self.state() != MeshState::Running {
            return None;
        }
        self.tunnel_io
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
}

/// Background discovery task handles.
struct DiscoveryRunner {
    stop: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
}

/// Bounded, cloneable node-side mesh service.
#[derive(Clone)]
pub struct MeshService {
    backend: Arc<dyn MeshBackend>,
    /// Optional kernel dataplane. `None` leaves the service userspace-only.
    dataplane: Option<Arc<MeshDataplane>>,
    /// Optional TUN provider. Only set when both the dataplane and packet pump
    /// are requested; the pump itself starts after both runtime parts are up.
    tun_provider: Option<Arc<dyn TunProvider>>,
    pump: Arc<Mutex<Option<PacketPump>>>,
    pump_error: Arc<Mutex<Option<PumpError>>>,
    pump_monitor: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    /// Deterministic discovery state machine, present only when configured.
    discovery: Option<Arc<Mutex<DiscoveryEngine>>>,
    discovery_started: Arc<AtomicBool>,
    discovery_task: Arc<Mutex<Option<DiscoveryRunner>>>,
    /// Whether `start()` should spawn the real mDNS browser. Tests inject
    /// advertisements directly and never open a network socket.
    discovery_browse: bool,
    /// Optional scheduled rotation policy (off by default).
    rotation_schedule: Option<RotationSchedule>,
    /// Automatic rotation task handle.
    rotation_task: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    /// Signed-advertisement refresh task handle.
    advertisement_task: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    /// Wall-clock time of the next scheduled rotation (status only).
    next_rotation: Arc<Mutex<Option<SystemTime>>>,
    /// Test/diagnostic override for the scheduler delay.
    rotation_interval_override: Option<Duration>,
    log: MeshEventLog,
}

impl MeshService {
    pub fn new(backend: Arc<dyn MeshBackend>, log: MeshEventLog) -> Self {
        Self::with_backend_and_dataplane(backend, None, log)
    }

    /// Build a service from configuration. Preflight/prerequisite failures
    /// produce an explicit unsupported service rather than an error the node
    /// would have to fabricate around.
    pub fn from_config(config: MeshServiceConfig, environment: &MeshEnvironment) -> Self {
        let tun_provider: Option<Arc<dyn TunProvider>> = config
            .dataplane
            .as_ref()
            .map(|_| Arc::new(SystemTunProvider::new()) as Arc<dyn TunProvider>);
        Self::from_config_with_seams_and_tun(
            config,
            environment,
            Arc::new(SystemCommandRunner::new()),
            Arc::new(SystemSysfs::new()),
            tun_provider,
        )
    }

    /// Build a service with injectable dataplane seams (tests/embedding).
    ///
    /// This constructor deliberately does not attach a real TUN; callers that
    /// want the packet pump must use
    /// [`MeshService::from_config_with_seams_and_tun`].
    pub fn from_config_with_seams(
        config: MeshServiceConfig,
        environment: &MeshEnvironment,
        runner: Arc<dyn CommandRunner>,
        sysfs: Arc<dyn Sysfs>,
    ) -> Self {
        Self::from_config_with_seams_and_tun(config, environment, runner, sysfs, None)
    }

    /// Build a service with injectable dataplane seams and an optional TUN
    /// provider. Providing a provider enables the packet pump after both the
    /// dataplane and tunnel are running.
    pub fn from_config_with_seams_and_tun(
        config: MeshServiceConfig,
        environment: &MeshEnvironment,
        runner: Arc<dyn CommandRunner>,
        sysfs: Arc<dyn Sysfs>,
        tun_provider: Option<Arc<dyn TunProvider>>,
    ) -> Self {
        let capacity = config
            .event_capacity
            .unwrap_or(MESH_EVENT_CAPACITY)
            .clamp(1, MESH_EVENT_CAPACITY);
        let log = MeshEventLog::new(capacity);

        if let Err(error) = config.preflight(environment) {
            return Self::unsupported(error, log);
        }

        let discovery = match &config.discovery {
            None => None,
            Some(discovery) => {
                let mut discovery = discovery.clone();
                // The static peer list is the authoritative source; never let a
                // caller build a discovery policy that disagrees with it.
                discovery.local_id = config.node_id.clone();
                discovery.static_peers = config.peers.clone();
                if let Some(key) = &config.cluster_key {
                    match key.verifier(DEFAULT_MAX_SKEW) {
                        Ok(auth) => discovery.auth = Some(auth),
                        Err(error) => {
                            return Self::unsupported(
                                MeshServiceError::new(
                                    "mesh.cluster-key-invalid",
                                    error.to_string(),
                                ),
                                log,
                            )
                        }
                    }
                }
                match DiscoveryEngine::new(discovery) {
                    Ok(engine) => Some(Arc::new(Mutex::new(engine))),
                    Err(error) => {
                        return Self::unsupported(
                            MeshServiceError::new("mesh.discovery-invalid", error.to_string()),
                            log,
                        )
                    }
                }
            }
        };

        let (private_key, identity) = match load_identity(&config) {
            Ok(value) => value,
            Err(error) => return Self::unsupported(error, log),
        };

        // Kernel WireGuard owns its link; a TUN dataplane/pump would create a
        // second, competing interface. Kernel mode therefore takes precedence
        // and programs the `wg` link's own addresses and routes.
        let kernel_requested = config.kernel.is_some();
        let kernel = config.kernel.clone();
        let dataplane = if kernel_requested {
            None
        } else {
            config.dataplane.clone().map(|dataplane| {
                Arc::new(MeshDataplane::new(dataplane, runner.clone(), sysfs.clone()))
            })
        };
        let backend: Arc<dyn MeshBackend> = match kernel {
            Some(settings) => Arc::new(KernelMeshBackend::new(
                identity,
                private_key,
                settings,
                config.peers.clone(),
                config.peer_endpoints.clone(),
                config.capabilities.clone(),
                config.cluster_key.clone(),
                runner,
                sysfs,
                log.clone(),
            )),
            None => Arc::new(BoringtunMeshBackend::new(
                identity,
                private_key,
                config.peers.clone(),
                config.capabilities.clone(),
                config.cluster_key.clone(),
                log.clone(),
            )),
        };
        let mut service = Self::with_backend_dataplane_and_discovery(
            backend,
            dataplane,
            log,
            tun_provider.filter(|_| config.dataplane.is_some() && !kernel_requested),
            discovery,
            true,
        );
        service.rotation_schedule = config.rotation_schedule.clone();
        if let Some(schedule) = &config.rotation_schedule {
            service.log.record(
                MeshEventKind::Started,
                format!(
                    "scheduled key rotation every {}h (+{}s jitter, grace {}s)",
                    schedule.interval_hours, schedule.jitter_secs, schedule.grace_secs
                ),
                None,
            );
        }
        service
    }

    /// Explicit unsupported service for a preflight failure.
    pub fn unsupported(error: MeshServiceError, log: MeshEventLog) -> Self {
        Self {
            backend: Arc::new(UnsupportedMeshBackend {
                error,
                identity: None,
            }),
            dataplane: None,
            tun_provider: None,
            pump: Arc::new(Mutex::new(None)),
            pump_error: Arc::new(Mutex::new(None)),
            pump_monitor: Arc::new(Mutex::new(None)),
            discovery: None,
            discovery_started: Arc::new(AtomicBool::new(false)),
            discovery_task: Arc::new(Mutex::new(None)),
            discovery_browse: false,
            rotation_schedule: None,
            rotation_task: Arc::new(Mutex::new(None)),
            advertisement_task: Arc::new(Mutex::new(None)),
            next_rotation: Arc::new(Mutex::new(None)),
            rotation_interval_override: None,
            log,
        }
    }

    /// Build a service with a caller-supplied backend (tests/embedding).
    pub fn with_backend(backend: Arc<dyn MeshBackend>, log: MeshEventLog) -> Self {
        Self::with_backend_and_dataplane(backend, None, log)
    }

    /// Build a service with caller-supplied backend and dataplane (tests).
    pub fn with_backend_and_dataplane(
        backend: Arc<dyn MeshBackend>,
        dataplane: Option<Arc<MeshDataplane>>,
        log: MeshEventLog,
    ) -> Self {
        Self::with_backend_dataplane_and_tun(backend, dataplane, log, None)
    }

    /// Build a service with caller-supplied backend, dataplane, and TUN provider
    /// (tests/embedding). The provider is only used when a dataplane exists.
    pub fn with_backend_dataplane_and_tun(
        backend: Arc<dyn MeshBackend>,
        dataplane: Option<Arc<MeshDataplane>>,
        log: MeshEventLog,
        tun_provider: Option<Arc<dyn TunProvider>>,
    ) -> Self {
        Self::with_backend_dataplane_and_discovery(
            backend,
            dataplane,
            log,
            tun_provider,
            None,
            false,
        )
    }

    /// Build a service with an injected discovery engine.
    ///
    /// `auto_browse = false` (used by tests) lets callers feed synthetic
    /// [`RawAdvertisement`]s through [`MeshService::apply_discovery_advertisement`]
    /// without opening a network socket.
    pub fn with_backend_dataplane_and_discovery(
        backend: Arc<dyn MeshBackend>,
        dataplane: Option<Arc<MeshDataplane>>,
        log: MeshEventLog,
        tun_provider: Option<Arc<dyn TunProvider>>,
        discovery: Option<Arc<Mutex<DiscoveryEngine>>>,
        auto_browse: bool,
    ) -> Self {
        let has_dataplane = dataplane.is_some();
        Self {
            backend,
            dataplane,
            tun_provider: tun_provider.filter(|_| has_dataplane),
            pump: Arc::new(Mutex::new(None)),
            pump_error: Arc::new(Mutex::new(None)),
            pump_monitor: Arc::new(Mutex::new(None)),
            discovery,
            discovery_started: Arc::new(AtomicBool::new(false)),
            discovery_task: Arc::new(Mutex::new(None)),
            discovery_browse: auto_browse,
            rotation_schedule: None,
            rotation_task: Arc::new(Mutex::new(None)),
            advertisement_task: Arc::new(Mutex::new(None)),
            next_rotation: Arc::new(Mutex::new(None)),
            rotation_interval_override: None,
            log,
        }
    }

    pub fn log(&self) -> MeshEventLog {
        self.log.clone()
    }

    /// Status of the packet pump, present only when both a dataplane and TUN
    /// provider are configured. A configured but not-yet-started pump reports
    /// `stopped` with zero counters, never fabricated traffic.
    pub fn pump_status(&self) -> Option<PumpStatus> {
        self.tun_provider.as_ref()?;
        let dataplane = self.dataplane.as_ref()?;
        let interface = Some(dataplane.config().interface.clone());
        let pump = self
            .pump
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        Some(match pump {
            Some(pump) => pump.status(interface),
            None => {
                let mut status =
                    PumpStatus::stopped(interface, PumpConfig::default().queue_capacity);
                if let Some(error) = self
                    .pump_error
                    .lock()
                    .unwrap_or_else(|value| value.into_inner())
                    .clone()
                {
                    status.state = PumpState::Failed;
                    status.last_error = Some(error);
                }
                status
            }
        })
    }

    fn set_pump_error(&self, error: Option<PumpError>) {
        *self
            .pump_error
            .lock()
            .unwrap_or_else(|value| value.into_inner()) = error;
    }

    /// Handle to the running packet pump, if any (tests/diagnostics).
    pub fn packet_pump(&self) -> Option<PacketPump> {
        self.pump
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// Effective service state. When a dataplane is configured it must be
    /// running too; a failed or still-starting dataplane is never reported as
    /// `running`. A configured pump must also be running, so the service never
    /// claims `running` while the TUN packet path is down.
    pub fn state(&self) -> MeshState {
        let backend = self.backend.state();
        let base = match &self.dataplane {
            None => backend,
            Some(dataplane) => match dataplane.status().state {
                DataplaneState::Failed => MeshState::Failed,
                DataplaneState::Starting => MeshState::Starting,
                DataplaneState::Running => backend,
                DataplaneState::Stopped | DataplaneState::NotConfigured => {
                    if backend == MeshState::Running {
                        MeshState::Starting
                    } else {
                        backend
                    }
                }
            },
        };
        if let Some(pump) = self.pump_status() {
            match pump.state {
                PumpState::Failed => return MeshState::Failed,
                PumpState::Starting => return MeshState::Starting,
                PumpState::Running => {}
                PumpState::Stopped | PumpState::NotConfigured => {
                    if base == MeshState::Running {
                        return MeshState::Starting;
                    }
                }
            }
        }
        base
    }

    pub fn error(&self) -> Option<MeshServiceError> {
        self.backend
            .error()
            .or_else(|| {
                self.pump_status()
                    .and_then(|pump| pump.last_error)
                    .map(MeshServiceError::from)
            })
            .or_else(|| {
                self.dataplane
                    .as_ref()
                    .and_then(|dataplane| dataplane.status().last_error)
                    .map(MeshServiceError::from)
            })
    }

    pub fn identity(&self) -> Option<MeshIdentity> {
        self.backend.identity()
    }

    fn stop_packet_pump(&self) -> bool {
        self.set_pump_error(None);
        if let Some(monitor) = self
            .pump_monitor
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
        {
            monitor.abort();
        }
        let pump = self
            .pump
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        match pump {
            Some(pump) => {
                pump.stop();
                true
            }
            None => false,
        }
    }

    fn spawn_pump_monitor(&self, pump: PacketPump) {
        let service = self.clone();
        let handle = tokio::spawn(async move {
            if let Some(error) = pump.wait_failed().await {
                service.log.record(
                    MeshEventKind::Error,
                    format!("mesh packet pump failed: {}", error.reason),
                    None,
                );
                // A failed pump means the data path is gone. Tear down the
                // tunnel and dataplane instead of leaving a half-live mesh.
                if let Some(dataplane) = &service.dataplane {
                    let _ = dataplane.stop();
                }
                let _ = service.backend.stop().await;
            }
        });
        *self
            .pump_monitor
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(handle);
    }

    /// Start the dataplane first (so an unavailable TUN/root never leaves a
    /// WireGuard socket bound), then the WireGuard manager, and finally the
    /// packet pump only when both are running. Any failure rolls back the
    /// already-started parts in reverse order.
    pub async fn start(&self) -> Result<(), MeshServiceError> {
        self.set_pump_error(None);
        let mut dataplane_started = false;
        if let Some(dataplane) = &self.dataplane {
            dataplane.start().map_err(MeshServiceError::from)?;
            dataplane_started = true;
            self.log.record(
                MeshEventKind::Dataplane,
                format!("mesh dataplane {} is running", dataplane.config().interface),
                None,
            );
        }

        let result = self.backend.start().await;
        if let Err(error) = &result {
            if dataplane_started {
                if let Some(dataplane) = &self.dataplane {
                    let _ = dataplane.stop();
                    self.log.record(
                        MeshEventKind::Dataplane,
                        "mesh dataplane rolled back after manager start failure",
                        None,
                    );
                }
            }
            self.log
                .record(MeshEventKind::Error, error.reason.clone(), None);
            return result;
        }

        if let Some(provider) = &self.tun_provider {
            let dataplane_running = self
                .dataplane
                .as_ref()
                .is_some_and(|dataplane| dataplane.status().state == DataplaneState::Running);
            let tunnel_running = self.backend.state() == MeshState::Running;
            let pump_running = self
                .pump
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .as_ref()
                .is_some_and(|pump| pump.state() == PumpState::Running);

            if !pump_running {
                // A previous stop/failure may have left a handle behind.
                let _ = self.stop_packet_pump();
                let failure = if !dataplane_running {
                    Some(MeshServiceError::new(
                        "mesh.pump-unavailable",
                        "packet pump not started: the mesh dataplane is not running",
                    ))
                } else if !tunnel_running {
                    Some(MeshServiceError::new(
                        "mesh.pump-unavailable",
                        "packet pump not started: the WireGuard tunnel is not running",
                    ))
                } else if let Some(dataplane) = &self.dataplane {
                    let interface = dataplane.config().interface.clone();
                    match self.backend.tunnel_io() {
                        None => Some(MeshServiceError::from(PumpError::unavailable(
                            "the running tunnel exposes no packet API",
                        ))),
                        Some(tunnel) => match provider.open(&interface).await {
                            Err(error) => Some(MeshServiceError::from(error)),
                            Ok(tun) => {
                                let pump = PacketPump::new(tun, tunnel, PumpConfig::default());
                                match pump.start() {
                                    Err(error) => Some(MeshServiceError::from(error)),
                                    Ok(()) => {
                                        self.set_pump_error(None);
                                        *self
                                            .pump
                                            .lock()
                                            .unwrap_or_else(|error| error.into_inner()) =
                                            Some(pump.clone());
                                        self.log.record(
                                            MeshEventKind::Dataplane,
                                            format!("mesh packet pump is running on {interface}"),
                                            None,
                                        );
                                        self.spawn_pump_monitor(pump);
                                        None
                                    }
                                }
                            }
                        },
                    }
                } else {
                    None
                };

                if let Some(error) = failure {
                    let _ = self.stop_packet_pump();
                    self.set_pump_error(Some(PumpError::new(
                        error.code.clone(),
                        error.reason.clone(),
                    )));
                    if let Some(dataplane) = &self.dataplane {
                        let _ = dataplane.stop();
                    }
                    let _ = self.backend.stop().await;
                    self.log
                        .record(MeshEventKind::Error, error.reason.clone(), None);
                    return Err(error);
                }
            }
        }

        self.start_discovery();
        self.start_advertisement_refresh();
        self.start_rotation_scheduler();
        self.log
            .record(MeshEventKind::Started, "mesh service started", None);
        Ok(())
    }

    /// Stop the packet pump, the WireGuard manager, and the dataplane in
    /// reverse start order.
    pub async fn stop(&self) -> Result<(), MeshServiceError> {
        self.stop_discovery();
        self.stop_advertisement_refresh();
        self.stop_rotation_scheduler();
        let pump_stopped = self.stop_packet_pump();
        let dataplane_result = match &self.dataplane {
            Some(dataplane) => dataplane.stop().map_err(MeshServiceError::from),
            None => Ok(()),
        };
        let backend_result = self.backend.stop().await;
        let result = match dataplane_result {
            Err(error) => Err(error),
            Ok(()) => backend_result,
        };
        match &result {
            Ok(()) => {
                if pump_stopped {
                    self.log
                        .record(MeshEventKind::Dataplane, "mesh packet pump stopped", None);
                }
                if self.dataplane.is_some() {
                    self.log
                        .record(MeshEventKind::Dataplane, "mesh dataplane stopped", None);
                }
                self.log
                    .record(MeshEventKind::Stopped, "mesh service stopped", None);
            }
            Err(error) => {
                self.log
                    .record(MeshEventKind::Error, error.reason.clone(), None);
            }
        }
        result
    }

    pub async fn add_peer(&self, node: MeshNode) -> Result<(), MeshServiceError> {
        let peer_id = node.id.clone();
        let mesh_ip = node.mesh_ip.clone();
        let result = self.backend.add_peer(node).await;
        if result.is_ok() {
            if let Some(dataplane) = &self.dataplane {
                let route = mesh_ip
                    .parse::<Ipv4Addr>()
                    .map_err(|_| {
                        MeshServiceError::new(
                            "mesh.peer-invalid",
                            format!("peer {peer_id} has an invalid IPv4 mesh address"),
                        )
                    })
                    .and_then(|route| {
                        dataplane.add_route(route).map_err(|error| {
                            if error.code == "mesh.dataplane-invalid" {
                                MeshServiceError::new("mesh.peer-invalid", error.reason)
                            } else {
                                MeshServiceError::from(error)
                            }
                        })
                    });
                if let Err(error) = route {
                    let _ = self.backend.remove_peer(&peer_id).await;
                    self.log
                        .record(MeshEventKind::Error, error.reason.clone(), Some(peer_id));
                    return Err(error);
                }
            }
        }
        match &result {
            Ok(()) => {
                self.log.record(
                    MeshEventKind::PeerAdded,
                    format!("peer {peer_id} added"),
                    Some(peer_id),
                );
            }
            Err(error) => {
                self.log
                    .record(MeshEventKind::Error, error.reason.clone(), Some(peer_id));
            }
        }
        result
    }

    pub async fn remove_peer(&self, peer_id: &str) -> Result<(), MeshServiceError> {
        let route = if self.dataplane.is_some() {
            self.backend
                .peers()
                .await
                .into_iter()
                .find(|peer| peer.id == peer_id)
                .and_then(|peer| peer.mesh_ip.parse::<Ipv4Addr>().ok())
        } else {
            None
        };
        let result = self.backend.remove_peer(peer_id).await;
        if result.is_ok() {
            if let (Some(dataplane), Some(route)) = (&self.dataplane, route) {
                if let Err(error) = dataplane.remove_route(route) {
                    self.log.record(
                        MeshEventKind::Error,
                        error.reason.clone(),
                        Some(peer_id.to_owned()),
                    );
                }
            }
        }
        match &result {
            Ok(()) => {
                self.log.record(
                    MeshEventKind::PeerRemoved,
                    format!("peer {peer_id} removed"),
                    Some(peer_id.to_owned()),
                );
            }
            Err(error) => {
                self.log.record(
                    MeshEventKind::Error,
                    error.reason.clone(),
                    Some(peer_id.to_owned()),
                );
            }
        }
        result
    }

    pub async fn peers(&self) -> Vec<MeshPeerView> {
        self.backend.peers().await
    }

    pub fn events(&self, limit: usize) -> Vec<MeshEvent> {
        self.log.snapshot(limit)
    }

    /// Rotate the local WireGuard key through the active backend.
    ///
    /// Refuses when the service is not running so a stopped node never reports
    /// a key change it did not install. No private key material is logged.
    pub async fn rotate(&self, grace: Duration) -> Result<RotationStatus, MeshServiceError> {
        if self.state() != MeshState::Running {
            return Err(MeshServiceError::new(
                "mesh.not-running",
                "mesh service is not running; there is no key to rotate",
            ));
        }
        let status = self.backend.rotate_keys(grace).await?;
        self.log.record(
            MeshEventKind::Rotation,
            format!(
                "WireGuard key rotated to key {} (fingerprint {}, grace {}s)",
                status.key_id, status.fingerprint, status.grace_secs
            ),
            None,
        );
        Ok(status)
    }

    /// Current key lifecycle plus the configured schedule, if any.
    pub fn rotation_status(&self) -> Option<RotationStatus> {
        let mut status = self.backend.rotation_status()?;
        if let Some(schedule) = &self.rotation_schedule {
            let next = *self
                .next_rotation
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            status.scheduled = Some(RotationScheduleStatus::from_schedule(schedule, next));
        }
        Some(status)
    }

    fn start_rotation_scheduler(&self) {
        let Some(schedule) = self.rotation_schedule.clone() else {
            return;
        };
        let mut guard = self
            .rotation_task
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if guard.is_some() {
            return;
        }
        let service = self.clone();
        let handle = tokio::spawn(async move {
            loop {
                let delay = service
                    .rotation_interval_override
                    .unwrap_or_else(|| schedule.next_delay());
                let next = std::time::SystemTime::now()
                    .checked_add(delay)
                    .unwrap_or_else(std::time::SystemTime::now);
                *service
                    .next_rotation
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = Some(next);
                tokio::time::sleep(delay).await;
                match service.rotate(schedule.grace()).await {
                    Ok(status) => tracing::info!(
                        key_id = status.key_id,
                        fingerprint = %status.fingerprint,
                        "scheduled mesh key rotation completed"
                    ),
                    Err(error) => {
                        service.log.record(
                            MeshEventKind::Error,
                            format!("scheduled key rotation failed: {}", error.reason),
                            None,
                        );
                    }
                }
            }
        });
        *guard = Some(handle);
    }

    fn stop_rotation_scheduler(&self) {
        if let Some(handle) = self
            .rotation_task
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
        {
            handle.abort();
        }
        *self
            .next_rotation
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = None;
    }

    /// Keep a signed advertisement fresh; unsigned backends no-op.
    fn start_advertisement_refresh(&self) {
        let mut guard = self
            .advertisement_task
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if guard.is_some() {
            return;
        }
        let service = self.clone();
        let handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(5));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                if let Err(error) = service.backend.refresh_advertisement().await {
                    service.log.record(MeshEventKind::Error, error.reason, None);
                }
            }
        });
        *guard = Some(handle);
    }

    fn stop_advertisement_refresh(&self) {
        if let Some(handle) = self
            .advertisement_task
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
        {
            handle.abort();
        }
    }

    /// Discovery status, present only when discovery is configured.
    pub fn discovery_status(&self) -> Option<DiscoveryStatus> {
        let engine = self.discovery.as_ref()?;
        let mut status = engine
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .status(Instant::now());
        status.enabled = self.discovery_started.load(Ordering::SeqCst);
        Some(status)
    }

    /// Feed one synthetic or browsed advertisement through validation,
    /// reconciliation, and route programming. Returns the state-machine
    /// actions for tests/diagnostics.
    pub async fn apply_discovery_advertisement(
        &self,
        raw: RawAdvertisement,
    ) -> Vec<DiscoveryAction> {
        let Some(engine) = &self.discovery else {
            return Vec::new();
        };
        let actions = engine
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .observe(raw, Instant::now());
        for action in &actions {
            self.reconcile_discovery_action(action).await;
        }
        actions
    }

    /// Expire discovered peers past their TTL.
    pub async fn expire_discovery(&self) -> Vec<DiscoveryAction> {
        let Some(engine) = &self.discovery else {
            return Vec::new();
        };
        let actions = engine
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .expire(Instant::now());
        for action in &actions {
            self.reconcile_discovery_action(action).await;
        }
        actions
    }

    async fn reconcile_discovery_action(&self, action: &DiscoveryAction) {
        match action {
            DiscoveryAction::Add(peer) => {
                match self
                    .backend
                    .add_discovered_peer(
                        peer.node.clone(),
                        peer.capabilities.clone(),
                        peer.underlay_ip,
                    )
                    .await
                {
                    Ok(_) => {
                        if let Err(error) = self.add_discovered_route(&peer.node) {
                            let _ = self.backend.remove_discovered_peer(&peer.node.id).await;
                            self.forget_discovered(&peer.node.id);
                            self.log.record(
                                MeshEventKind::Error,
                                error.reason,
                                Some(peer.node.id.clone()),
                            );
                            return;
                        }
                        self.log.record(
                            MeshEventKind::PeerDiscovered,
                            format!(
                                "discovered peer {} at {}:{} (key authorized)",
                                peer.node.id, peer.node.mesh_ip, peer.node.wg_port
                            ),
                            Some(peer.node.id.clone()),
                        );
                        self.log_route_change(
                            &peer.node,
                            format!(
                                "route added for discovered peer {} ({}/32)",
                                peer.node.id, peer.node.mesh_ip
                            ),
                        );
                        self.initiate_discovered_handshake(&peer.node.id).await;
                    }
                    Err(error) => {
                        self.forget_discovered(&peer.node.id);
                        self.record_discovery_refusal(Some(peer.node.id.clone()), &error);
                    }
                }
            }
            DiscoveryAction::Update { peer, previous } => {
                let previous_route = previous.mesh_ip.parse::<Ipv4Addr>().ok();
                match self
                    .backend
                    .add_discovered_peer(
                        peer.node.clone(),
                        peer.capabilities.clone(),
                        peer.underlay_ip,
                    )
                    .await
                {
                    Ok(_) => {
                        let new_route = peer.node.mesh_ip.parse::<Ipv4Addr>().ok();
                        if previous_route != new_route {
                            if let Err(error) = self.add_discovered_route(&peer.node) {
                                // Restore the previous peer so the manager and
                                // route table stay consistent, then retry on the
                                // next advertisement.
                                let _ = self
                                    .backend
                                    .add_discovered_peer(
                                        previous.clone(),
                                        peer.capabilities.clone(),
                                        peer.underlay_ip,
                                    )
                                    .await;
                                self.forget_discovered(&peer.node.id);
                                self.log.record(
                                    MeshEventKind::Error,
                                    error.reason,
                                    Some(peer.node.id.clone()),
                                );
                                return;
                            }
                            if let Some(route) = previous_route {
                                self.remove_discovered_route(route, &peer.node.id);
                            }
                        }
                        self.log.record(
                            MeshEventKind::PeerDiscovered,
                            format!(
                                "discovered peer {} updated to {}:{}",
                                peer.node.id, peer.node.mesh_ip, peer.node.wg_port
                            ),
                            Some(peer.node.id.clone()),
                        );
                        self.log_route_change(
                            &peer.node,
                            format!(
                                "route reconciled for discovered peer {} ({}/32)",
                                peer.node.id, peer.node.mesh_ip
                            ),
                        );
                        self.initiate_discovered_handshake(&peer.node.id).await;
                    }
                    Err(error) => {
                        self.forget_discovered(&peer.node.id);
                        self.record_discovery_refusal(Some(peer.node.id.clone()), &error);
                    }
                }
            }
            DiscoveryAction::KeyRotated { peer, previous } => {
                let previous_route = previous.mesh_ip.parse::<Ipv4Addr>().ok();
                match self
                    .backend
                    .add_discovered_peer_rotated(
                        peer.node.clone(),
                        peer.capabilities.clone(),
                        peer.underlay_ip,
                        vec![previous.wg_pubkey.0.clone()],
                    )
                    .await
                {
                    Ok(_) => {
                        let new_route = peer.node.mesh_ip.parse::<Ipv4Addr>().ok();
                        if previous_route != new_route {
                            if let Err(error) = self.add_discovered_route(&peer.node) {
                                let _ = self
                                    .backend
                                    .add_discovered_peer_rotated(
                                        previous.clone(),
                                        peer.capabilities.clone(),
                                        peer.underlay_ip,
                                        vec![peer.node.wg_pubkey.0.clone()],
                                    )
                                    .await;
                                self.forget_discovered(&peer.node.id);
                                self.log.record(
                                    MeshEventKind::Error,
                                    error.reason,
                                    Some(peer.node.id.clone()),
                                );
                                return;
                            }
                            if let Some(route) = previous_route {
                                self.remove_discovered_route(route, &peer.node.id);
                            }
                        }
                        self.log.record(
                            MeshEventKind::Rotation,
                            format!(
                                "discovered peer {} rotated its public key to {}",
                                peer.node.id,
                                ozma_mesh::fingerprint(&peer.node.wg_pubkey)
                            ),
                            Some(peer.node.id.clone()),
                        );
                        self.initiate_discovered_handshake(&peer.node.id).await;
                    }
                    Err(error) => {
                        self.forget_discovered(&peer.node.id);
                        self.record_discovery_refusal(Some(peer.node.id.clone()), &error);
                    }
                }
            }
            DiscoveryAction::Remove(peer) => {
                let _ = self.backend.remove_discovered_peer(&peer.node.id).await;
                if let Ok(route) = peer.node.mesh_ip.parse::<Ipv4Addr>() {
                    self.remove_discovered_route(route, &peer.node.id);
                }
                self.log.record(
                    MeshEventKind::PeerRemoved,
                    format!(
                        "discovered peer {} expired after the discovery TTL",
                        peer.node.id
                    ),
                    Some(peer.node.id.clone()),
                );
            }
            DiscoveryAction::SignatureInvalid { id, reason } => {
                self.log.record(
                    MeshEventKind::Conflict,
                    format!("mesh.discovery-signature-invalid: {reason}"),
                    id.clone(),
                );
            }
            DiscoveryAction::Stale { id, reason } => {
                self.log.record(
                    MeshEventKind::Conflict,
                    format!("mesh.discovery-stale: {reason}"),
                    id.clone(),
                );
            }
            DiscoveryAction::Conflict { id, reason }
            | DiscoveryAction::Unauthorized { id, reason }
            | DiscoveryAction::Ignored { id, reason } => {
                self.log.record(
                    MeshEventKind::Conflict,
                    format!("discovery refused: {reason}"),
                    id.clone(),
                );
            }
            DiscoveryAction::LimitReached { id, max_peers } => {
                self.log.record(
                    MeshEventKind::Conflict,
                    format!(
                        "discovery refused peer {id}: the configured bound of {max_peers} \
                         discovered peers is full"
                    ),
                    Some(id.clone()),
                );
            }
            DiscoveryAction::Unchanged { .. }
            | DiscoveryAction::StaticMatched { .. }
            | DiscoveryAction::IgnoredSelf { .. } => {
                // Deduplicated / static-authoritative / self: no state change.
            }
        }
    }

    fn add_discovered_route(&self, peer: &MeshNode) -> Result<(), MeshServiceError> {
        let Some(dataplane) = &self.dataplane else {
            return Ok(());
        };
        let route = peer.mesh_ip.parse::<Ipv4Addr>().map_err(|_| {
            MeshServiceError::new(
                "mesh.peer-invalid",
                format!("peer {} has an invalid IPv4 mesh address", peer.id),
            )
        })?;
        dataplane.add_route(route).map_err(MeshServiceError::from)
    }

    fn remove_discovered_route(&self, route: Ipv4Addr, peer_id: &str) {
        let Some(dataplane) = &self.dataplane else {
            return;
        };
        match dataplane.remove_route(route) {
            Ok(()) => {
                self.log.record(
                    MeshEventKind::Route,
                    format!("route removed for discovered peer {peer_id} ({route}/32)"),
                    Some(peer_id.to_owned()),
                );
            }
            Err(error) => {
                self.log
                    .record(MeshEventKind::Error, error.reason, Some(peer_id.to_owned()));
            }
        }
    }

    fn log_route_change(&self, peer: &MeshNode, message: String) {
        if self.dataplane.is_some() {
            self.log
                .record(MeshEventKind::Route, message, Some(peer.id.clone()));
        }
    }

    fn forget_discovered(&self, peer_id: &str) {
        if let Some(engine) = &self.discovery {
            engine
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .forget(peer_id);
        }
    }

    async fn initiate_discovered_handshake(&self, peer_id: &str) {
        if let Err(error) = self.backend.initiate_handshake(peer_id).await {
            self.log.record(
                MeshEventKind::Error,
                format!(
                    "discovered peer {peer_id} handshake could not start: {}",
                    error.reason
                ),
                Some(peer_id.to_owned()),
            );
        }
    }

    fn record_discovery_refusal(&self, peer_id: Option<String>, error: &MeshServiceError) {
        let kind = if error.code == "mesh.peer-conflict" {
            MeshEventKind::Conflict
        } else {
            MeshEventKind::Error
        };
        self.log.record(
            kind,
            format!("discovery refused: {}", error.reason),
            peer_id,
        );
    }

    fn start_discovery(&self) {
        let Some(engine) = &self.discovery else {
            return;
        };
        if self.discovery_started.swap(true, Ordering::SeqCst) {
            return;
        }
        {
            let mut engine = engine.lock().unwrap_or_else(|error| error.into_inner());
            engine.clear();
        }
        if !self.discovery_browse {
            return;
        }

        let config = engine
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .config()
            .clone();
        let browser = match MdnsBrowser::new(config.query_interval) {
            Ok(browser) => browser,
            Err(error) => {
                self.discovery_started.store(false, Ordering::SeqCst);
                self.log.record(
                    MeshEventKind::Error,
                    format!("mDNS discovery browser could not start: {error}"),
                    None,
                );
                return;
            }
        };

        let (sender, mut receiver) = tokio::sync::mpsc::channel::<RawAdvertisement>(64);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let spawned = std::thread::Builder::new()
            .name("mesh-mdns-browse".to_owned())
            .spawn(move || {
                let mut browser = browser;
                while !stop_thread.load(Ordering::SeqCst) {
                    match browser.poll() {
                        Ok(advertisements) => {
                            for advertisement in advertisements {
                                if sender.blocking_send(advertisement).is_err() {
                                    return;
                                }
                            }
                        }
                        Err(error) => {
                            tracing::debug!(%error, "mDNS discovery browse error");
                        }
                    }
                }
            });
        if let Err(error) = spawned {
            self.discovery_started.store(false, Ordering::SeqCst);
            self.log.record(
                MeshEventKind::Error,
                format!("mDNS discovery thread could not start: {error}"),
                None,
            );
            return;
        }

        let service = self.clone();
        let task = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(1));
            loop {
                tokio::select! {
                    advertisement = receiver.recv() => {
                        match advertisement {
                            Some(advertisement) => {
                                service.apply_discovery_advertisement(advertisement).await;
                            }
                            None => break,
                        }
                    }
                    _ = ticker.tick() => {
                        service.expire_discovery().await;
                    }
                }
            }
        });
        *self
            .discovery_task
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(DiscoveryRunner { stop, task });
        self.log.record(
            MeshEventKind::Started,
            format!(
                "mesh discovery browsing {} every {}s (ttl {}s, max {} peers)",
                ozma_mesh::MDNS_FQDN,
                config.query_interval.as_secs(),
                config.ttl.as_secs(),
                config.max_peers
            ),
            None,
        );
    }

    fn stop_discovery(&self) {
        if let Some(runner) = self
            .discovery_task
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
        {
            runner.stop.store(true, Ordering::SeqCst);
            runner.task.abort();
        }
        self.discovery_started.store(false, Ordering::SeqCst);
        if let Some(engine) = &self.discovery {
            engine
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clear();
        }
    }

    pub async fn status(&self) -> MeshStatus {
        let state = self.state();
        let error = self.error();
        let running = state == MeshState::Running;
        let peers = if running {
            Some(self.backend.peers().await)
        } else {
            None
        };
        MeshStatus {
            ok: !matches!(state, MeshState::Unsupported | MeshState::Failed),
            capability: MESH_CAPABILITY,
            supported: !matches!(state, MeshState::Unsupported),
            enabled: true,
            state,
            available: running,
            wireguard: self
                .backend
                .identity()
                .map(|identity| identity.wireguard)
                .unwrap_or_else(|| "unavailable".to_owned()),
            code: error.as_ref().map(|value| value.code.clone()),
            reason: error.as_ref().map(|value| value.reason.clone()),
            identity: self.backend.identity(),
            dataplane: self.dataplane.as_ref().map(|dataplane| dataplane.status()),
            kernel: self.backend.kernel_status(),
            pump: self.pump_status(),
            discovery: self.discovery_status(),
            rotation: self.rotation_status(),
            peer_count: peers.as_ref().map(Vec::len),
            peers,
            event_count: Some(self.log.len()),
            event_capacity: Some(self.log.capacity()),
            capabilities: MESH_SURFACES.to_vec(),
        }
    }
}

fn load_identity(config: &MeshServiceConfig) -> Result<(WgPrivateKey, MeshNode), MeshServiceError> {
    let path = config.key_file.as_ref().ok_or_else(|| {
        MeshServiceError::new(
            "mesh.key-missing",
            "no WireGuard private-key file is configured",
        )
    })?;
    let encoded = std::fs::read_to_string(path).map_err(|error| {
        MeshServiceError::new(
            "mesh.key-missing",
            format!(
                "WireGuard private-key file {} is not readable: {error}",
                path.display()
            ),
        )
    })?;
    let private_key = WgPrivateKey::from_base64(encoded.trim()).map_err(|error| {
        MeshServiceError::new(
            "mesh.key-invalid",
            format!(
                "WireGuard private-key file {} is not valid base64 X25519 material: {error}",
                path.display()
            ),
        )
    })?;
    let node = MeshNode {
        id: config.node_id.clone(),
        wg_pubkey: private_key.public_key(),
        mesh_ip: config.mesh_ip.clone(),
        wg_port: config.wg_port,
    };
    Ok((private_key, node))
}

fn tunnel_label(state: MeshTunnelState) -> &'static str {
    match state {
        MeshTunnelState::Idle => "idle",
        MeshTunnelState::Handshaking => "handshaking",
        MeshTunnelState::Established => "established",
        MeshTunnelState::Failed => "failed",
    }
}

/// Map a library peer error to a stable node-service code.
fn map_mesh_peer_error(error: ozma_mesh::MeshError) -> MeshServiceError {
    let code = match &error {
        ozma_mesh::MeshError::PeerAlreadyExists(_) => "mesh.peer-already-exists",
        ozma_mesh::MeshError::PeerNotFound(_) => "mesh.peer-not-found",
        ozma_mesh::MeshError::DiscoveryConflict(_) => "mesh.peer-conflict",
        ozma_mesh::MeshError::DiscoveryUnauthorized(_) => "mesh.peer-unauthorized",
        ozma_mesh::MeshError::DiscoveryInvalid(_)
        | ozma_mesh::MeshError::KeyError(_)
        | ozma_mesh::MeshError::Base64(_)
        | ozma_mesh::MeshError::InvalidIp(_) => "mesh.peer-invalid",
        _ => "mesh.peer-invalid",
    };
    MeshServiceError::new(code, error.to_string())
}

/// Honest status document for `/api/v1/connect/status`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MeshStatus {
    pub ok: bool,
    pub capability: &'static str,
    pub supported: bool,
    pub enabled: bool,
    pub state: MeshState,
    pub available: bool,
    pub wireguard: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity: Option<MeshIdentity>,
    /// Kernel TUN/routing/forwarding/NAT state, present only when
    /// `--mesh-dataplane` was configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dataplane: Option<DataplaneStatus>,
    /// Kernel WireGuard link state and real `wg show dump` peers, present only
    /// when `--mesh-kernel` selected the kernel backend.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kernel: Option<KernelWgStatus>,
    /// Bounded TUN/WireGuard packet-pump state and real counters, present only
    /// when the dataplane and a TUN provider are configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pump: Option<PumpStatus>,
    /// Opt-in mDNS discovery policy, counters, and discovered peers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub discovery: Option<DiscoveryStatus>,
    /// Current key id/fingerprint, last rotation, and grace remaining. Contains
    /// no private key material.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rotation: Option<RotationStatus>,
    /// Omitted unless the service is running; `Some(0)` is a real empty table.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peers: Option<Vec<MeshPeerView>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_capacity: Option<usize>,
    pub capabilities: Vec<&'static str>,
}

impl MeshStatus {
    /// Default status for a node that never supplied `--mesh`.
    pub fn not_configured() -> Self {
        Self {
            ok: true,
            capability: MESH_CAPABILITY,
            supported: true,
            enabled: false,
            state: MeshState::NotConfigured,
            available: false,
            wireguard: "not-configured".to_owned(),
            code: None,
            reason: Some("mesh service is not enabled (--mesh / OZMA_MESH)".to_owned()),
            identity: None,
            dataplane: None,
            kernel: None,
            pump: None,
            discovery: None,
            rotation: None,
            peer_count: None,
            peers: None,
            event_count: None,
            event_capacity: None,
            capabilities: MESH_SURFACES.to_vec(),
        }
    }
}

#[derive(Clone)]
struct MeshRouteState {
    service: Option<MeshService>,
}

/// Build the bearer-guarded `/api/v1/connect/*` router.
///
/// Merge this into `controller_compat::compat_router` (before the node API
/// bearer layer) so it has exactly the same auth behavior as `/input/*`,
/// `/power/*`, and the other control routes.
pub fn router(service: Option<MeshService>) -> Router {
    Router::new()
        .route("/api/v1/connect/status", get(mesh_status))
        .route("/api/v1/connect/peers", get(mesh_peers))
        .route("/api/v1/connect/events", get(mesh_events))
        .route("/api/v1/connect/rotate", post(mesh_rotate))
        .with_state(MeshRouteState { service })
}

async fn mesh_status(State(state): State<MeshRouteState>) -> Response {
    let Some(service) = state.service else {
        return Json(MeshStatus::not_configured()).into_response();
    };
    let status = service.status().await;
    if status.state == MeshState::Unsupported {
        return unsupported_response(
            "/api/v1/connect/status",
            status.code.as_deref().unwrap_or("mesh.unsupported"),
            status
                .reason
                .as_deref()
                .unwrap_or("mesh is unsupported on this node"),
        );
    }
    Json(status).into_response()
}

#[derive(Debug, Deserialize)]
struct EventsQuery {
    #[serde(default)]
    limit: Option<usize>,
}

/// Explicit confirmation body for `POST /api/v1/connect/rotate`.
#[derive(Debug, Default, Deserialize)]
struct RotateRequest {
    /// Must be exactly `true`; rotation is never implicit.
    #[serde(default)]
    confirm: bool,
    /// Grace window in seconds (default 90, bounded 5..=600).
    #[serde(default)]
    grace_secs: Option<u64>,
}

async fn mesh_rotate(
    State(state): State<MeshRouteState>,
    Json(body): Json<RotateRequest>,
) -> Response {
    let Some(service) = state.service else {
        return unsupported_response(
            "/api/v1/connect/rotate",
            "mesh.not-configured",
            "mesh service is not enabled (--mesh / OZMA_MESH)",
        );
    };
    let status = service.status().await;
    if status.state == MeshState::Unsupported {
        return unsupported_response(
            "/api/v1/connect/rotate",
            status.code.as_deref().unwrap_or("mesh.unsupported"),
            status
                .reason
                .as_deref()
                .unwrap_or("mesh is unsupported on this node"),
        );
    }
    if !body.confirm {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "ok": false,
                "capability": MESH_CAPABILITY,
                "code": "mesh.rotation-unconfirmed",
                "reason": "key rotation requires {\"confirm\":true}",
                "surface": "/api/v1/connect/rotate",
                "rotation": service.rotation_status(),
            })),
        )
            .into_response();
    }
    let grace = Duration::from_secs(
        body.grace_secs
            .unwrap_or(ozma_mesh::DEFAULT_GRACE.as_secs()),
    );
    if let Err(reason) = ozma_mesh::validate_grace(grace) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "ok": false,
                "capability": MESH_CAPABILITY,
                "code": "mesh.rotation-invalid",
                "reason": reason,
                "surface": "/api/v1/connect/rotate",
                "rotation": service.rotation_status(),
            })),
        )
            .into_response();
    }
    match service.rotate(grace).await {
        Ok(rotation) => Json(json!({
            "ok": true,
            "capability": MESH_CAPABILITY,
            "rotation": rotation,
            "identity": service.identity(),
            "capabilities": MESH_SURFACES,
        }))
        .into_response(),
        Err(error) => {
            let http = match error.code.as_str() {
                "mesh.rotation-unsupported" => StatusCode::NOT_IMPLEMENTED,
                "mesh.rotation-invalid" => StatusCode::BAD_REQUEST,
                "mesh.not-running" => StatusCode::CONFLICT,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            };
            (
                http,
                Json(json!({
                    "ok": false,
                    "capability": MESH_CAPABILITY,
                    "code": error.code,
                    "reason": error.reason,
                    "surface": "/api/v1/connect/rotate",
                    "rotation": service.rotation_status(),
                })),
            )
                .into_response()
        }
    }
}

async fn mesh_peers(State(state): State<MeshRouteState>) -> Response {
    let Some(service) = state.service else {
        return Json(json!({
            "ok": true,
            "capability": MESH_CAPABILITY,
            "supported": true,
            "enabled": false,
            "state": MeshState::NotConfigured,
            "available": false,
            "reason": "mesh service is not enabled (--mesh / OZMA_MESH)",
            "capabilities": MESH_SURFACES,
        }))
        .into_response();
    };
    let status = service.status().await;
    if status.state == MeshState::Unsupported {
        return unsupported_response(
            "/api/v1/connect/peers",
            status.code.as_deref().unwrap_or("mesh.unsupported"),
            status
                .reason
                .as_deref()
                .unwrap_or("mesh is unsupported on this node"),
        );
    }
    Json(json!({
        "ok": status.ok,
        "capability": MESH_CAPABILITY,
        "supported": status.supported,
        "enabled": status.enabled,
        "state": status.state,
        "available": status.available,
        "capabilities": status.capabilities,
        "peers": status.peers,
        "count": status.peer_count,
        "code": status.code,
        "reason": status.reason,
    }))
    .into_response()
}

async fn mesh_events(
    State(state): State<MeshRouteState>,
    Query(query): Query<EventsQuery>,
) -> Response {
    let limit = query
        .limit
        .unwrap_or(MESH_EVENT_MAX_LIMIT)
        .clamp(1, MESH_EVENT_MAX_LIMIT);
    let Some(service) = state.service else {
        return Json(json!({
            "ok": true,
            "capability": MESH_CAPABILITY,
            "supported": true,
            "enabled": false,
            "state": MeshState::NotConfigured,
            "available": false,
            "reason": "mesh service is not enabled (--mesh / OZMA_MESH)",
            "events": [],
            "count": 0,
            "capacity": MESH_EVENT_CAPACITY,
            "capabilities": MESH_SURFACES,
        }))
        .into_response();
    };
    let status = service.status().await;
    if status.state == MeshState::Unsupported {
        return unsupported_response(
            "/api/v1/connect/events",
            status.code.as_deref().unwrap_or("mesh.unsupported"),
            status
                .reason
                .as_deref()
                .unwrap_or("mesh is unsupported on this node"),
        );
    }
    let events = service.events(limit);
    Json(json!({
        "ok": status.ok,
        "capability": MESH_CAPABILITY,
        "supported": status.supported,
        "enabled": status.enabled,
        "state": status.state,
        "available": status.available,
        "capabilities": status.capabilities,
        "events": events,
        "count": events.len(),
        "capacity": service.log().capacity(),
        "code": status.code,
        "reason": status.reason,
    }))
    .into_response()
}

fn unsupported_response(surface: &str, code: &str, reason: &str) -> Response {
    let body = json!({
        "ok": false,
        "code": code,
        "surface": surface,
        "capability": MESH_CAPABILITY,
        "supported": false,
        "reason": reason,
        "error": reason,
        "remediation": "Configure a readable WireGuard key and the selected WireGuard mode's \
                        prerequisites, or leave mesh disabled; this node never fabricates peers, \
                        tunnels, or handshake state.",
    });
    (StatusCode::NOT_IMPLEMENTED, Json(body)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn fake_node(id: &str, mesh_ip: &str) -> MeshNode {
        MeshNode::generate(id, mesh_ip, 59_200).0
    }

    fn fake_service(capacity: usize) -> (Arc<FakeMeshBackend>, MeshService) {
        let log = MeshEventLog::new(capacity);
        let backend = Arc::new(FakeMeshBackend::new(
            fake_node("self", "10.200.9.1"),
            log.clone(),
        ));
        let service = MeshService::with_backend(backend.clone(), log);
        (backend, service)
    }

    fn peer(id: &str, mesh_ip: &str) -> MeshNode {
        fake_node(id, mesh_ip)
    }

    #[tokio::test]
    async fn start_stop_and_events_are_honest() {
        let (_backend, service) = fake_service(16);
        assert_eq!(service.state(), MeshState::Stopped);

        service.start().await.unwrap();
        assert_eq!(service.state(), MeshState::Running);
        let status = service.status().await;
        assert!(status.available);
        assert_eq!(status.peer_count, Some(0));
        assert_eq!(status.peers.as_deref(), Some(&[][..]));

        service.stop().await.unwrap();
        assert_eq!(service.state(), MeshState::Stopped);
        let status = service.status().await;
        assert!(!status.available);
        assert!(
            status.peer_count.is_none(),
            "stopped state must not claim a peer count"
        );

        let kinds: Vec<_> = service.events(16).into_iter().map(|e| e.kind).collect();
        assert_eq!(kinds, vec![MeshEventKind::Started, MeshEventKind::Stopped]);
    }

    #[tokio::test]
    async fn peer_add_remove_and_duplicate_are_typed() {
        let (_backend, service) = fake_service(16);
        service.start().await.unwrap();

        service
            .add_peer(peer("peer-a", "10.200.9.2"))
            .await
            .unwrap();
        let status = service.status().await;
        assert_eq!(status.peer_count, Some(1));
        let peers = status.peers.unwrap();
        assert_eq!(peers[0].id, "peer-a");
        assert_eq!(peers[0].tunnel, MeshTunnelState::Idle);
        assert!(!peers[0].established);

        let duplicate = service
            .add_peer(peer("peer-a", "10.200.9.2"))
            .await
            .unwrap_err();
        assert_eq!(duplicate.code, "mesh.peer-already-exists");

        service.remove_peer("peer-a").await.unwrap();
        assert_eq!(service.status().await.peer_count, Some(0));
        let missing = service.remove_peer("peer-a").await.unwrap_err();
        assert_eq!(missing.code, "mesh.peer-not-found");

        let kinds: Vec<_> = service.events(16).into_iter().map(|e| e.kind).collect();
        assert!(kinds.contains(&MeshEventKind::PeerAdded));
        assert!(kinds.contains(&MeshEventKind::PeerRemoved));
    }

    #[tokio::test]
    async fn tunnel_state_is_backend_driven_not_assumed() {
        let (backend, service) = fake_service(16);
        service.start().await.unwrap();
        service
            .add_peer(peer("peer-a", "10.200.9.2"))
            .await
            .unwrap();
        assert_eq!(
            service.status().await.peers.unwrap()[0].tunnel,
            MeshTunnelState::Idle
        );

        backend
            .set_tunnel_state("peer-a", MeshTunnelState::Handshaking)
            .unwrap();
        assert_eq!(
            service.status().await.peers.unwrap()[0].tunnel,
            MeshTunnelState::Handshaking
        );

        backend
            .set_tunnel_state("peer-a", MeshTunnelState::Established)
            .unwrap();
        let view = service.status().await.peers.unwrap().remove(0);
        assert_eq!(view.tunnel, MeshTunnelState::Established);
        assert!(view.established);
        assert_eq!(view.last_handshake_age_secs, Some(0));

        assert!(service
            .events(16)
            .iter()
            .any(|event| event.kind == MeshEventKind::TunnelState));
    }

    #[tokio::test]
    async fn event_log_is_bounded() {
        let (_backend, service) = fake_service(4);
        service.start().await.unwrap();
        for index in 0..10 {
            service
                .add_peer(peer(&format!("peer-{index}"), "10.200.9.2"))
                .await
                .unwrap();
        }
        assert_eq!(service.log().len(), 4);
        let events = service.events(64);
        assert_eq!(events.len(), 4);
        assert_eq!(events.last().unwrap().kind, MeshEventKind::PeerAdded);
        let seqs: Vec<u64> = events.iter().map(|event| event.seq).collect();
        assert!(seqs.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn preflight_reports_key_root_and_wg_typed_errors() {
        let base = MeshServiceConfig {
            node_id: "node".into(),
            mesh_ip: "10.200.9.1".into(),
            wg_port: 51_820,
            ..MeshServiceConfig::default()
        };
        let env = MeshEnvironment {
            is_root: false,
            wg_binary: false,
            userspace_wireguard: true,
            tun_available: true,
            ip_binary: true,
            nft_binary: true,
            wireguard_module: true,
        };
        assert_eq!(
            base.preflight_with(false, &env).unwrap_err().code,
            "mesh.key-missing"
        );

        let kernel = MeshServiceConfig {
            requires_root: true,
            requires_wg: true,
            ..base.clone()
        };
        let error = kernel.preflight_with(true, &env).unwrap_err();
        assert_eq!(error.code, "mesh.root-required");

        let root_env = MeshEnvironment {
            is_root: true,
            wg_binary: false,
            userspace_wireguard: true,
            tun_available: true,
            ip_binary: true,
            nft_binary: true,
            wireguard_module: true,
        };
        let error = kernel.preflight_with(true, &root_env).unwrap_err();
        assert_eq!(error.code, "mesh.wg-missing");

        let complete = MeshEnvironment {
            is_root: true,
            wg_binary: true,
            userspace_wireguard: true,
            tun_available: true,
            ip_binary: true,
            nft_binary: true,
            wireguard_module: true,
        };
        kernel.preflight_with(true, &complete).unwrap();
    }

    #[test]
    fn preflight_reports_kernel_module_and_tool_errors() {
        let base = MeshServiceConfig {
            node_id: "node".into(),
            mesh_ip: "10.200.9.1".into(),
            wg_port: 51_820,
            kernel: Some(KernelWgSettings::new(
                "ozma0",
                Ipv4Addr::new(10, 200, 9, 1),
                24,
                1420,
                51_820,
            )),
            ..MeshServiceConfig::default()
        };
        let complete = MeshEnvironment {
            is_root: true,
            wg_binary: true,
            userspace_wireguard: true,
            tun_available: true,
            ip_binary: true,
            nft_binary: true,
            wireguard_module: true,
        };
        base.preflight_with(true, &complete).unwrap();

        let not_root = MeshEnvironment {
            is_root: false,
            ..complete
        };
        assert_eq!(
            base.preflight_with(true, &not_root).unwrap_err().code,
            "mesh.root-required"
        );

        let no_module = MeshEnvironment {
            wireguard_module: false,
            ..complete
        };
        let error = base.preflight_with(true, &no_module).unwrap_err();
        assert_eq!(error.code, "mesh.dataplane-unavailable");
        assert!(error.reason.contains("wireguard kernel module"));

        let no_wg = MeshEnvironment {
            wg_binary: false,
            ..complete
        };
        assert_eq!(
            base.preflight_with(true, &no_wg).unwrap_err().code,
            "mesh.dataplane-unavailable"
        );

        let no_ip = MeshEnvironment {
            ip_binary: false,
            ..complete
        };
        assert_eq!(
            base.preflight_with(true, &no_ip).unwrap_err().code,
            "mesh.dataplane-unavailable"
        );

        let mut ghost = base.clone();
        ghost.peer_endpoints.insert(
            "ghost".to_owned(),
            SocketAddr::from((Ipv4Addr::new(10, 10, 30, 9), 51_820)),
        );
        assert_eq!(
            ghost.preflight_with(true, &complete).unwrap_err().code,
            "mesh.peer-invalid"
        );
    }

    fn write_test_key() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "ozma-mesh-node-key-{}-{sequence}.key",
            std::process::id()
        ));
        std::fs::write(&path, "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=").unwrap();
        path
    }

    #[tokio::test]
    async fn kernel_configuration_selects_the_kernel_backend() {
        let key_path = write_test_key();
        let config = MeshServiceConfig {
            node_id: "kernel-node".into(),
            mesh_ip: "10.200.9.1".into(),
            wg_port: 51_820,
            kernel: Some(KernelWgSettings::new(
                "ozma0",
                Ipv4Addr::new(10, 200, 9, 1),
                24,
                1420,
                51_820,
            )),
            key_file: Some(key_path.clone()),
            ..MeshServiceConfig::default()
        };
        let environment = MeshEnvironment {
            is_root: true,
            wg_binary: true,
            userspace_wireguard: true,
            tun_available: true,
            ip_binary: true,
            nft_binary: true,
            wireguard_module: true,
        };
        let service = MeshService::from_config_with_seams(
            config,
            &environment,
            Arc::new(NodeRunner::default()),
            Arc::new(NodeSysfs::rooted()),
        );
        let _ = std::fs::remove_file(&key_path);
        assert_eq!(service.state(), MeshState::Stopped);
        assert_eq!(service.identity().unwrap().wireguard.as_str(), "kernel");
        assert!(service.log().is_empty());
        let status = service.status().await;
        assert!(status.kernel.is_some());
        assert_eq!(status.kernel.unwrap().interface, "ozma0");
        assert!(status.dataplane.is_none());
        assert!(status.pump.is_none());
        assert_eq!(status.wireguard, "kernel");
    }

    #[test]
    fn preflight_rejects_empty_and_invalid_configuration() {
        let env = MeshEnvironment {
            is_root: true,
            wg_binary: true,
            userspace_wireguard: true,
            tun_available: true,
            ip_binary: true,
            nft_binary: true,
            wireguard_module: true,
        };
        let empty = MeshServiceConfig::default();
        assert_eq!(
            empty.preflight_with(true, &env).unwrap_err().code,
            "mesh.not-configured"
        );

        let invalid = MeshServiceConfig {
            node_id: "node".into(),
            mesh_ip: "not-an-ip".into(),
            wg_port: 51_820,
            ..MeshServiceConfig::default()
        };
        assert_eq!(
            invalid.preflight_with(true, &env).unwrap_err().code,
            "mesh.ip-invalid"
        );
    }

    #[tokio::test]
    async fn unsupported_service_is_typed_and_has_no_fake_peers() {
        let log = MeshEventLog::new(8);
        let service = MeshService::unsupported(
            MeshServiceError::new("mesh.key-missing", "no key file"),
            log,
        );
        let status = service.status().await;
        assert_eq!(status.state, MeshState::Unsupported);
        assert!(!status.supported);
        assert_eq!(status.code.as_deref(), Some("mesh.key-missing"));
        assert!(status.peer_count.is_none());
        assert!(status.peers.is_none());
        assert!(service.start().await.is_err());
    }

    #[tokio::test]
    async fn route_not_configured_reports_no_counts_or_peers() {
        let app = router(None);
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/connect/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body["state"], "not-configured");
        assert_eq!(body["available"], false);
        assert!(body.get("peer_count").is_none());
        assert!(body.get("peers").is_none());
        assert_eq!(body["capability"], MESH_CAPABILITY);
        assert_eq!(
            body["capabilities"].as_array().unwrap().len(),
            MESH_SURFACES.len()
        );

        let peers = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/connect/peers")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = body_json(peers).await;
        assert_eq!(body["state"], "not-configured");
        assert!(body.get("count").is_none());
        assert!(body.get("peers").is_none());
    }

    #[tokio::test]
    async fn route_unsupported_returns_typed_501() {
        let log = MeshEventLog::new(8);
        let service =
            MeshService::unsupported(MeshServiceError::new("mesh.root-required", "not root"), log);
        let app = router(Some(service));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/connect/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
        let body = body_json(response).await;
        assert_eq!(body["code"], "mesh.root-required");
        assert_eq!(body["supported"], false);
        assert_eq!(body["capability"], MESH_CAPABILITY);
        assert_eq!(body["surface"], "/api/v1/connect/status");
        assert!(body["remediation"]
            .as_str()
            .unwrap()
            .contains("never fabricates"));
    }

    #[tokio::test]
    async fn route_running_reports_peers_and_bounded_events() {
        let (backend, service) = fake_service(3);
        service.start().await.unwrap();
        service
            .add_peer(peer("peer-a", "10.200.9.2"))
            .await
            .unwrap();
        backend
            .set_tunnel_state("peer-a", MeshTunnelState::Established)
            .unwrap();

        let app = router(Some(service));
        let status = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/connect/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(status.status(), StatusCode::OK);
        let body = body_json(status).await;
        assert_eq!(body["state"], "running");
        assert_eq!(body["available"], true);
        assert_eq!(body["peer_count"], 1);
        assert_eq!(body["peers"][0]["id"], "peer-a");
        assert_eq!(body["peers"][0]["tunnel"], "established");
        assert_eq!(body["peers"][0]["established"], true);

        let events = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/connect/events?limit=100")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = body_json(events).await;
        assert_eq!(body["count"], 3);
        assert_eq!(body["capacity"], 3);
        assert_eq!(body["events"].as_array().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn route_failed_start_is_a_runtime_state_not_a_fake_peer() {
        let (backend, service) = fake_service(8);
        backend.set_start_error(MeshServiceError::new("mesh.start-failed", "bind failed"));
        let error = service.start().await.unwrap_err();
        assert_eq!(error.code, "mesh.start-failed");
        let status = service.status().await;
        assert_eq!(status.state, MeshState::Failed);
        assert!(!status.available);
        assert!(status.peer_count.is_none());
        assert!(status.peers.is_none());
        assert_eq!(status.code.as_deref(), Some("mesh.start-failed"));
    }

    #[tokio::test]
    async fn dataplane_running_status_is_honest() {
        let (service, runner) = dataplane_service(dataplane_config(), NodeSysfs::rooted());
        assert_eq!(service.state(), MeshState::Stopped);
        let before = service.status().await.dataplane.unwrap();
        assert_eq!(before.state, DataplaneState::Stopped);
        assert!(!before.up);

        service.start().await.unwrap();
        assert_eq!(service.state(), MeshState::Running);
        assert!(!runner.calls().is_empty());

        let status = service.status().await;
        assert!(status.available);
        let dataplane = status.dataplane.clone().unwrap();
        assert_eq!(dataplane.state, DataplaneState::Running);
        assert!(dataplane.up);
        assert!(dataplane.forwarding);
        assert!(!dataplane.nat);
        assert_eq!(dataplane.interface.as_deref(), Some("ozma0"));
        assert_eq!(dataplane.address.as_deref(), Some("10.200.9.1/24"));
        assert_eq!(dataplane.mtu, Some(1420));
        assert_eq!(dataplane.routes, vec!["10.200.9.2/32".to_owned()]);
        assert!(dataplane.last_error.is_none());

        service
            .add_peer(peer("peer-a", "10.200.9.3"))
            .await
            .unwrap();
        let status = service.status().await;
        assert_eq!(
            status.dataplane.as_ref().unwrap().routes,
            vec!["10.200.9.2/32".to_owned(), "10.200.9.3/32".to_owned()]
        );
        let peer = status.peers.unwrap().remove(0);
        assert_eq!(peer.tunnel, MeshTunnelState::Idle);
        assert!(
            !peer.established,
            "dataplane readiness must not fabricate a handshake"
        );

        service.remove_peer("peer-a").await.unwrap();
        assert_eq!(
            service.status().await.dataplane.unwrap().routes,
            vec!["10.200.9.2/32".to_owned()]
        );

        service.stop().await.unwrap();
        assert_eq!(service.state(), MeshState::Stopped);
        let status = service.status().await;
        assert!(!status.available);
        let dataplane = status.dataplane.unwrap();
        assert_eq!(dataplane.state, DataplaneState::Stopped);
        assert!(!dataplane.up);
        assert!(dataplane.routes.is_empty());
    }

    #[tokio::test]
    async fn dataplane_failed_status_is_typed_and_never_running() {
        let (service, runner) =
            dataplane_service(dataplane_config(), NodeSysfs::rooted().not_root());
        let error = service.start().await.unwrap_err();
        assert_eq!(error.code, "mesh.root-required");
        assert!(runner.calls().is_empty());

        let status = service.status().await;
        assert_eq!(status.state, MeshState::Failed);
        assert!(!status.available);
        assert!(status.peer_count.is_none());
        assert!(status.peers.is_none());
        assert_eq!(status.code.as_deref(), Some("mesh.root-required"));
        let dataplane = status.dataplane.unwrap();
        assert_eq!(dataplane.state, DataplaneState::Failed);
        assert!(!dataplane.up);
        assert!(dataplane.routes.is_empty());
        assert_eq!(
            dataplane
                .last_error
                .as_ref()
                .map(|error| error.code.as_str()),
            Some("mesh.root-required")
        );
    }

    #[tokio::test]
    async fn dataplane_missing_tun_and_nft_are_typed() {
        let (no_tun, no_tun_runner) =
            dataplane_service(dataplane_config(), NodeSysfs::rooted().without_tun());
        let error = no_tun.start().await.unwrap_err();
        assert_eq!(error.code, "mesh.dataplane-unavailable");
        assert!(no_tun_runner.calls().is_empty());
        assert_eq!(no_tun.status().await.state, MeshState::Failed);

        let nat_config = dataplane_config().with_nat(NatConfig::new());
        let (no_nft, no_nft_runner) =
            dataplane_service(nat_config, NodeSysfs::rooted().without_nft());
        let error = no_nft.start().await.unwrap_err();
        assert_eq!(error.code, "mesh.dataplane-unavailable");
        assert!(error.reason.contains("nft"));
        assert!(no_nft_runner.calls().is_empty());
    }

    #[tokio::test]
    async fn dataplane_starting_is_reported_but_never_as_running() {
        let log = MeshEventLog::new(8);
        let backend = Arc::new(FakeMeshBackend::new(
            fake_node("self", "10.200.9.1"),
            log.clone(),
        ));
        let runner = Arc::new(NodeRunner::default());
        let observed = Arc::new(Mutex::new(None));
        let slot: Arc<Mutex<Option<MeshService>>> = Arc::new(Mutex::new(None));
        {
            let observed = observed.clone();
            let slot = slot.clone();
            runner.set_probe(move || {
                if let Some(service) = slot
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .as_ref()
                {
                    *observed.lock().unwrap_or_else(|error| error.into_inner()) =
                        Some(service.state());
                }
            });
        }
        let dataplane = Arc::new(MeshDataplane::new(
            dataplane_config(),
            runner,
            Arc::new(NodeSysfs::rooted()),
        ));
        let service = MeshService::with_backend_and_dataplane(backend, Some(dataplane), log);
        *slot.lock().unwrap_or_else(|error| error.into_inner()) = Some(service.clone());

        service.start().await.unwrap();
        assert_eq!(
            *observed.lock().unwrap_or_else(|error| error.into_inner()),
            Some(MeshState::Starting)
        );
        assert_eq!(service.state(), MeshState::Running);
    }

    #[test]
    fn preflight_reports_dataplane_prerequisites_typed() {
        let config = MeshServiceConfig {
            node_id: "node".into(),
            mesh_ip: "10.200.9.1".into(),
            wg_port: 51_820,
            dataplane: Some(dataplane_config()),
            ..MeshServiceConfig::default()
        };
        let base = MeshEnvironment {
            is_root: true,
            wg_binary: false,
            userspace_wireguard: true,
            tun_available: true,
            ip_binary: true,
            nft_binary: true,
            wireguard_module: true,
        };
        config.preflight_with(true, &base).unwrap();

        let no_root = MeshEnvironment {
            is_root: false,
            ..base
        };
        assert_eq!(
            config.preflight_with(true, &no_root).unwrap_err().code,
            "mesh.root-required"
        );

        let no_tun = MeshEnvironment {
            tun_available: false,
            ..base
        };
        assert_eq!(
            config.preflight_with(true, &no_tun).unwrap_err().code,
            "mesh.dataplane-unavailable"
        );

        let no_ip = MeshEnvironment {
            ip_binary: false,
            ..base
        };
        assert_eq!(
            config.preflight_with(true, &no_ip).unwrap_err().code,
            "mesh.dataplane-unavailable"
        );

        let nat = MeshServiceConfig {
            dataplane: Some(dataplane_config().with_nat(NatConfig::new())),
            ..config.clone()
        };
        let no_nft = MeshEnvironment {
            nft_binary: false,
            ..base
        };
        assert_eq!(
            nat.preflight_with(true, &no_nft).unwrap_err().code,
            "mesh.dataplane-unavailable"
        );
        // NAT is opt-in: nft is irrelevant without a NAT config.
        config.preflight_with(true, &no_nft).unwrap();

        let invalid = MeshServiceConfig {
            dataplane: Some(DataplaneConfig::new(
                "ozma0",
                Ipv4Addr::new(10, 200, 9, 1),
                24,
                0,
            )),
            ..config
        };
        assert_eq!(
            invalid.preflight_with(true, &base).unwrap_err().code,
            "mesh.dataplane-invalid"
        );
    }

    fn dataplane_config() -> DataplaneConfig {
        DataplaneConfig::new("ozma0", Ipv4Addr::new(10, 200, 9, 1), 24, 1420)
            .with_peer_routes(vec![Ipv4Addr::new(10, 200, 9, 2)])
    }

    fn dataplane_service(
        config: DataplaneConfig,
        sysfs: NodeSysfs,
    ) -> (MeshService, Arc<NodeRunner>) {
        let log = MeshEventLog::new(16);
        let backend = Arc::new(FakeMeshBackend::new(
            fake_node("self", "10.200.9.1"),
            log.clone(),
        ));
        let runner = Arc::new(NodeRunner::default());
        let dataplane = Arc::new(MeshDataplane::new(config, runner.clone(), Arc::new(sysfs)));
        (
            MeshService::with_backend_and_dataplane(backend, Some(dataplane), log),
            runner,
        )
    }

    #[derive(Default)]
    struct NodeRunner {
        calls: Mutex<Vec<CommandSpec>>,
        probe: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    }

    impl NodeRunner {
        fn calls(&self) -> Vec<CommandSpec> {
            self.calls
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone()
        }

        fn set_probe(&self, probe: impl Fn() + Send + Sync + 'static) {
            *self.probe.lock().unwrap_or_else(|error| error.into_inner()) = Some(Box::new(probe));
        }
    }

    impl CommandRunner for NodeRunner {
        fn run(
            &self,
            command: &CommandSpec,
        ) -> std::io::Result<ozma_mesh::dataplane::CommandOutput> {
            if let Some(probe) = self
                .probe
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take()
            {
                probe();
            }
            self.calls
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(command.clone());
            Ok(ozma_mesh::dataplane::CommandOutput {
                status: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    }

    struct NodeSysfs {
        root: bool,
        tun: bool,
        ip: bool,
        nft: bool,
        files: Mutex<BTreeMap<std::path::PathBuf, String>>,
    }

    impl NodeSysfs {
        fn rooted() -> Self {
            Self {
                root: true,
                tun: true,
                ip: true,
                nft: true,
                files: Mutex::new(BTreeMap::from([(
                    std::path::PathBuf::from(ozma_mesh::dataplane::IPV4_FORWARD),
                    "0\n".to_owned(),
                )])),
            }
        }

        fn not_root(mut self) -> Self {
            self.root = false;
            self
        }

        fn without_tun(mut self) -> Self {
            self.tun = false;
            self
        }

        fn without_nft(mut self) -> Self {
            self.nft = false;
            self
        }
    }

    impl FileOpener for NodeSysfs {
        fn exists(&self, path: &std::path::Path) -> bool {
            match path.to_str() {
                Some(ozma_mesh::dataplane::TUN_DEVICE) => self.tun,
                _ => self
                    .files
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .contains_key(path),
            }
        }

        fn read_to_string(&self, path: &std::path::Path) -> std::io::Result<String> {
            self.files
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .get(path)
                .cloned()
                .ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::NotFound, "missing fake file")
                })
        }

        fn write(&self, path: &std::path::Path, contents: &str) -> std::io::Result<()> {
            self.files
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .insert(path.to_owned(), contents.to_owned());
            Ok(())
        }
    }

    impl Sysfs for NodeSysfs {
        fn is_root(&self) -> bool {
            self.root
        }

        fn command_available(&self, program: &str) -> bool {
            match program {
                "ip" => self.ip,
                "nft" => self.nft,
                _ => false,
            }
        }
    }

    #[derive(Default)]
    struct FakeTunProvider {
        calls: Mutex<Vec<String>>,
        error: Mutex<Option<PumpError>>,
        probe: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    }

    impl FakeTunProvider {
        fn calls(&self) -> Vec<String> {
            self.calls
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone()
        }

        fn set_error(&self, error: PumpError) {
            *self.error.lock().unwrap_or_else(|value| value.into_inner()) = Some(error);
        }

        fn set_probe(&self, probe: impl Fn() + Send + Sync + 'static) {
            *self.probe.lock().unwrap_or_else(|value| value.into_inner()) = Some(Box::new(probe));
        }
    }

    #[async_trait]
    impl TunProvider for FakeTunProvider {
        async fn open(&self, interface: &str) -> Result<Arc<dyn TunIo>, PumpError> {
            if let Some(probe) = self
                .probe
                .lock()
                .unwrap_or_else(|value| value.into_inner())
                .take()
            {
                probe();
            }
            self.calls
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(interface.to_owned());
            if let Some(error) = self
                .error
                .lock()
                .unwrap_or_else(|value| value.into_inner())
                .clone()
            {
                return Err(error);
            }
            Ok(Arc::new(IdleTun))
        }
    }

    struct IdleTun;

    #[async_trait]
    impl TunIo for IdleTun {
        async fn read_packet(&self, _buffer: &mut [u8]) -> Result<usize, PumpError> {
            std::future::pending().await
        }

        async fn write_packet(&self, packet: &[u8]) -> Result<usize, PumpError> {
            Ok(packet.len())
        }
    }

    struct IdleTunnel;

    #[async_trait]
    impl TunnelIo for IdleTunnel {
        async fn send_packet(&self, _packet: &[u8]) -> Result<(), PumpError> {
            Ok(())
        }

        async fn recv_packet(&self, _buffer: &mut [u8]) -> Result<usize, PumpError> {
            std::future::pending().await
        }
    }

    struct FailingTunnel;

    #[async_trait]
    impl TunnelIo for FailingTunnel {
        async fn send_packet(&self, _packet: &[u8]) -> Result<(), PumpError> {
            Ok(())
        }

        async fn recv_packet(&self, _buffer: &mut [u8]) -> Result<usize, PumpError> {
            Err(PumpError::tunnel_receive("simulated decapsulation failure"))
        }
    }

    fn pump_service(
        sysfs: NodeSysfs,
        provider: Arc<FakeTunProvider>,
        tunnel: Option<Arc<dyn TunnelIo>>,
    ) -> (MeshService, Arc<FakeMeshBackend>, Arc<NodeRunner>) {
        let log = MeshEventLog::new(16);
        let backend = Arc::new(FakeMeshBackend::new(
            fake_node("self", "10.200.9.1"),
            log.clone(),
        ));
        if let Some(tunnel) = tunnel {
            backend.set_tunnel_io(tunnel);
        }
        let runner = Arc::new(NodeRunner::default());
        let dataplane = Arc::new(MeshDataplane::new(
            dataplane_config(),
            runner.clone(),
            Arc::new(sysfs),
        ));
        let service = MeshService::with_backend_dataplane_and_tun(
            backend.clone(),
            Some(dataplane),
            log,
            Some(provider),
        );
        (service, backend, runner)
    }

    #[tokio::test]
    async fn pump_is_absent_without_a_dataplane() {
        let log = MeshEventLog::new(8);
        let backend = Arc::new(FakeMeshBackend::new(
            fake_node("self", "10.200.9.1"),
            log.clone(),
        ));
        backend.set_tunnel_io(Arc::new(IdleTunnel));
        let provider = Arc::new(FakeTunProvider::default());
        let service =
            MeshService::with_backend_dataplane_and_tun(backend, None, log, Some(provider.clone()));
        service.start().await.unwrap();
        assert!(service.pump_status().is_none());
        assert!(service.status().await.pump.is_none());
        assert!(
            provider.calls().is_empty(),
            "no TUN may be opened without a dataplane"
        );
        service.stop().await.unwrap();
    }

    #[tokio::test]
    async fn pump_waits_for_a_running_tunnel_and_dataplane() {
        let provider = Arc::new(FakeTunProvider::default());
        let (service, backend, _runner) = pump_service(
            NodeSysfs::rooted(),
            provider.clone(),
            Some(Arc::new(IdleTunnel)),
        );
        backend.set_start_error(MeshServiceError::new("mesh.start-failed", "bind failed"));

        let error = service.start().await.unwrap_err();
        assert_eq!(error.code, "mesh.start-failed");
        assert!(provider.calls().is_empty());
        let status = service.status().await;
        assert_eq!(status.state, MeshState::Failed);
        let pump = status.pump.unwrap();
        assert_eq!(pump.state, PumpState::Stopped);
        assert_eq!(pump.stats, PumpStats::default());

        let provider = Arc::new(FakeTunProvider::default());
        let (not_root, _backend, _runner) = pump_service(
            NodeSysfs::rooted().not_root(),
            provider.clone(),
            Some(Arc::new(IdleTunnel)),
        );
        let error = not_root.start().await.unwrap_err();
        assert_eq!(error.code, "mesh.root-required");
        assert!(
            provider.calls().is_empty(),
            "pump must not open TUN before the dataplane is running"
        );
    }

    #[tokio::test]
    async fn pump_reports_starting_while_opening_the_tun() {
        let provider = Arc::new(FakeTunProvider::default());
        let (service, _backend, _runner) = pump_service(
            NodeSysfs::rooted(),
            provider.clone(),
            Some(Arc::new(IdleTunnel)),
        );
        let observed = Arc::new(Mutex::new(None));
        let slot: Arc<Mutex<Option<MeshService>>> = Arc::new(Mutex::new(None));
        {
            let observed = observed.clone();
            let slot = slot.clone();
            provider.set_probe(move || {
                if let Some(service) = slot
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .as_ref()
                {
                    *observed.lock().unwrap_or_else(|error| error.into_inner()) =
                        Some(service.state());
                }
            });
        }
        *slot.lock().unwrap_or_else(|error| error.into_inner()) = Some(service.clone());

        service.start().await.unwrap();
        assert_eq!(
            *observed.lock().unwrap_or_else(|error| error.into_inner()),
            Some(MeshState::Starting),
            "the pump is visible as starting while the TUN fd is opened"
        );
        assert_eq!(service.state(), MeshState::Running);
        assert_eq!(
            service.status().await.pump.unwrap().state,
            PumpState::Running
        );
        service.stop().await.unwrap();
    }

    #[tokio::test]
    async fn pump_tun_open_failure_is_typed_and_rolls_back() {
        let provider = Arc::new(FakeTunProvider::default());
        provider.set_error(PumpError::tun_open("cannot attach ozma0"));
        let (service, backend, runner) = pump_service(
            NodeSysfs::rooted(),
            provider.clone(),
            Some(Arc::new(IdleTunnel)),
        );

        let before = service.status().await.pump.unwrap();
        assert_eq!(before.state, PumpState::Stopped);
        assert_eq!(before.stats, PumpStats::default());

        let error = service.start().await.unwrap_err();
        assert_eq!(error.code, "mesh.tun-open");
        assert_eq!(provider.calls(), vec!["ozma0".to_owned()]);
        assert!(!runner.calls().is_empty(), "the dataplane had started");

        let status = service.status().await;
        assert_eq!(status.state, MeshState::Failed);
        assert!(!status.available);
        assert!(status.peer_count.is_none());
        let pump = status.pump.unwrap();
        assert_eq!(pump.state, PumpState::Failed);
        assert_eq!(
            pump.last_error.as_ref().map(|error| error.code.as_str()),
            Some("mesh.tun-open")
        );
        assert_eq!(pump.stats, PumpStats::default(), "no fake packet counts");
        assert_eq!(
            status.dataplane.as_ref().map(|dataplane| dataplane.state),
            Some(DataplaneState::Stopped),
            "start failure tears the dataplane down"
        );
        assert_eq!(backend.state(), MeshState::Stopped);
    }

    #[tokio::test]
    async fn pump_requires_the_tunnel_packet_api() {
        let provider = Arc::new(FakeTunProvider::default());
        let (service, _backend, _runner) =
            pump_service(NodeSysfs::rooted(), provider.clone(), None);

        let error = service.start().await.unwrap_err();
        assert_eq!(error.code, "mesh.pump-unavailable");
        assert!(
            provider.calls().is_empty(),
            "no TUN is opened for a tunnel without a packet API"
        );
        let status = service.status().await;
        assert_eq!(status.state, MeshState::Failed);
        let pump = status.pump.unwrap();
        assert_eq!(pump.state, PumpState::Failed);
        assert_eq!(
            pump.last_error.as_ref().map(|error| error.code.as_str()),
            Some("mesh.pump-unavailable")
        );
        assert_eq!(pump.stats, PumpStats::default());
    }

    #[tokio::test]
    async fn pump_running_status_has_real_zero_counters_and_stops_cleanly() {
        let provider = Arc::new(FakeTunProvider::default());
        let (service, _backend, _runner) = pump_service(
            NodeSysfs::rooted(),
            provider.clone(),
            Some(Arc::new(IdleTunnel)),
        );

        let stopped = service.status().await.pump.unwrap();
        assert_eq!(stopped.state, PumpState::Stopped);
        assert_eq!(stopped.stats, PumpStats::default());

        service.start().await.unwrap();
        assert_eq!(service.state(), MeshState::Running);
        let status = service.status().await;
        assert!(status.available);
        let pump = status.pump.unwrap();
        assert_eq!(pump.state, PumpState::Running);
        assert_eq!(pump.interface.as_deref(), Some("ozma0"));
        assert_eq!(pump.queue_capacity, PumpConfig::default().queue_capacity);
        assert_eq!(
            pump.stats,
            PumpStats::default(),
            "a running pump with no traffic reports real zeroes"
        );
        assert!(service.packet_pump().is_some());
        assert_eq!(provider.calls(), vec!["ozma0".to_owned()]);

        let app = router(Some(service.clone()));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/connect/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = body_json(response).await;
        assert_eq!(body["pump"]["state"], "running");
        assert_eq!(body["pump"]["stats"]["tun_to_tunnel_packets"], 0);
        assert_eq!(body["pump"]["stats"]["tunnel_to_tun_packets"], 0);

        service.stop().await.unwrap();
        assert_eq!(service.state(), MeshState::Stopped);
        let status = service.status().await;
        assert!(!status.available);
        let pump = status.pump.unwrap();
        assert_eq!(pump.state, PumpState::Stopped);
        assert_eq!(pump.stats, PumpStats::default());
    }

    #[tokio::test]
    async fn asynchronous_pump_failure_tears_down_runtime_parts() {
        let provider = Arc::new(FakeTunProvider::default());
        let (service, backend, _runner) =
            pump_service(NodeSysfs::rooted(), provider, Some(Arc::new(FailingTunnel)));
        service.start().await.unwrap();

        let observed = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let dataplane_stopped = service
                    .status()
                    .await
                    .dataplane
                    .as_ref()
                    .is_some_and(|dataplane| dataplane.state == DataplaneState::Stopped);
                if service.state() == MeshState::Failed
                    && backend.state() == MeshState::Stopped
                    && dataplane_stopped
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await;
        if observed.is_err() {
            let status = service.status().await;
            panic!(
                "pump failure did not tear down runtime: state={:?} backend={:?} \
                 dataplane={:?} pump={:?} error={:?}",
                service.state(),
                backend.state(),
                status.dataplane,
                status.pump,
                service.error()
            );
        }

        let status = service.status().await;
        let pump = status.pump.unwrap();
        assert_eq!(pump.state, PumpState::Failed);
        assert_eq!(
            pump.last_error.as_ref().map(|error| error.code.as_str()),
            Some("mesh.pump-tunnel-receive")
        );
        assert_eq!(backend.state(), MeshState::Stopped);
        assert_eq!(
            status.dataplane.as_ref().map(|dataplane| dataplane.state),
            Some(DataplaneState::Stopped)
        );
    }

    fn discovery_service(
        static_peers: Vec<MeshNode>,
        trusted: Vec<ozma_mesh::TrustedPeer>,
        ttl: Duration,
    ) -> (Arc<FakeMeshBackend>, MeshService, Arc<NodeRunner>) {
        let log = MeshEventLog::new(32);
        let backend = Arc::new(FakeMeshBackend::new(
            fake_node("self", "10.200.9.1"),
            log.clone(),
        ));
        let runner = Arc::new(NodeRunner::default());
        let dataplane = Arc::new(MeshDataplane::new(
            DataplaneConfig::new("ozma0", Ipv4Addr::new(10, 200, 9, 1), 24, 1420),
            runner.clone(),
            Arc::new(NodeSysfs::rooted()),
        ));
        let engine =
            DiscoveryEngine::new(DiscoveryConfig::new("self", static_peers, trusted).with_ttl(ttl))
                .expect("discovery engine");
        let service = MeshService::with_backend_dataplane_and_discovery(
            backend.clone(),
            Some(dataplane),
            log,
            None,
            Some(Arc::new(Mutex::new(engine))),
            false,
        );
        (backend, service, runner)
    }

    #[tokio::test]
    async fn discovery_key_rotation_updates_the_peer_and_rehandshakes() {
        let old = peer("peer-a", "10.200.9.2");
        let (backend, service, _runner) = discovery_service(
            Vec::new(),
            vec![ozma_mesh::TrustedPeer::new(&old.id, &old.wg_pubkey.0)],
            Duration::from_secs(30),
        );
        service.start().await.unwrap();
        let actions = service
            .apply_discovery_advertisement(advertisement(&old, &["video"]))
            .await;
        assert!(
            matches!(actions.as_slice(), [DiscoveryAction::Add(_)]),
            "the initial advertisement is added: {actions:?}"
        );

        let mut rotated = peer("peer-a", "10.200.9.2");
        rotated.wg_pubkey = WgPrivateKey::generate().public_key();
        let mut raw = advertisement(&rotated, &["video"]);
        raw.previous_pubkeys = vec![old.wg_pubkey.0.clone()];
        let actions = service.apply_discovery_advertisement(raw).await;
        assert!(
            matches!(actions.as_slice(), [DiscoveryAction::KeyRotated { .. }]),
            "a grace advertisement rotates the peer: {actions:?}"
        );
        let peers = service.peers().await;
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].wg_pubkey, rotated.wg_pubkey.0);
        assert!(
            backend
                .handshakes()
                .iter()
                .filter(|id| id.as_str() == "peer-a")
                .count()
                >= 2,
            "the rotated tunnel gets a fresh handshake"
        );
    }

    fn advertisement(peer: &MeshNode, capabilities: &[&str]) -> RawAdvertisement {
        RawAdvertisement {
            id: Some(peer.id.clone()),
            wg_pubkey: Some(peer.wg_pubkey.0.clone()),
            mesh_ip: Some(peer.mesh_ip.clone()),
            wg_port: Some(peer.wg_port),
            capabilities: capabilities.iter().map(|value| value.to_string()).collect(),
            ..RawAdvertisement::default()
        }
    }

    fn signed_discovery_service(
        signer: &ClusterSigningKey,
        trusted: Vec<ozma_mesh::TrustedPeer>,
    ) -> (Arc<FakeMeshBackend>, MeshService, Arc<NodeRunner>) {
        let log = MeshEventLog::new(32);
        let backend = Arc::new(FakeMeshBackend::new(
            fake_node("self", "10.200.9.1"),
            log.clone(),
        ));
        let runner = Arc::new(NodeRunner::default());
        let dataplane = Arc::new(MeshDataplane::new(
            DataplaneConfig::new("ozma0", Ipv4Addr::new(10, 200, 9, 1), 24, 1420),
            runner.clone(),
            Arc::new(NodeSysfs::rooted()),
        ));
        let engine = DiscoveryEngine::new(
            DiscoveryConfig::new("self", Vec::new(), trusted)
                .with_ttl(Duration::from_secs(30))
                .with_auth(signer.verifier(Duration::from_secs(30)).unwrap()),
        )
        .expect("signed discovery engine");
        let service = MeshService::with_backend_dataplane_and_discovery(
            backend.clone(),
            Some(dataplane),
            log,
            None,
            Some(Arc::new(Mutex::new(engine))),
            false,
        );
        (backend, service, runner)
    }

    fn signed_advertisement(
        signer: &ClusterSigningKey,
        peer: &MeshNode,
        capabilities: &[&str],
    ) -> RawAdvertisement {
        let capabilities: Vec<String> =
            capabilities.iter().map(|value| value.to_string()).collect();
        let auth = signer.sign_advertisement(
            &peer.id,
            &peer.mesh_ip,
            &peer.wg_pubkey.0,
            peer.wg_port,
            &[],
            &capabilities,
            SystemTime::now(),
        );
        RawAdvertisement {
            id: Some(peer.id.clone()),
            wg_pubkey: Some(peer.wg_pubkey.0.clone()),
            mesh_ip: Some(peer.mesh_ip.clone()),
            wg_port: Some(peer.wg_port),
            capabilities,
            cluster_key_id: Some(auth.key_id),
            cluster_signature: Some(auth.signature),
            timestamp: Some(auth.timestamp),
            nonce: Some(auth.nonce),
            ..RawAdvertisement::default()
        }
    }

    fn route_command(runner: &NodeRunner, verb: &str, route: &str) -> bool {
        runner.calls().iter().any(|command| {
            command.program == "ip"
                && command.args.iter().any(|arg| arg == verb)
                && command.args.iter().any(|arg| arg == route)
        })
    }

    #[tokio::test]
    async fn discovery_authorizes_then_adds_peer_route_and_handshake() {
        let authorized = peer("peer-a", "10.200.9.2");
        let (backend, service, runner) = discovery_service(
            Vec::new(),
            vec![ozma_mesh::TrustedPeer::new(
                &authorized.id,
                &authorized.wg_pubkey.0,
            )],
            Duration::from_secs(30),
        );
        service.start().await.unwrap();

        let unauthorized = peer("peer-b", "10.200.9.3");
        let actions = service
            .apply_discovery_advertisement(advertisement(&unauthorized, &["video"]))
            .await;
        assert!(matches!(
            actions.as_slice(),
            [DiscoveryAction::Unauthorized { .. }]
        ));
        assert!(
            service.peers().await.is_empty(),
            "an unauthenticated advertisement must not become a peer"
        );
        assert!(
            backend.handshakes().is_empty(),
            "no handshake may start before key authorization"
        );
        assert!(
            !route_command(&runner, "replace", "10.200.9.3/32"),
            "no route may be programmed for an unauthorized peer"
        );

        let mut authorized_advertisement = advertisement(&authorized, &["video", "audio"]);
        authorized_advertisement.underlay_ip = Some(Ipv4Addr::new(10, 10, 10, 2));
        let actions = service
            .apply_discovery_advertisement(authorized_advertisement)
            .await;
        assert!(matches!(actions.as_slice(), [DiscoveryAction::Add(_)]));
        let peers = service.peers().await;
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].origin, PeerOrigin::Discovered);
        assert_eq!(peers[0].capabilities, vec!["audio", "video"]);
        assert_eq!(
            peers[0].endpoint.as_deref(),
            Some("10.10.10.2:59200"),
            "the observed underlay address, not the overlay mesh IP, is the WireGuard endpoint"
        );
        assert_eq!(backend.handshakes(), vec!["peer-a".to_owned()]);
        assert!(route_command(&runner, "replace", "10.200.9.2/32"));

        let status = service.status().await;
        let discovery = status.discovery.expect("discovery status");
        assert!(discovery.enabled);
        assert_eq!(discovery.discovered_count, 1);
        assert_eq!(discovery.counters.added, 1);
        assert_eq!(discovery.counters.unauthorized, 1);
        assert_eq!(discovery.peers[0].id, "peer-a");
        assert_eq!(
            status.dataplane.unwrap().routes,
            vec!["10.200.9.2/32".to_owned()]
        );

        let kinds: Vec<_> = service
            .events(32)
            .into_iter()
            .map(|event| event.kind)
            .collect();
        assert!(kinds.contains(&MeshEventKind::PeerDiscovered));
        assert!(kinds.contains(&MeshEventKind::Route));
        assert!(kinds.contains(&MeshEventKind::Conflict));
    }

    #[tokio::test]
    async fn signed_discovery_accepts_only_valid_cluster_signatures() {
        let signer = ClusterSigningKey::from_seed([7u8; 32]);
        let other = ClusterSigningKey::generate();
        let peer = peer("peer-a", "10.200.9.2");
        let (backend, service, runner) = signed_discovery_service(&signer, Vec::new());
        service.start().await.unwrap();

        // Unsigned advertisement: the cluster key is configured, so the old
        // trust fallback is not silently used.
        let actions = service
            .apply_discovery_advertisement(advertisement(&peer, &["video"]))
            .await;
        assert!(matches!(
            actions.as_slice(),
            [DiscoveryAction::SignatureInvalid { .. }]
        ));
        assert!(service.peers().await.is_empty());
        assert!(backend.handshakes().is_empty());

        // Signed, but by a different cluster key.
        let actions = service
            .apply_discovery_advertisement(signed_advertisement(&other, &peer, &["video"]))
            .await;
        assert!(matches!(
            actions.as_slice(),
            [DiscoveryAction::SignatureInvalid { .. }]
        ));

        // Signed and valid: no --mesh-trust entry is required.
        let mut valid = signed_advertisement(&signer, &peer, &["video"]);
        valid.underlay_ip = Some(Ipv4Addr::new(10, 10, 10, 2));
        let actions = service.apply_discovery_advertisement(valid).await;
        assert!(matches!(actions.as_slice(), [DiscoveryAction::Add(_)]));
        assert_eq!(backend.handshakes(), vec!["peer-a".to_owned()]);
        assert!(route_command(&runner, "replace", "10.200.9.2/32"));

        let status = service.status().await;
        let discovery = status.discovery.expect("discovery status");
        assert_eq!(discovery.counters.signature_invalid, 2);
        assert_eq!(discovery.counters.added, 1);
        assert_eq!(
            discovery.auth.as_ref().map(|auth| auth.key_id.as_str()),
            Some(signer.key_id())
        );

        // Tampering the advertised overlay address invalidates the signature.
        let mut tampered = signed_advertisement(&signer, &peer, &["video"]);
        tampered.mesh_ip = Some("10.200.9.99".to_owned());
        let actions = service.apply_discovery_advertisement(tampered).await;
        assert!(matches!(
            actions.as_slice(),
            [DiscoveryAction::SignatureInvalid { .. }]
        ));

        let events = service.events(32);
        assert!(events
            .iter()
            .any(|event| event.message.contains("mesh.discovery-signature-invalid")));
        use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
        let all_text = format!("{events:?}{discovery:?}");
        let private = B64.encode([7u8; 32]);
        assert!(
            !all_text.contains(&private),
            "status/events must not contain private cluster material"
        );
    }

    #[tokio::test]
    async fn signed_discovery_stale_timestamp_is_typed() {
        let signer = ClusterSigningKey::generate();
        let peer = peer("peer-a", "10.200.9.2");
        let (_backend, service, _runner) = signed_discovery_service(&signer, Vec::new());
        service.start().await.unwrap();

        let mut stale = signed_advertisement(&signer, &peer, &["video"]);
        // Keep the signature valid but move the timestamp outside the 30s
        // freshness window by re-signing the old wall clock.
        let old = SystemTime::now() - Duration::from_secs(120);
        let auth = signer.sign_advertisement(
            &peer.id,
            &peer.mesh_ip,
            &peer.wg_pubkey.0,
            peer.wg_port,
            &[],
            &["video".to_owned()],
            old,
        );
        stale.cluster_key_id = Some(auth.key_id);
        stale.cluster_signature = Some(auth.signature);
        stale.timestamp = Some(auth.timestamp);
        stale.nonce = Some(auth.nonce);
        let actions = service.apply_discovery_advertisement(stale).await;
        assert!(matches!(
            actions.as_slice(),
            [DiscoveryAction::Stale { .. }]
        ));
        assert!(service
            .events(32)
            .iter()
            .any(|event| event.message.contains("mesh.discovery-stale")));
    }

    #[tokio::test]
    async fn discovery_expiry_removes_the_tunnel_and_route() {
        let peer = peer("peer-a", "10.200.9.2");
        let (_backend, service, runner) = discovery_service(
            Vec::new(),
            vec![ozma_mesh::TrustedPeer::new(&peer.id, &peer.wg_pubkey.0)],
            Duration::from_millis(30),
        );
        service.start().await.unwrap();
        service
            .apply_discovery_advertisement(advertisement(&peer, &["video"]))
            .await;
        assert_eq!(service.peers().await.len(), 1);

        tokio::time::sleep(Duration::from_millis(50)).await;
        let actions = service.expire_discovery().await;
        assert!(matches!(actions.as_slice(), [DiscoveryAction::Remove(_)]));
        assert!(service.peers().await.is_empty());
        assert!(route_command(&runner, "del", "10.200.9.2/32"));
        assert_eq!(service.status().await.peer_count, Some(0));
        assert_eq!(
            service.status().await.discovery.unwrap().counters.expired,
            1
        );
        assert!(service
            .events(32)
            .iter()
            .any(|event| event.kind == MeshEventKind::PeerRemoved));
    }

    #[tokio::test]
    async fn discovery_conflict_never_overwrites_a_static_peer() {
        let mut static_peer = peer("static", "10.200.9.2");
        static_peer.wg_port = 51_820;
        let (_backend, service, runner) = discovery_service(
            vec![static_peer.clone()],
            vec![ozma_mesh::TrustedPeer::new(
                &static_peer.id,
                &static_peer.wg_pubkey.0,
            )],
            Duration::from_secs(30),
        );
        service.start().await.unwrap();

        // Exact static advertisement is observed but not duplicated.
        let actions = service
            .apply_discovery_advertisement(advertisement(&static_peer, &[]))
            .await;
        assert!(matches!(
            actions.as_slice(),
            [DiscoveryAction::StaticMatched { .. }]
        ));
        assert!(service.peers().await.is_empty());

        let mut conflict = static_peer.clone();
        conflict.mesh_ip = "10.200.9.99".to_owned();
        let actions = service
            .apply_discovery_advertisement(advertisement(&conflict, &[]))
            .await;
        assert!(matches!(
            actions.as_slice(),
            [DiscoveryAction::Conflict { .. }]
        ));
        assert!(service.peers().await.is_empty());
        assert!(
            !route_command(&runner, "replace", "10.200.9.99/32"),
            "a conflict must not program a discovered route"
        );
        let discovery = service.status().await.discovery.unwrap();
        assert_eq!(discovery.counters.conflicts, 1);
        assert_eq!(discovery.counters.static_matched, 1);
    }

    #[tokio::test]
    async fn discovery_mesh_ip_change_reconciles_one_peer_and_two_routes() {
        let peer = peer("peer-a", "10.200.9.2");
        let (backend, service, runner) = discovery_service(
            Vec::new(),
            vec![ozma_mesh::TrustedPeer::new(&peer.id, &peer.wg_pubkey.0)],
            Duration::from_secs(30),
        );
        service.start().await.unwrap();
        service
            .apply_discovery_advertisement(advertisement(&peer, &["video"]))
            .await;

        let mut moved = peer.clone();
        moved.mesh_ip = "10.200.9.3".to_owned();
        let actions = service
            .apply_discovery_advertisement(advertisement(&moved, &["video"]))
            .await;
        assert!(matches!(
            actions.as_slice(),
            [DiscoveryAction::Update { .. }]
        ));
        let peers = service.peers().await;
        assert_eq!(peers.len(), 1, "one id must own one tunnel");
        assert_eq!(peers[0].mesh_ip, "10.200.9.3");
        assert!(route_command(&runner, "replace", "10.200.9.3/32"));
        assert!(route_command(&runner, "del", "10.200.9.2/32"));
        assert_eq!(
            backend.handshakes(),
            vec!["peer-a".to_owned(), "peer-a".to_owned()]
        );
    }

    #[tokio::test]
    async fn discovery_survives_restart_without_stale_deduplication() {
        let peer = peer("peer-a", "10.200.9.2");
        let (_backend, service, _runner) = discovery_service(
            Vec::new(),
            vec![ozma_mesh::TrustedPeer::new(&peer.id, &peer.wg_pubkey.0)],
            Duration::from_secs(30),
        );
        service.start().await.unwrap();
        service
            .apply_discovery_advertisement(advertisement(&peer, &[]))
            .await;
        service.stop().await.unwrap();
        assert_eq!(
            service.status().await.discovery.unwrap().discovered_count,
            0
        );

        service.start().await.unwrap();
        let actions = service
            .apply_discovery_advertisement(advertisement(&peer, &[]))
            .await;
        assert!(
            matches!(actions.as_slice(), [DiscoveryAction::Add(_)]),
            "a restarted service must re-add the peer, not deduplicate a stale entry"
        );
        assert_eq!(service.peers().await.len(), 1);
    }

    #[tokio::test]
    async fn rotation_updates_identity_and_logs_without_private_material() {
        let (_backend, service) = fake_service(16);
        service.start().await.unwrap();
        service
            .add_peer(peer("peer-a", "10.200.9.2"))
            .await
            .unwrap();
        let before = service.identity().unwrap().wg_pubkey;

        let status = service.rotate(Duration::from_secs(30)).await.unwrap();
        assert_eq!(status.key_id, 2);
        assert_ne!(status.public_key, before);
        assert_eq!(service.identity().unwrap().wg_pubkey, status.public_key);
        assert_eq!(service.rotation_status().unwrap().key_id, 2);
        assert!(service
            .events(16)
            .iter()
            .any(|event| event.kind == MeshEventKind::Rotation));

        let document = serde_json::to_string(&service.status().await).unwrap();
        let events = serde_json::to_string(&service.events(16)).unwrap();
        for text in [&document, &events] {
            assert!(!text.contains("private_key"));
            assert!(!text.contains("private-key"));
            assert!(!text.contains("secret"));
        }
        assert!(document.contains(&status.fingerprint));
    }

    #[tokio::test]
    async fn rotation_failure_keeps_the_old_identity() {
        let (backend, service) = fake_service(16);
        service.start().await.unwrap();
        let before = service.identity().unwrap().wg_pubkey;
        backend.set_rotation_failure(MeshServiceError::new(
            "mesh.rotation-failed",
            "peer peer-a could not be updated",
        ));

        let error = service
            .rotate(Duration::from_secs(30))
            .await
            .expect_err("the injected peer failure rejects rotation");
        assert_eq!(error.code, "mesh.rotation-failed");
        assert_eq!(service.identity().unwrap().wg_pubkey, before);
        assert_eq!(service.rotation_status().unwrap().key_id, 1);
        assert!(service
            .events(16)
            .iter()
            .all(|event| event.kind != MeshEventKind::Rotation));
    }

    #[tokio::test]
    async fn rotation_route_requires_confirmation_and_reports_status() {
        let (_backend, service) = fake_service(8);
        service.start().await.unwrap();
        let app = router(Some(service));

        let unconfirmed = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/connect/rotate")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"confirm":false}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unconfirmed.status(), StatusCode::BAD_REQUEST);
        let body = body_json(unconfirmed).await;
        assert_eq!(body["code"], "mesh.rotation-unconfirmed");

        let invalid = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/connect/rotate")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"confirm":true,"grace_secs":1}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_json(invalid).await["code"], "mesh.rotation-invalid");

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/connect/rotate")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"confirm":true,"grace_secs":30}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body["ok"], true);
        assert_eq!(body["rotation"]["key_id"], 2);
        assert_eq!(body["rotation"]["grace_secs"], 30);
        assert!(body["rotation"].get("public_key").is_some());
        assert!(body["rotation"].get("fingerprint").is_some());
        assert!(body.get("private_key").is_none());
    }

    #[tokio::test]
    async fn rotation_route_is_conflict_while_stopped() {
        let (_backend, service) = fake_service(8);
        let app = router(Some(service));
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/connect/rotate")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"confirm":true}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(body_json(response).await["code"], "mesh.not-running");
    }

    #[tokio::test]
    async fn scheduled_rotation_fires_and_reports_next_time() {
        let (_backend, mut service) = fake_service(16);
        service.rotation_schedule = Some(
            RotationSchedule::new(1)
                .with_jitter_secs(0)
                .with_grace_secs(30),
        );
        service.rotation_interval_override = Some(Duration::from_millis(15));
        service.start().await.unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;

        let status = service
            .rotation_status()
            .expect("fake backend reports rotation status");
        assert!(
            status.key_id >= 2,
            "the scheduled task must rotate at least once, got key_id {}",
            status.key_id
        );
        assert!(status.scheduled.is_some());
        assert!(status
            .scheduled
            .as_ref()
            .unwrap()
            .next_rotation_unix_ms
            .is_some());
        service.stop().await.unwrap();
        assert!(service.rotation_status().is_some());
    }

    async fn body_json(response: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }
}
