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
//! The real backend uses the existing userspace boringtun implementation in
//! [`ozma_mesh::MeshManager`]. Kernel WireGuard is not implemented; requesting
//! it is an explicit typed unsupported state even when root and `wg` exist.

use std::collections::{BTreeMap, VecDeque};
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use ozma_mesh::{MeshManager, MeshNode, WgPrivateKey};
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
                | "mesh.kernel-unsupported"
                | "mesh.not-configured"
                | "mesh.ip-invalid"
                | "mesh.peer-invalid"
        )
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
    /// `userspace-boringtun` or `kernel` (not implemented).
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
    TunnelState,
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
    /// Configured peers.
    pub peers: Vec<MeshNode>,
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
            peers: Vec::new(),
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
    async fn peers(&self) -> Vec<MeshPeerView>;
    fn identity(&self) -> Option<MeshIdentity>;
    fn state(&self) -> MeshState;
    fn error(&self) -> Option<MeshServiceError>;
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
    identity: MeshNode,
    private_key: WgPrivateKey,
    configured_peers: Vec<MeshNode>,
    /// `MeshManager` is `Send` but not `Sync` (libmdns uses an internal
    /// `RefCell`), so it lives behind a Tokio mutex instead of an `Arc`.
    manager: tokio::sync::Mutex<Option<MeshManager>>,
    state: Mutex<MeshState>,
    error: Mutex<Option<MeshServiceError>>,
    peer_states: Mutex<BTreeMap<String, MeshTunnelState>>,
    log: MeshEventLog,
}

impl BoringtunMeshBackend {
    pub fn new(
        identity: MeshNode,
        private_key: WgPrivateKey,
        configured_peers: Vec<MeshNode>,
        log: MeshEventLog,
    ) -> Self {
        Self {
            identity,
            private_key,
            configured_peers,
            manager: tokio::sync::Mutex::new(None),
            state: Mutex::new(MeshState::Stopped),
            error: Mutex::new(None),
            peer_states: Mutex::new(BTreeMap::new()),
            log,
        }
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

        let mut manager = self.manager.lock().await;
        if manager.is_none() {
            match MeshManager::new(self.identity.clone(), self.private_key.clone()).await {
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

        self.set_state(MeshState::Running);
        self.set_error(None);
        Ok(())
    }

    async fn stop(&self) -> Result<(), MeshServiceError> {
        let manager = self.manager.lock().await.take();
        if let Some(manager) = manager {
            manager.stop();
        }
        self.set_state(MeshState::Stopped);
        self.set_error(None);
        Ok(())
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
        Some(MeshIdentity {
            id: self.identity.id.clone(),
            mesh_ip: self.identity.mesh_ip.clone(),
            wg_port: self.identity.wg_port,
            wg_pubkey: self.identity.wg_pubkey.0.clone(),
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
}

/// In-memory backend used by tests. It never binds a socket, touches root, or
/// invents a handshake.
pub struct FakeMeshBackend {
    identity: MeshNode,
    state: Mutex<MeshState>,
    peers: Mutex<BTreeMap<String, MeshPeerView>>,
    error: Mutex<Option<MeshServiceError>>,
    start_error: Mutex<Option<MeshServiceError>>,
    log: MeshEventLog,
}

impl FakeMeshBackend {
    pub fn new(identity: MeshNode, log: MeshEventLog) -> Self {
        Self {
            identity,
            state: Mutex::new(MeshState::Stopped),
            peers: Mutex::new(BTreeMap::new()),
            error: Mutex::new(None),
            start_error: Mutex::new(None),
            log,
        }
    }

    /// Force the next `start()` to fail with a typed runtime error.
    pub fn set_start_error(&self, error: MeshServiceError) {
        *self
            .start_error
            .lock()
            .unwrap_or_else(|value| value.into_inner()) = Some(error);
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
                mesh_ip: node.mesh_ip,
                wg_port: node.wg_port,
                wg_pubkey: node.wg_pubkey.0,
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
        Some(MeshIdentity {
            id: self.identity.id.clone(),
            mesh_ip: self.identity.mesh_ip.clone(),
            wg_port: self.identity.wg_port,
            wg_pubkey: self.identity.wg_pubkey.0.clone(),
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
}

/// Bounded, cloneable node-side mesh service.
#[derive(Clone)]
pub struct MeshService {
    backend: Arc<dyn MeshBackend>,
    log: MeshEventLog,
}

impl MeshService {
    pub fn new(backend: Arc<dyn MeshBackend>, log: MeshEventLog) -> Self {
        Self { backend, log }
    }

    /// Build a service from configuration. Preflight/prerequisite failures
    /// produce an explicit unsupported service rather than an error the node
    /// would have to fabricate around.
    pub fn from_config(config: MeshServiceConfig, environment: &MeshEnvironment) -> Self {
        let capacity = config
            .event_capacity
            .unwrap_or(MESH_EVENT_CAPACITY)
            .clamp(1, MESH_EVENT_CAPACITY);
        let log = MeshEventLog::new(capacity);

        if let Err(error) = config.preflight(environment) {
            return Self::unsupported(error, log);
        }

        let (private_key, identity) = match load_identity(&config) {
            Ok(value) => value,
            Err(error) => return Self::unsupported(error, log),
        };

        let backend =
            BoringtunMeshBackend::new(identity, private_key, config.peers.clone(), log.clone());
        Self {
            backend: Arc::new(backend),
            log,
        }
    }

    /// Explicit unsupported service for a preflight failure.
    pub fn unsupported(error: MeshServiceError, log: MeshEventLog) -> Self {
        Self {
            backend: Arc::new(UnsupportedMeshBackend {
                error,
                identity: None,
            }),
            log,
        }
    }

    /// Build a service with a caller-supplied backend (tests/embedding).
    pub fn with_backend(backend: Arc<dyn MeshBackend>, log: MeshEventLog) -> Self {
        Self { backend, log }
    }

    pub fn log(&self) -> MeshEventLog {
        self.log.clone()
    }

    pub fn state(&self) -> MeshState {
        self.backend.state()
    }

    pub fn error(&self) -> Option<MeshServiceError> {
        self.backend.error()
    }

    pub fn identity(&self) -> Option<MeshIdentity> {
        self.backend.identity()
    }

    pub async fn start(&self) -> Result<(), MeshServiceError> {
        let result = self.backend.start().await;
        match &result {
            Ok(()) => {
                self.log
                    .record(MeshEventKind::Started, "mesh service started", None);
            }
            Err(error) => {
                self.log
                    .record(MeshEventKind::Error, error.reason.clone(), None);
            }
        }
        result
    }

    pub async fn stop(&self) -> Result<(), MeshServiceError> {
        let result = self.backend.stop().await;
        match &result {
            Ok(()) => {
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
        let result = self.backend.add_peer(node).await;
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
        let result = self.backend.remove_peer(peer_id).await;
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

    pub async fn status(&self) -> MeshStatus {
        let state = self.backend.state();
        let error = self.backend.error();
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
        };
        let error = kernel.preflight_with(true, &root_env).unwrap_err();
        assert_eq!(error.code, "mesh.wg-missing");

        let complete = MeshEnvironment {
            is_root: true,
            wg_binary: true,
            userspace_wireguard: true,
        };
        kernel.preflight_with(true, &complete).unwrap();
    }

    #[test]
    fn preflight_rejects_empty_and_invalid_configuration() {
        let env = MeshEnvironment {
            is_root: true,
            wg_binary: true,
            userspace_wireguard: true,
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
        assert_eq!(body["capabilities"].as_array().unwrap().len(), 3);

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

    async fn body_json(response: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }
}
