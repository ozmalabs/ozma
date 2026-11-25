//! Kernel WireGuard backend.
//!
//! This module programs the Linux kernel `wireguard` link directly. It is
//! deliberately separate from [`crate::dataplane::MeshDataplane`]: kernel
//! WireGuard owns its own network interface, so no TUN device or userspace
//! boringtun tunnel is involved.
//!
//! Every host interaction goes through the existing dataplane seams:
//!
//! * [`CommandRunner`] — exact argv, no shell, for `ip` and `wg`;
//! * [`Sysfs`] / [`FileOpener`] — root/tool/module detection and a short-lived
//!   mode-0600 private-key file.
//!
//! The private key is never placed in an argv, a log line, or a status
//! document. `wg set` reads it from the temporary file, which is removed
//! immediately after the command returns. `wg show <if> dump` embeds a private
//! key in its first field; [`parse_wg_dump`] discards it and command errors are
//! scrubbed with [`redact_keys`].
//!
//! A missing root, kernel module, `ip`, or `wg` binary is a typed
//! `mesh.root-required` / `mesh.dataplane-unavailable` error. A peer is only
//! reported established after the kernel reports a non-zero latest handshake;
//! configuration alone never claims a tunnel.

use std::collections::BTreeMap;
use std::fmt;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::dataplane::{
    command_failure, is_already_absent, short_reason, valid_interface_name, valid_mesh_address,
    CommandOutput, CommandRunner, CommandSpec, DataplaneError, DataplaneState, Sysfs, MAX_MTU,
    MIN_MTU,
};
use crate::discovery::{normalize_capabilities, validate_peer_node};
use crate::manager::{PeerChange, PeerOrigin, PeerStatus};
use crate::node::{MeshNode, WgPrivateKey};

/// Default kernel WireGuard interface name.
pub const DEFAULT_KERNEL_INTERFACE: &str = "ozma0";
/// Default kernel WireGuard MTU (leaves room for the WireGuard header).
pub const DEFAULT_KERNEL_MTU: u16 = 1420;
/// Persistent keepalive used for every peer, matching the userspace backend.
pub const DEFAULT_KERNEL_KEEPALIVE: u16 = 25;
/// Sysfs path that proves the kernel module is loaded.
pub const WIREGUARD_MODULE_PATH: &str = "/sys/module/wireguard";
/// A handshake older than this is no longer reported as established.
pub const HANDSHAKE_REJECT_AFTER_SECS: u64 = 180;

const MAX_REASON_BYTES: usize = 512;
static SECRET_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Kernel link configuration. Contains no key material.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KernelWgSettings {
    /// WireGuard link name (Linux limits this to 15 characters).
    pub interface: String,
    /// Local mesh IPv4 address.
    pub address: Ipv4Addr,
    /// Mesh prefix length (`1..=32`).
    pub prefix_len: u8,
    /// Link MTU.
    pub mtu: u16,
    /// WireGuard UDP listen port.
    pub listen_port: u16,
    /// Persistent keepalive in seconds (`0` disables it).
    pub persistent_keepalive: u16,
}

impl KernelWgSettings {
    pub fn new(
        interface: impl Into<String>,
        address: Ipv4Addr,
        prefix_len: u8,
        mtu: u16,
        listen_port: u16,
    ) -> Self {
        Self {
            interface: interface.into(),
            address,
            prefix_len,
            mtu,
            listen_port,
            persistent_keepalive: DEFAULT_KERNEL_KEEPALIVE,
        }
    }

    pub fn with_persistent_keepalive(mut self, seconds: u16) -> Self {
        self.persistent_keepalive = seconds;
        self
    }

    /// Local address in CIDR form (`10.200.9.1/24`).
    pub fn address_cidr(&self) -> String {
        format!("{}/{}", self.address, self.prefix_len)
    }

    /// Reject anything that would produce an ambiguous or dangerous argv.
    pub fn validate(&self) -> Result<(), DataplaneError> {
        if !valid_interface_name(&self.interface) {
            return Err(DataplaneError::invalid(format!(
                "kernel WireGuard interface name {:?} is invalid (1-15 chars, \
                 letters/digits/._- only)",
                self.interface
            )));
        }
        if !valid_mesh_address(self.address) {
            return Err(DataplaneError::invalid(format!(
                "kernel WireGuard address {} is not a usable unicast IPv4 address",
                self.address
            )));
        }
        if !(1..=32).contains(&self.prefix_len) {
            return Err(DataplaneError::invalid(format!(
                "kernel WireGuard prefix length {} is invalid; expected 1..=32",
                self.prefix_len
            )));
        }
        if !(MIN_MTU..=MAX_MTU).contains(&self.mtu) {
            return Err(DataplaneError::invalid(format!(
                "kernel WireGuard MTU {} is invalid; expected {MIN_MTU}..={MAX_MTU}",
                self.mtu
            )));
        }
        if self.listen_port == 0 {
            return Err(DataplaneError::invalid(
                "kernel WireGuard listen port must be non-zero",
            ));
        }
        Ok(())
    }
}

/// One peer as reported by `wg show <if> dump`.
///
/// The private key, preshared key, and any other secret columns are discarded
/// during parsing and never stored.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WgDumpPeer {
    pub public_key: String,
    pub endpoint: Option<String>,
    pub allowed_ips: Vec<String>,
    pub last_handshake_age_secs: Option<u64>,
    pub tx_bytes: u64,
    pub rx_bytes: u64,
    pub persistent_keepalive: Option<u16>,
}

/// Parsed, secret-free `wg show <if> dump`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WgDump {
    pub public_key: String,
    pub listen_port: u16,
    pub peers: Vec<WgDumpPeer>,
}

/// Parse `wg show <interface> dump` at `now_unix_secs`.
///
/// The interface line is
/// `<private-key> <public-key> <listen-port> <fwmark>` and each peer line is
/// `<public-key> <preshared-key> <endpoint> <allowed-ips> <latest-handshake>
/// <rx> <tx> <persistent-keepalive>`. The private and preshared key fields are
/// intentionally not retained.
pub fn parse_wg_dump(output: &str, now_unix_secs: u64) -> Result<WgDump, DataplaneError> {
    let mut lines = output.lines().filter(|line| !line.trim().is_empty());
    let interface = lines
        .next()
        .ok_or_else(|| DataplaneError::command_failed("wg show dump produced no interface line"))?;
    let fields: Vec<&str> = interface.split('\t').collect();
    if fields.len() < 3 {
        return Err(DataplaneError::command_failed(
            "wg show dump interface line is malformed",
        ));
    }
    let public_key = fields[1].to_owned();
    let listen_port = fields[2].parse::<u16>().map_err(|_| {
        DataplaneError::command_failed("wg show dump listen-port field is not a u16")
    })?;

    let mut peers = Vec::new();
    for line in lines {
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() < 8 {
            return Err(DataplaneError::command_failed(
                "wg show dump peer line is malformed",
            ));
        }
        let latest_handshake = fields[4].parse::<u64>().unwrap_or(0);
        let last_handshake_age_secs = if latest_handshake == 0 {
            None
        } else {
            Some(now_unix_secs.saturating_sub(latest_handshake))
        };
        let allowed_ips = if fields[3] == "(none)" || fields[3].trim().is_empty() {
            Vec::new()
        } else {
            fields[3]
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .collect()
        };
        let endpoint = if fields[2] == "(none)" || fields[2].trim().is_empty() {
            None
        } else {
            Some(fields[2].to_owned())
        };
        let persistent_keepalive = if fields[7] == "off" || fields[7].trim().is_empty() {
            None
        } else {
            fields[7].parse::<u16>().ok()
        };
        peers.push(WgDumpPeer {
            public_key: fields[0].to_owned(),
            endpoint,
            allowed_ips,
            last_handshake_age_secs,
            tx_bytes: fields[6].parse::<u64>().unwrap_or(0),
            rx_bytes: fields[5].parse::<u64>().unwrap_or(0),
            persistent_keepalive,
        });
    }
    Ok(WgDump {
        public_key,
        listen_port,
        peers,
    })
}

/// Honest kernel WireGuard status.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KernelWgStatus {
    pub configured: bool,
    pub state: DataplaneState,
    pub interface: String,
    /// Interface public key only; the private key is never included.
    pub public_key: String,
    pub listen_port: u16,
    pub address: String,
    pub mtu: u16,
    pub up: bool,
    /// Peer `/32` routes currently programmed by this backend.
    pub routes: Vec<String>,
    pub peers: Vec<WgDumpPeer>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<DataplaneError>,
}

#[derive(Clone)]
struct PeerRecord {
    node: MeshNode,
    endpoint: SocketAddr,
    origin: PeerOrigin,
    capabilities: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Applied {
    Link(String),
    Peer(String),
    Route(Ipv4Addr),
}

struct Inner {
    state: DataplaneState,
    applied: Vec<Applied>,
    error: Option<DataplaneError>,
    peers: BTreeMap<String, PeerRecord>,
}

/// Programmable kernel WireGuard link.
pub struct KernelWireguard {
    settings: KernelWgSettings,
    private_key: WgPrivateKey,
    /// Explicit underlay endpoints for static peers, keyed by node id. The
    /// mesh IP remains the overlay identity; this only selects transport.
    peer_endpoints: BTreeMap<String, SocketAddr>,
    configuration_error: Option<DataplaneError>,
    runner: Arc<dyn CommandRunner>,
    sysfs: Arc<dyn Sysfs>,
    secret_path: PathBuf,
    inner: Mutex<Inner>,
}

impl fmt::Debug for KernelWireguard {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("KernelWireguard")
            .field("settings", &self.settings)
            .field("private_key", &"[redacted]")
            .field("secret_path", &"[redacted]")
            .finish_non_exhaustive()
    }
}

impl KernelWireguard {
    /// Build a backend. Peer records are registered but nothing is programmed
    /// until [`KernelWireguard::start`].
    pub fn new(
        settings: KernelWgSettings,
        private_key: WgPrivateKey,
        configured_peers: Vec<MeshNode>,
        runner: Arc<dyn CommandRunner>,
        sysfs: Arc<dyn Sysfs>,
    ) -> Self {
        Self::new_with_peer_endpoints(
            settings,
            private_key,
            configured_peers,
            BTreeMap::new(),
            runner,
            sysfs,
        )
    }

    /// Build a backend with explicit underlay endpoints for static peers.
    ///
    /// A peer's `mesh_ip` remains its overlay identity and `/32` route; the
    /// endpoint map only changes where encrypted UDP is sent, avoiding a
    /// transport route that recurses into the WireGuard link.
    pub fn new_with_peer_endpoints(
        settings: KernelWgSettings,
        private_key: WgPrivateKey,
        configured_peers: Vec<MeshNode>,
        peer_endpoints: BTreeMap<String, SocketAddr>,
        runner: Arc<dyn CommandRunner>,
        sysfs: Arc<dyn Sysfs>,
    ) -> Self {
        let mut peers = BTreeMap::new();
        let mut configuration_error = None;
        for node in configured_peers {
            let endpoint = peer_endpoints
                .get(&node.id)
                .copied()
                .map(Ok)
                .unwrap_or_else(|| peer_endpoint(&node, None));
            match endpoint {
                Ok(endpoint) => {
                    peers.insert(
                        node.id.clone(),
                        PeerRecord {
                            node,
                            endpoint,
                            origin: PeerOrigin::Static,
                            capabilities: Vec::new(),
                        },
                    );
                }
                Err(error) => {
                    configuration_error = Some(DataplaneError::invalid(format!(
                        "peer {}: {error}",
                        node.id
                    )));
                    break;
                }
            }
        }
        Self {
            settings,
            private_key,
            peer_endpoints,
            configuration_error,
            runner,
            sysfs,
            secret_path: default_secret_path(),
            inner: Mutex::new(Inner {
                state: DataplaneState::Stopped,
                applied: Vec::new(),
                error: None,
                peers,
            }),
        }
    }

    /// Override the short-lived private-key file path (tests/diagnostics).
    pub fn with_secret_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.secret_path = path.into();
        self
    }

    pub fn settings(&self) -> &KernelWgSettings {
        &self.settings
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn set_state(
        &self,
        state: DataplaneState,
        error: Option<DataplaneError>,
        applied: Vec<Applied>,
    ) {
        let mut inner = self.lock();
        inner.state = state;
        inner.error = error;
        inner.applied = applied;
    }

    /// Check root, the kernel module, `ip`, and `wg`.
    pub fn preflight(&self) -> Result<(), DataplaneError> {
        if !self.sysfs.is_root() {
            return Err(DataplaneError::root_required(
                "kernel WireGuard requires root to create/config its link, addresses, peers, \
                 and routes",
            ));
        }
        if !self.sysfs.exists(Path::new(WIREGUARD_MODULE_PATH)) {
            return Err(DataplaneError::unavailable(format!(
                "the wireguard kernel module is not loaded ({WIREGUARD_MODULE_PATH} is absent); \
                 load it with `modprobe wireguard`"
            )));
        }
        if !self.sysfs.command_available("ip") {
            return Err(DataplaneError::unavailable(
                "the ip binary is not on PATH; kernel WireGuard cannot program its link/routes",
            ));
        }
        if !self.sysfs.command_available("wg") {
            return Err(DataplaneError::unavailable(
                "the wg binary is not on PATH; install wireguard-tools",
            ));
        }
        Ok(())
    }

    /// Create and program the kernel link. Idempotent while running.
    pub fn start(&self) -> Result<KernelWgStatus, DataplaneError> {
        if self.lock().state == DataplaneState::Running {
            return Ok(self.status());
        }
        if let Some(error) = &self.configuration_error {
            self.set_state(DataplaneState::Failed, Some(error.clone()), Vec::new());
            return Err(error.clone());
        }

        let leftover = self.lock().applied.clone();
        if !leftover.is_empty() {
            if let Some(error) = self.cleanup(&leftover) {
                self.set_state(DataplaneState::Failed, Some(error.clone()), leftover);
                return Err(error);
            }
            self.set_state(DataplaneState::Starting, None, Vec::new());
        }

        if let Err(error) = self.settings.validate() {
            self.set_state(DataplaneState::Failed, Some(error.clone()), Vec::new());
            return Err(error);
        }
        if let Err(error) = self.preflight() {
            self.set_state(DataplaneState::Failed, Some(error.clone()), Vec::new());
            return Err(error);
        }

        let records: Vec<PeerRecord> = self.lock().peers.values().cloned().collect();
        self.set_state(DataplaneState::Starting, None, Vec::new());
        let mut applied = Vec::new();
        match self.apply(&records, &mut applied) {
            Ok(()) => {
                self.set_state(DataplaneState::Running, None, applied);
                info!(
                    interface = %self.settings.interface,
                    address = %self.settings.address_cidr(),
                    listen_port = self.settings.listen_port,
                    peers = records.len(),
                    "kernel WireGuard link programmed"
                );
                Ok(self.status())
            }
            Err(error) => {
                let cleanup_error = self.cleanup(&applied);
                let mut error = error;
                if let Some(cleanup) = &cleanup_error {
                    error.reason =
                        format!("{}; rollback incomplete: {}", error.reason, cleanup.reason);
                }
                let retry = if cleanup_error.is_some() {
                    applied
                } else {
                    Vec::new()
                };
                self.set_state(DataplaneState::Failed, Some(error.clone()), retry);
                warn!(
                    code = %error.code,
                    reason = %error.reason,
                    "kernel WireGuard start failed"
                );
                Err(error)
            }
        }
    }

    /// Tear down the link/peers/routes in reverse order. Safe to repeat.
    pub fn stop(&self) -> Result<(), DataplaneError> {
        let applied = self.lock().applied.clone();
        if applied.is_empty() {
            self.set_state(DataplaneState::Stopped, None, Vec::new());
            return Ok(());
        }
        if let Some(error) = self.cleanup(&applied) {
            self.set_state(DataplaneState::Failed, Some(error.clone()), applied);
            return Err(error);
        }
        self.set_state(DataplaneState::Stopped, None, Vec::new());
        debug!(interface = %self.settings.interface, "kernel WireGuard link stopped");
        Ok(())
    }

    /// Add a static peer while running.
    pub fn add_peer(&self, node: MeshNode) -> Result<(), DataplaneError> {
        if self.lock().state != DataplaneState::Running {
            return Err(DataplaneError::not_running(
                "kernel WireGuard is not running; start it before adding peers",
            ));
        }
        if self.lock().peers.contains_key(&node.id) {
            return Err(DataplaneError::new(
                "mesh.peer-already-exists",
                format!("peer {} already exists", node.id),
            ));
        }
        let endpoint = self
            .peer_endpoints
            .get(&node.id)
            .copied()
            .map(Ok)
            .unwrap_or_else(|| peer_endpoint(&node, None))
            .map_err(|error| DataplaneError::invalid(format!("peer {}: {error}", node.id)))?;
        let record = PeerRecord {
            node,
            endpoint,
            origin: PeerOrigin::Static,
            capabilities: Vec::new(),
        };
        self.apply_peer_transaction(record.clone())?;
        self.lock().peers.insert(record.node.id.clone(), record);
        Ok(())
    }

    /// Add or update a peer learned through authorized discovery.
    pub fn add_discovered_peer(
        &self,
        node: MeshNode,
        capabilities: Vec<String>,
        underlay_ip: Option<Ipv4Addr>,
    ) -> Result<PeerChange, DataplaneError> {
        if self.lock().state != DataplaneState::Running {
            return Err(DataplaneError::not_running(
                "kernel WireGuard is not running; start it before adding peers",
            ));
        }
        validate_peer_node(&node)
            .map_err(|error| DataplaneError::invalid(format!("peer {}: {error}", node.id)))?;
        let capabilities = normalize_capabilities(&capabilities)
            .map_err(|error| DataplaneError::invalid(error.to_string()))?;
        let endpoint = peer_endpoint(&node, underlay_ip)
            .map_err(|error| DataplaneError::invalid(format!("peer {}: {error}", node.id)))?;

        let existing = self.lock().peers.get(&node.id).cloned();
        if let Some(existing) = &existing {
            if existing.origin == PeerOrigin::Static {
                return Err(DataplaneError::new(
                    "mesh.peer-conflict",
                    format!(
                        "discovered peer {} conflicts with a static peer; static configuration \
                         is authoritative",
                        node.id
                    ),
                ));
            }
            if existing.node.wg_pubkey != node.wg_pubkey {
                return Err(DataplaneError::new(
                    "mesh.peer-conflict",
                    format!(
                        "discovered peer {} changed WireGuard public key while registered",
                        node.id
                    ),
                ));
            }
            if existing.node == node
                && existing.capabilities == capabilities
                && existing.endpoint == endpoint
            {
                return Ok(PeerChange::Unchanged);
            }
        }

        self.reject_other_owner(&node)?;
        let previous = existing.as_ref().map(|record| record.node.clone());
        let record = PeerRecord {
            node,
            endpoint,
            origin: PeerOrigin::Discovered,
            capabilities,
        };
        // `wg set peer` updates endpoint/allowed-ips in place for the same key,
        // so a changed mesh IP or underlay endpoint never creates a second peer.
        self.apply_peer_transaction(record.clone())?;
        self.lock()
            .peers
            .insert(record.node.id.clone(), record.clone());
        Ok(match previous {
            Some(previous) => PeerChange::Updated { previous },
            None => PeerChange::Added,
        })
    }

    /// Remove a discovered peer, returning its identity when it existed.
    pub fn remove_discovered_peer(
        &self,
        peer_id: &str,
    ) -> Result<Option<MeshNode>, DataplaneError> {
        let record = self.lock().peers.get(peer_id).cloned();
        let Some(record) = record else {
            return Ok(None);
        };
        if record.origin == PeerOrigin::Static {
            return Err(DataplaneError::new(
                "mesh.peer-conflict",
                format!("static peer {peer_id} cannot be removed by discovery"),
            ));
        }
        self.remove_peer_record(&record)?;
        Ok(Some(record.node))
    }

    /// Remove a peer and its `/32` route.
    pub fn remove_peer(&self, peer_id: &str) -> Result<(), DataplaneError> {
        let record = self.lock().peers.get(peer_id).cloned().ok_or_else(|| {
            DataplaneError::new("mesh.peer-not-found", format!("peer {peer_id} is unknown"))
        })?;
        self.remove_peer_record(&record)
    }

    fn remove_peer_record(&self, record: &PeerRecord) -> Result<(), DataplaneError> {
        if self.lock().state != DataplaneState::Running {
            return Err(DataplaneError::not_running(
                "kernel WireGuard is not running; there is no peer table to change",
            ));
        }
        if let Ok(route) = record.node.mesh_ip.parse::<Ipv4Addr>() {
            self.remove_route_command(route)?;
        }
        self.run_teardown(&peer_remove_spec(&self.settings.interface, &record.node))?;
        {
            let mut inner = self.lock();
            inner.peers.remove(&record.node.id);
            inner.applied.retain(
                |step| !matches!(step, Applied::Peer(key) if key == &record.node.wg_pubkey.0),
            );
            inner
                .applied
                .retain(|step| !matches!(step, Applied::Route(existing) if record.node.mesh_ip.parse::<Ipv4Addr>().ok() == Some(*existing)));
        }
        info!(peer_id = %record.node.id, "kernel WireGuard peer removed");
        Ok(())
    }

    /// Kernel WireGuard has no userspace "initiate" command; real traffic
    /// triggers the handshake. Validate the peer so callers cannot mistake a
    /// missing peer for a started handshake.
    pub fn initiate_handshake(&self, peer_id: &str) -> Result<(), DataplaneError> {
        if self.lock().peers.contains_key(peer_id) {
            debug!(
                peer_id = %peer_id,
                "kernel WireGuard handshake is driven by traffic; no userspace initiation is needed"
            );
            Ok(())
        } else {
            Err(DataplaneError::new(
                "mesh.peer-not-found",
                format!("peer {peer_id} is unknown"),
            ))
        }
    }

    /// Honest peer snapshot. `established` requires a non-zero, recent kernel
    /// handshake and never follows from configuration alone.
    pub fn peer_statuses(&self) -> Vec<PeerStatus> {
        let records: Vec<PeerRecord> = self.lock().peers.values().cloned().collect();
        let dump = self.wg_dump().ok();
        records
            .into_iter()
            .map(|record| {
                let peer = dump.as_ref().and_then(|dump| {
                    dump.peers
                        .iter()
                        .find(|peer| peer.public_key == record.node.wg_pubkey.0)
                        .cloned()
                });
                let age = peer.as_ref().and_then(|peer| peer.last_handshake_age_secs);
                PeerStatus {
                    node: record.node,
                    origin: record.origin,
                    capabilities: record.capabilities,
                    endpoint: record.endpoint.to_string(),
                    established: age.is_some_and(|age| age <= HANDSHAKE_REJECT_AFTER_SECS),
                    last_handshake_age_secs: age,
                    tx_bytes: peer
                        .as_ref()
                        .map(|peer| peer.tx_bytes as usize)
                        .unwrap_or(0),
                    rx_bytes: peer
                        .as_ref()
                        .map(|peer| peer.rx_bytes as usize)
                        .unwrap_or(0),
                    loss: 0.0,
                    rtt_ms: None,
                }
            })
            .collect()
    }

    /// Current status. `wg show dump` is consulted while running so public key,
    /// listen port, handshake age, and byte counters reflect the kernel.
    pub fn status(&self) -> KernelWgStatus {
        let (state, applied, error, records) = {
            let inner = self.lock();
            (
                inner.state,
                inner.applied.clone(),
                inner.error.clone(),
                inner.peers.values().cloned().collect::<Vec<_>>(),
            )
        };
        let running = state == DataplaneState::Running;
        let mut public_key = self.private_key.public_key().0;
        let mut listen_port = self.settings.listen_port;
        let mut last_error = error;
        let mut peers = Vec::new();

        if running {
            match self.wg_dump() {
                Ok(dump) => {
                    public_key = dump.public_key;
                    listen_port = dump.listen_port;
                    peers = dump.peers;
                }
                Err(error) => {
                    last_error = Some(error);
                    peers = records
                        .iter()
                        .map(|record| idle_dump_peer(record))
                        .collect();
                }
            }
        } else if !records.is_empty() {
            peers = records.iter().map(idle_dump_peer).collect();
        }

        KernelWgStatus {
            configured: true,
            state,
            interface: self.settings.interface.clone(),
            public_key,
            listen_port,
            address: self.settings.address_cidr(),
            mtu: self.settings.mtu,
            up: running,
            routes: if running {
                applied
                    .iter()
                    .filter_map(|step| match step {
                        Applied::Route(route) => Some(format!("{route}/32")),
                        _ => None,
                    })
                    .collect()
            } else {
                Vec::new()
            },
            peers,
            last_error,
        }
    }

    fn apply(
        &self,
        records: &[PeerRecord],
        applied: &mut Vec<Applied>,
    ) -> Result<(), DataplaneError> {
        let interface = self.settings.interface.clone();
        let link = CommandSpec::new(
            "ip",
            [
                "link".to_owned(),
                "add".to_owned(),
                "dev".to_owned(),
                interface.clone(),
                "type".to_owned(),
                "wireguard".to_owned(),
            ],
        );
        let output = self.invoke(&link)?;
        if !output.success() {
            if looks_like_missing_wireguard(&output) {
                return Err(DataplaneError::unavailable(format!(
                    "the kernel refused `{link}`; the wireguard module is not available \
                     ({}). Load it with `modprobe wireguard`",
                    short_reason(command_detail(&output))
                )));
            }
            return Err(command_failure(&link, &output));
        }
        applied.push(Applied::Link(interface.clone()));

        self.install_private_key()?;

        self.run(&CommandSpec::new(
            "ip",
            [
                "link".to_owned(),
                "set".to_owned(),
                "dev".to_owned(),
                interface.clone(),
                "mtu".to_owned(),
                self.settings.mtu.to_string(),
            ],
        ))?;

        self.run(&CommandSpec::new(
            "ip",
            [
                "addr".to_owned(),
                "replace".to_owned(),
                self.settings.address_cidr(),
                "dev".to_owned(),
                interface.clone(),
            ],
        ))?;

        self.run(&CommandSpec::new(
            "ip",
            [
                "link".to_owned(),
                "set".to_owned(),
                "dev".to_owned(),
                interface.clone(),
                "up".to_owned(),
            ],
        ))?;

        for record in records {
            self.run(&peer_set_spec(&self.settings, record))?;
            applied.push(Applied::Peer(record.node.wg_pubkey.0.clone()));
            let route = record.node.mesh_ip.parse::<Ipv4Addr>().map_err(|_| {
                DataplaneError::invalid(format!(
                    "peer {} has an invalid IPv4 mesh address",
                    record.node.id
                ))
            })?;
            self.run(&route_replace_spec(&interface, route))?;
            applied.push(Applied::Route(route));
        }
        Ok(())
    }

    fn apply_peer_transaction(&self, record: PeerRecord) -> Result<(), DataplaneError> {
        let route = record.node.mesh_ip.parse::<Ipv4Addr>().map_err(|_| {
            DataplaneError::invalid(format!(
                "peer {} has an invalid IPv4 mesh address",
                record.node.id
            ))
        })?;
        self.run(&peer_set_spec(&self.settings, &record))?;
        if let Err(error) = self.run(&route_replace_spec(&self.settings.interface, route)) {
            let _ = self.run_teardown(&peer_remove_spec(&self.settings.interface, &record.node));
            return Err(error);
        }
        let mut inner = self.lock();
        inner
            .applied
            .push(Applied::Peer(record.node.wg_pubkey.0.clone()));
        inner.applied.push(Applied::Route(route));
        Ok(())
    }

    fn remove_route_command(&self, route: Ipv4Addr) -> Result<(), DataplaneError> {
        let applied = self.lock().applied.clone();
        if !applied
            .iter()
            .any(|step| matches!(step, Applied::Route(existing) if *existing == route))
        {
            return Ok(());
        }
        self.run_teardown(&route_del_spec(&self.settings.interface, route))?;
        self.lock()
            .applied
            .retain(|step| !matches!(step, Applied::Route(existing) if *existing == route));
        Ok(())
    }

    fn install_private_key(&self) -> Result<(), DataplaneError> {
        self.sysfs
            .write_secret(&self.secret_path, &self.private_key.to_base64())
            .map_err(|error| {
                DataplaneError::unavailable(format!(
                    "cannot write the temporary WireGuard private-key file: {error}"
                ))
            })?;
        let command = CommandSpec::new(
            "wg",
            [
                "set".to_owned(),
                self.settings.interface.clone(),
                "private-key".to_owned(),
                self.secret_path.display().to_string(),
                "listen-port".to_owned(),
                self.settings.listen_port.to_string(),
            ],
        );
        let result = self.run(&command);
        if let Err(error) = self.sysfs.remove(&self.secret_path) {
            warn!("cannot remove temporary WireGuard private-key file: {error}");
        }
        result
    }

    fn wg_dump(&self) -> Result<WgDump, DataplaneError> {
        let command = CommandSpec::new(
            "wg",
            [
                "show".to_owned(),
                self.settings.interface.clone(),
                "dump".to_owned(),
            ],
        );
        let output = self.invoke(&command)?;
        if !output.success() {
            return Err(command_failure_redacted(&command, &output));
        }
        parse_wg_dump(&output.stdout, now_unix_secs())
    }

    fn invoke(&self, command: &CommandSpec) -> Result<CommandOutput, DataplaneError> {
        self.runner.run(command).map_err(|error| {
            DataplaneError::command_failed(format!(
                "could not execute `{command}`: {}",
                redact_keys(&error.to_string())
            ))
        })
    }

    fn run(&self, command: &CommandSpec) -> Result<(), DataplaneError> {
        let output = self.invoke(command)?;
        if output.success() {
            return Ok(());
        }
        Err(command_failure_redacted(command, &output))
    }

    fn run_teardown(&self, command: &CommandSpec) -> Result<(), DataplaneError> {
        match self.run(command) {
            Ok(()) => Ok(()),
            Err(error) if is_already_absent(&error.reason) => {
                debug!(command = %command, "kernel WireGuard teardown: already absent");
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    fn cleanup(&self, applied: &[Applied]) -> Option<DataplaneError> {
        let interface = self.settings.interface.as_str();
        let records = self.lock().peers.clone();
        let mut first_error = None;
        for step in applied.iter().rev() {
            let result = match step {
                Applied::Route(route) => self.run_teardown(&route_del_spec(interface, *route)),
                Applied::Peer(public_key) => {
                    let node = records
                        .values()
                        .find(|record| &record.node.wg_pubkey.0 == public_key)
                        .map(|record| record.node.clone());
                    match node {
                        Some(node) => self.run_teardown(&peer_remove_spec(interface, &node)),
                        None => Ok(()),
                    }
                }
                Applied::Link(link) => self.run_teardown(&CommandSpec::new(
                    "ip",
                    ["link", "delete", "dev", link.as_str()],
                )),
            };
            if let Err(error) = result {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
        first_error
    }

    fn reject_other_owner(&self, candidate: &MeshNode) -> Result<(), DataplaneError> {
        if candidate.mesh_ip == self.settings.address.to_string() {
            return Err(DataplaneError::new(
                "mesh.peer-conflict",
                format!(
                    "advertised mesh IP {} is this node's own address",
                    candidate.mesh_ip
                ),
            ));
        }
        if candidate.wg_pubkey == self.private_key.public_key() {
            return Err(DataplaneError::new(
                "mesh.peer-conflict",
                "advertised public key is this node's own key",
            ));
        }
        let peers = self.lock().peers.clone();
        for (id, peer) in peers {
            if id == candidate.id {
                continue;
            }
            if peer.node.mesh_ip == candidate.mesh_ip {
                return Err(DataplaneError::new(
                    "mesh.peer-conflict",
                    format!(
                        "advertised mesh IP {} is already owned by peer {id}",
                        candidate.mesh_ip
                    ),
                ));
            }
            if peer.node.wg_pubkey == candidate.wg_pubkey {
                return Err(DataplaneError::new(
                    "mesh.peer-conflict",
                    format!("advertised public key is already owned by peer {id}"),
                ));
            }
        }
        Ok(())
    }
}

fn route_replace_spec(interface: &str, route: Ipv4Addr) -> CommandSpec {
    CommandSpec::new(
        "ip",
        [
            "route".to_owned(),
            "replace".to_owned(),
            format!("{route}/32"),
            "dev".to_owned(),
            interface.to_owned(),
        ],
    )
}

fn route_del_spec(interface: &str, route: Ipv4Addr) -> CommandSpec {
    CommandSpec::new(
        "ip",
        [
            "route".to_owned(),
            "del".to_owned(),
            format!("{route}/32"),
            "dev".to_owned(),
            interface.to_owned(),
        ],
    )
}

fn peer_set_spec(settings: &KernelWgSettings, record: &PeerRecord) -> CommandSpec {
    let mut args = vec![
        "set".to_owned(),
        settings.interface.clone(),
        "peer".to_owned(),
        record.node.wg_pubkey.0.clone(),
        "allowed-ips".to_owned(),
        format!("{}/32", record.node.mesh_ip),
        "endpoint".to_owned(),
        record.endpoint.to_string(),
    ];
    if settings.persistent_keepalive > 0 {
        args.push("persistent-keepalive".to_owned());
        args.push(settings.persistent_keepalive.to_string());
    }
    CommandSpec::new("wg", args)
}

fn peer_remove_spec(interface: &str, node: &MeshNode) -> CommandSpec {
    CommandSpec::new(
        "wg",
        [
            "set".to_owned(),
            interface.to_owned(),
            "peer".to_owned(),
            node.wg_pubkey.0.clone(),
            "remove".to_owned(),
        ],
    )
}

fn peer_endpoint(node: &MeshNode, underlay: Option<Ipv4Addr>) -> Result<SocketAddr, String> {
    let mesh_ip = node
        .mesh_ip
        .parse::<Ipv4Addr>()
        .map_err(|_| format!("mesh IP {:?} is not IPv4", node.mesh_ip))?;
    let host = underlay.unwrap_or(mesh_ip);
    Ok(SocketAddr::from((host, node.wg_port)))
}

fn idle_dump_peer(record: &PeerRecord) -> WgDumpPeer {
    WgDumpPeer {
        public_key: record.node.wg_pubkey.0.clone(),
        endpoint: Some(record.endpoint.to_string()),
        allowed_ips: vec![format!("{}/32", record.node.mesh_ip)],
        last_handshake_age_secs: None,
        tx_bytes: 0,
        rx_bytes: 0,
        persistent_keepalive: None,
    }
}

fn default_secret_path() -> PathBuf {
    let sequence = SECRET_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "ozma-wg-{}-{nanos}-{sequence}.key",
        std::process::id()
    ))
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn command_detail(output: &CommandOutput) -> &str {
    if output.stderr.trim().is_empty() {
        output.stdout.trim()
    } else {
        output.stderr.trim()
    }
}

/// Redact anything that looks like a WireGuard base64 key from diagnostics.
///
/// This is deliberately broader than needed (it also hides public keys in
/// error text); status documents carry public keys explicitly instead.
pub fn redact_keys(text: &str) -> String {
    let mut redacted = String::with_capacity(text.len());
    let mut token = String::new();
    for character in text.chars() {
        if character.is_whitespace() {
            push_redacted_token(&mut redacted, &mut token);
            redacted.push(character);
        } else {
            token.push(character);
        }
    }
    push_redacted_token(&mut redacted, &mut token);
    redacted
}

fn push_redacted_token(output: &mut String, token: &mut String) {
    // Redact key-shaped runs even when they are embedded in punctuation such
    // as `key=<base64>`.
    let mut segment = String::new();
    for character in token.chars() {
        if is_key_character(character) {
            segment.push(character);
        } else {
            redact_key_run(output, &segment);
            segment.clear();
            output.push(character);
        }
    }
    redact_key_run(output, &segment);
    token.clear();
}

/// Redact any 44-character `<43 base64 chars>=` window inside a run.
fn redact_key_run(output: &mut String, run: &str) {
    let characters: Vec<char> = run.chars().collect();
    let mut copied = 0;
    for index in 0..characters.len() {
        if characters[index] != '=' || index < 43 {
            continue;
        }
        let start = index - 43;
        let body = &characters[start..index];
        if body.iter().any(|character| *character == '=') {
            continue;
        }
        output.extend(characters[copied..start].iter());
        output.push_str("[redacted]");
        copied = index + 1;
    }
    output.extend(characters[copied..].iter());
}

fn is_key_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '+' | '/' | '=')
}

fn command_failure_redacted(command: &CommandSpec, output: &CommandOutput) -> DataplaneError {
    DataplaneError::command_failed(format!(
        "`{command}` exited with status {}: {}",
        output.status,
        truncate(&redact_keys(command_detail(output)))
    ))
}

fn truncate(reason: &str) -> String {
    if reason.len() <= MAX_REASON_BYTES {
        return reason.to_owned();
    }
    let shortened: String = reason.chars().take(MAX_REASON_BYTES).collect();
    format!("{shortened}…")
}

fn looks_like_missing_wireguard(output: &CommandOutput) -> bool {
    let text = format!("{} {}", output.stdout, output.stderr).to_ascii_lowercase();
    [
        "unknown device type",
        "operation not supported",
        "not supported",
        "no such device",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::WgPublicKey;
    use std::collections::BTreeSet;
    use std::io;

    const TEST_PRIVATE: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
    const PEER_A: &str = "0KPBWU6P/UQy87dYaFCJkvDv9Bq42ki2idJHtwmaPwE=";
    const PEER_B: &str = "FxPDi5JFINCR0rCMcQyQUQjFMTR0Skf/ur8524sslnE=";
    const PEER_C: &str = "Y2NjY2NjY2NjY2NjY2NjY2NjY2NjY2NjY2NjY2NjY2M=";

    struct FakeRunner {
        calls: Mutex<Vec<CommandSpec>>,
        fail: Mutex<Option<CommandSpec>>,
        link_error: Mutex<Option<(String, String)>>,
        dump: Mutex<String>,
        dump_error_key: Mutex<Option<String>>,
    }

    impl Default for FakeRunner {
        fn default() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                fail: Mutex::new(None),
                link_error: Mutex::new(None),
                dump: Mutex::new(format!("{TEST_PRIVATE}\t{}0=\t51820\t0\n", "A".repeat(42))),
                dump_error_key: Mutex::new(None),
            }
        }
    }

    impl FakeRunner {
        fn calls(&self) -> Vec<CommandSpec> {
            self.calls
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone()
        }

        fn clear(&self) {
            self.calls
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clear();
        }

        fn fail_on(&self, command: CommandSpec) {
            *self.fail.lock().unwrap_or_else(|error| error.into_inner()) = Some(command);
        }

        fn fail_link_create(&self) {
            *self
                .link_error
                .lock()
                .unwrap_or_else(|error| error.into_inner()) =
                Some((String::new(), "Error: Unknown device type.".to_owned()));
        }

        fn set_dump(&self, dump: impl Into<String>) {
            *self.dump.lock().unwrap_or_else(|error| error.into_inner()) = dump.into();
        }

        fn fail_dump_with_key(&self, key: &str) {
            *self
                .dump_error_key
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some(key.to_owned());
        }
    }

    impl CommandRunner for FakeRunner {
        fn run(&self, command: &CommandSpec) -> io::Result<CommandOutput> {
            self.calls
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(command.clone());
            if command.program == "wg" && command.args.first().map(String::as_str) == Some("show") {
                if let Some(key) = self
                    .dump_error_key
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .clone()
                {
                    return Ok(CommandOutput {
                        status: 1,
                        stdout: String::new(),
                        stderr: format!("failed to parse key {key}"),
                    });
                }
                return Ok(CommandOutput {
                    status: 0,
                    stdout: self
                        .dump
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .clone(),
                    stderr: String::new(),
                });
            }
            let link_create = command.program == "ip"
                && command.args.first().map(String::as_str) == Some("link")
                && command.args.get(1).map(String::as_str) == Some("add");
            if link_create {
                if let Some((stdout, stderr)) = self
                    .link_error
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .clone()
                {
                    return Ok(CommandOutput {
                        status: 1,
                        stdout,
                        stderr,
                    });
                }
            }
            if self
                .fail
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .as_ref()
                == Some(command)
            {
                return Ok(CommandOutput {
                    status: 1,
                    stdout: String::new(),
                    stderr: "injected failure".to_owned(),
                });
            }
            Ok(CommandOutput {
                status: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    }

    struct FakeSysfs {
        root: bool,
        existing: Mutex<BTreeSet<PathBuf>>,
        tools: Mutex<BTreeSet<String>>,
        writes: Mutex<Vec<(PathBuf, String)>>,
        removed: Mutex<Vec<PathBuf>>,
    }

    impl FakeSysfs {
        fn rooted() -> Self {
            Self {
                root: true,
                existing: Mutex::new(BTreeSet::from([PathBuf::from(WIREGUARD_MODULE_PATH)])),
                tools: Mutex::new(BTreeSet::from(["ip".to_owned(), "wg".to_owned()])),
                writes: Mutex::new(Vec::new()),
                removed: Mutex::new(Vec::new()),
            }
        }

        fn not_root(mut self) -> Self {
            self.root = false;
            self
        }

        fn without_module(mut self) -> Self {
            self.existing
                .get_mut()
                .unwrap_or_else(|error| error.into_inner())
                .remove(Path::new(WIREGUARD_MODULE_PATH));
            self
        }

        fn without_tool(mut self, tool: &str) -> Self {
            self.tools
                .get_mut()
                .unwrap_or_else(|error| error.into_inner())
                .remove(tool);
            self
        }

        fn writes(&self) -> Vec<(PathBuf, String)> {
            self.writes
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone()
        }

        fn removed(&self) -> Vec<PathBuf> {
            self.removed
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone()
        }
    }

    impl crate::dataplane::FileOpener for FakeSysfs {
        fn exists(&self, path: &Path) -> bool {
            self.existing
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .contains(path)
        }

        fn read_to_string(&self, path: &Path) -> io::Result<String> {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{} is not a fake file", path.display()),
            ))
        }

        fn write(&self, path: &Path, contents: &str) -> io::Result<()> {
            self.writes
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push((path.to_owned(), contents.to_owned()));
            Ok(())
        }

        fn remove(&self, path: &Path) -> io::Result<()> {
            self.removed
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(path.to_owned());
            Ok(())
        }
    }

    impl Sysfs for FakeSysfs {
        fn is_root(&self) -> bool {
            self.root
        }

        fn command_available(&self, program: &str) -> bool {
            self.tools
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .contains(program)
        }
    }

    fn cmd(program: &str, args: &[&str]) -> CommandSpec {
        CommandSpec::new(program, args.iter().copied())
    }

    fn settings() -> KernelWgSettings {
        KernelWgSettings::new(
            DEFAULT_KERNEL_INTERFACE,
            Ipv4Addr::new(10, 200, 9, 1),
            24,
            DEFAULT_KERNEL_MTU,
            51_820,
        )
    }

    fn peer(id: &str, key: &str, ip: &str) -> MeshNode {
        MeshNode {
            id: id.to_owned(),
            wg_pubkey: WgPublicKey(key.to_owned()),
            mesh_ip: ip.to_owned(),
            wg_port: 51_820,
        }
    }

    fn kernel(
        settings: KernelWgSettings,
        peers: Vec<MeshNode>,
        sysfs: FakeSysfs,
    ) -> (Arc<KernelWireguard>, Arc<FakeRunner>, Arc<FakeSysfs>) {
        let runner = Arc::new(FakeRunner::default());
        let sysfs = Arc::new(sysfs);
        let private_key =
            WgPrivateKey::from_base64(TEST_PRIVATE).expect("test private key is valid");
        let kernel = Arc::new(
            KernelWireguard::new(settings, private_key, peers, runner.clone(), sysfs.clone())
                .with_secret_path("/tmp/ozma-kernel-test.key"),
        );
        (kernel, runner, sysfs)
    }

    #[test]
    fn start_programs_exact_kernel_argv() {
        let (kernel, runner, sysfs) = kernel(
            settings(),
            vec![
                peer("a", PEER_A, "10.200.9.2"),
                peer("b", PEER_B, "10.200.9.3"),
            ],
            FakeSysfs::rooted(),
        );
        let status = kernel.start().unwrap();
        assert_eq!(status.state, DataplaneState::Running);
        assert!(status.up);

        let expected = vec![
            cmd("ip", &["link", "add", "dev", "ozma0", "type", "wireguard"]),
            cmd(
                "wg",
                &[
                    "set",
                    "ozma0",
                    "private-key",
                    "/tmp/ozma-kernel-test.key",
                    "listen-port",
                    "51820",
                ],
            ),
            cmd("ip", &["link", "set", "dev", "ozma0", "mtu", "1420"]),
            cmd("ip", &["addr", "replace", "10.200.9.1/24", "dev", "ozma0"]),
            cmd("ip", &["link", "set", "dev", "ozma0", "up"]),
            cmd(
                "wg",
                &[
                    "set",
                    "ozma0",
                    "peer",
                    PEER_A,
                    "allowed-ips",
                    "10.200.9.2/32",
                    "endpoint",
                    "10.200.9.2:51820",
                    "persistent-keepalive",
                    "25",
                ],
            ),
            cmd("ip", &["route", "replace", "10.200.9.2/32", "dev", "ozma0"]),
            cmd(
                "wg",
                &[
                    "set",
                    "ozma0",
                    "peer",
                    PEER_B,
                    "allowed-ips",
                    "10.200.9.3/32",
                    "endpoint",
                    "10.200.9.3:51820",
                    "persistent-keepalive",
                    "25",
                ],
            ),
            cmd("ip", &["route", "replace", "10.200.9.3/32", "dev", "ozma0"]),
            cmd("wg", &["show", "ozma0", "dump"]),
        ];
        assert_eq!(runner.calls(), expected);

        let writes = sysfs.writes();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].0, PathBuf::from("/tmp/ozma-kernel-test.key"));
        assert_eq!(writes[0].1, TEST_PRIVATE);
        assert_eq!(
            sysfs.removed(),
            vec![PathBuf::from("/tmp/ozma-kernel-test.key")]
        );

        for call in runner.calls() {
            assert!(!call.args.iter().any(|arg| arg.contains(TEST_PRIVATE)));
            assert!(!format!("{call:?}").contains(TEST_PRIVATE));
        }
        assert!(!format!("{kernel:?}").contains(TEST_PRIVATE));
        assert!(!format!("{:?}", kernel.settings()).contains(TEST_PRIVATE));
    }

    #[test]
    fn static_peer_underlay_endpoint_overrides_the_mesh_ip() {
        let runner = Arc::new(FakeRunner::default());
        let sysfs = Arc::new(FakeSysfs::rooted());
        let private_key = WgPrivateKey::from_base64(TEST_PRIVATE).unwrap();
        let endpoints = BTreeMap::from([(
            "a".to_owned(),
            SocketAddr::from((Ipv4Addr::new(10, 10, 30, 2), 51_820)),
        )]);
        let kernel = Arc::new(
            KernelWireguard::new_with_peer_endpoints(
                settings(),
                private_key,
                vec![peer("a", PEER_A, "10.200.9.2")],
                endpoints,
                runner.clone(),
                sysfs,
            )
            .with_secret_path("/tmp/ozma-kernel-test.key"),
        );
        kernel.start().unwrap();
        let peer_call = runner
            .calls()
            .into_iter()
            .find(|call| {
                call.program == "wg" && call.args.get(2).map(String::as_str) == Some("peer")
            })
            .expect("peer wg set call");
        assert!(peer_call.args.contains(&"10.10.30.2:51820".to_owned()));
        assert!(!peer_call.args.contains(&"10.200.9.2:51820".to_owned()));
        assert!(runner.calls().contains(&cmd(
            "ip",
            &["route", "replace", "10.200.9.2/32", "dev", "ozma0"],
        )));
    }

    #[test]
    fn runtime_peer_add_and_remove_update_wg_and_routes() {
        let (kernel, runner, _sysfs) = kernel(settings(), Vec::new(), FakeSysfs::rooted());
        kernel.start().unwrap();
        runner.clear();

        kernel.add_peer(peer("c", PEER_C, "10.200.9.4")).unwrap();
        assert_eq!(
            runner.calls(),
            vec![
                cmd(
                    "wg",
                    &[
                        "set",
                        "ozma0",
                        "peer",
                        PEER_C,
                        "allowed-ips",
                        "10.200.9.4/32",
                        "endpoint",
                        "10.200.9.4:51820",
                        "persistent-keepalive",
                        "25",
                    ],
                ),
                cmd("ip", &["route", "replace", "10.200.9.4/32", "dev", "ozma0"],),
            ]
        );

        runner.clear();
        kernel.remove_peer("c").unwrap();
        assert_eq!(
            runner.calls(),
            vec![
                cmd("ip", &["route", "del", "10.200.9.4/32", "dev", "ozma0"],),
                cmd("wg", &["set", "ozma0", "peer", PEER_C, "remove"]),
            ]
        );
    }

    #[test]
    fn peer_add_rolls_back_when_route_fails() {
        let (kernel, runner, _sysfs) = kernel(settings(), Vec::new(), FakeSysfs::rooted());
        kernel.start().unwrap();
        runner.clear();
        runner.fail_on(cmd(
            "ip",
            &["route", "replace", "10.200.9.5/32", "dev", "ozma0"],
        ));
        let error = kernel
            .add_peer(peer("d", PEER_C, "10.200.9.5"))
            .unwrap_err();
        assert_eq!(error.code, "mesh.dataplane-command-failed");
        assert_eq!(
            runner.calls().last(),
            Some(&cmd("wg", &["set", "ozma0", "peer", PEER_C, "remove"]))
        );
    }

    #[test]
    fn discovery_adds_updates_and_removes_peer_routes() {
        let (kernel, runner, _sysfs) = kernel(settings(), Vec::new(), FakeSysfs::rooted());
        kernel.start().unwrap();
        runner.clear();

        let change = kernel
            .add_discovered_peer(
                peer("d", PEER_C, "10.200.9.5"),
                vec!["audio".to_owned()],
                Some(Ipv4Addr::new(10, 10, 20, 1)),
            )
            .unwrap();
        assert_eq!(change, PeerChange::Added);
        assert!(runner
            .calls()
            .iter()
            .any(|call| call.args.contains(&PEER_C.to_owned())
                && call.args.contains(&"10.10.20.1:51820".to_owned())));

        runner.clear();
        let change = kernel
            .add_discovered_peer(
                peer("d", PEER_C, "10.200.9.6"),
                vec!["audio".to_owned()],
                Some(Ipv4Addr::new(10, 10, 20, 1)),
            )
            .unwrap();
        assert!(matches!(change, PeerChange::Updated { .. }));
        assert_eq!(
            runner.calls(),
            vec![
                cmd(
                    "wg",
                    &[
                        "set",
                        "ozma0",
                        "peer",
                        PEER_C,
                        "allowed-ips",
                        "10.200.9.6/32",
                        "endpoint",
                        "10.10.20.1:51820",
                        "persistent-keepalive",
                        "25",
                    ],
                ),
                cmd("ip", &["route", "replace", "10.200.9.6/32", "dev", "ozma0"],),
            ]
        );

        runner.clear();
        let removed = kernel.remove_discovered_peer("d").unwrap().unwrap();
        assert_eq!(removed.id, "d");
        assert!(runner
            .calls()
            .contains(&cmd("wg", &["set", "ozma0", "peer", PEER_C, "remove"])));
    }

    #[test]
    fn status_parses_wg_dump_without_exposing_private_key() {
        let (kernel, runner, _sysfs) = kernel(
            settings(),
            vec![peer("a", PEER_A, "10.200.9.2")],
            FakeSysfs::rooted(),
        );
        let now = now_unix_secs();
        runner.set_dump(format!(
            "{TEST_PRIVATE}\t{PEER_A}\t51821\t0\n\
             {PEER_A}\t(none)\t10.200.9.2:51820\t10.200.9.2/32\t{}\t120\t340\t25\n",
            now - 30
        ));
        kernel.start().unwrap();
        let status = kernel.status();
        assert_eq!(status.public_key, PEER_A);
        assert_eq!(status.listen_port, 51821);
        assert_eq!(status.state, DataplaneState::Running);
        assert_eq!(status.routes, vec!["10.200.9.2/32"]);
        assert_eq!(status.peers.len(), 1);
        let peer = &status.peers[0];
        assert_eq!(peer.public_key, PEER_A);
        assert!(peer.last_handshake_age_secs.unwrap() >= 30);
        assert!(peer.last_handshake_age_secs.unwrap() <= 31);
        assert_eq!(peer.rx_bytes, 120);
        assert_eq!(peer.tx_bytes, 340);
        assert_eq!(peer.endpoint.as_deref(), Some("10.200.9.2:51820"));

        let serialized = serde_json::to_string(&status).unwrap();
        assert!(!serialized.contains(TEST_PRIVATE));
        assert!(!format!("{status:?}").contains(TEST_PRIVATE));

        let peers = kernel.peer_statuses();
        assert_eq!(peers.len(), 1);
        assert!(peers[0].established);
        assert_eq!(peers[0].tx_bytes, 340);
        assert_eq!(peers[0].rx_bytes, 120);
    }

    #[test]
    fn wg_dump_parses_none_and_zero_handshake_honestly() {
        let dump = format!(
            "{TEST_PRIVATE}\t{PEER_A}\t51820\t0\n\
             {PEER_A}\t(none)\t(none)\t(none)\t0\t0\t0\toff\n\
             {PEER_B}\t(none)\t10.0.0.2:51820\t10.200.9.3/32\t1800000000\t7\t9\t25\n"
        );
        let parsed = parse_wg_dump(&dump, 1_800_000_100).unwrap();
        assert_eq!(parsed.public_key, PEER_A);
        assert_eq!(parsed.peers[0].endpoint, None);
        assert!(parsed.peers[0].allowed_ips.is_empty());
        assert_eq!(parsed.peers[0].last_handshake_age_secs, None);
        assert_eq!(parsed.peers[0].persistent_keepalive, None);
        assert_eq!(parsed.peers[1].last_handshake_age_secs, Some(100));
        assert_eq!(parsed.peers[1].persistent_keepalive, Some(25));
        assert!(!format!("{parsed:?}").contains(TEST_PRIVATE));
    }

    #[test]
    fn preflight_reports_typed_root_module_and_tool_errors() {
        let run = |sysfs: FakeSysfs| {
            let (kernel, _runner, _sysfs) = kernel(settings(), Vec::new(), sysfs);
            kernel.start().unwrap_err()
        };

        assert_eq!(
            run(FakeSysfs::rooted().not_root()).code,
            "mesh.root-required"
        );
        let missing_module = run(FakeSysfs::rooted().without_module());
        assert_eq!(missing_module.code, "mesh.dataplane-unavailable");
        assert!(missing_module.reason.contains("wireguard kernel module"));
        assert_eq!(
            run(FakeSysfs::rooted().without_tool("wg")).code,
            "mesh.dataplane-unavailable"
        );
        assert_eq!(
            run(FakeSysfs::rooted().without_tool("ip")).code,
            "mesh.dataplane-unavailable"
        );
    }

    #[test]
    fn invalid_configured_peer_fails_before_programming() {
        let (kernel, runner, _sysfs) = kernel(
            settings(),
            vec![peer("bad", PEER_A, "not-an-ip")],
            FakeSysfs::rooted(),
        );
        let error = kernel.start().unwrap_err();
        assert_eq!(error.code, "mesh.dataplane-invalid");
        assert!(error.reason.contains("peer bad"));
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn missing_kernel_support_at_link_creation_is_typed_unavailable() {
        let (kernel, runner, _sysfs) = kernel(settings(), Vec::new(), FakeSysfs::rooted());
        runner.fail_link_create();
        let error = kernel.start().unwrap_err();
        assert_eq!(error.code, "mesh.dataplane-unavailable");
        assert!(error.reason.contains("wireguard module"));
    }

    #[test]
    fn start_rolls_back_link_when_route_fails() {
        let (kernel, runner, sysfs) = kernel(
            settings(),
            vec![peer("a", PEER_A, "10.200.9.2")],
            FakeSysfs::rooted(),
        );
        runner.fail_on(cmd(
            "ip",
            &["route", "replace", "10.200.9.2/32", "dev", "ozma0"],
        ));
        let error = kernel.start().unwrap_err();
        assert_eq!(error.code, "mesh.dataplane-command-failed");
        let calls = runner.calls();
        assert_eq!(
            calls.last(),
            Some(&cmd("ip", &["link", "delete", "dev", "ozma0"]))
        );
        assert!(calls.contains(&cmd("wg", &["set", "ozma0", "peer", PEER_A, "remove"])));
        assert_eq!(
            sysfs.removed(),
            vec![PathBuf::from("/tmp/ozma-kernel-test.key")]
        );
    }

    #[test]
    fn wg_show_errors_are_redacted() {
        let (kernel, runner, _sysfs) = kernel(settings(), Vec::new(), FakeSysfs::rooted());
        runner.fail_dump_with_key(TEST_PRIVATE);
        kernel.start().unwrap();
        let status = kernel.status();
        let error = status.last_error.unwrap();
        assert!(!error.reason.contains(TEST_PRIVATE));
        assert!(error.reason.contains("[redacted]"));
        assert!(
            redact_keys("prefix AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA= suffix")
                .contains("[redacted]")
        );
        assert!(
            !redact_keys("key=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=.")
                .contains("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
        );
        assert_eq!(redact_keys("ordinary text"), "ordinary text");
    }

    #[test]
    fn stop_removes_routes_peers_and_link_in_reverse_order() {
        let (kernel, runner, _sysfs) = kernel(
            settings(),
            vec![peer("a", PEER_A, "10.200.9.2")],
            FakeSysfs::rooted(),
        );
        kernel.start().unwrap();
        runner.clear();
        kernel.stop().unwrap();
        assert_eq!(
            runner.calls(),
            vec![
                cmd("ip", &["route", "del", "10.200.9.2/32", "dev", "ozma0"]),
                cmd("wg", &["set", "ozma0", "peer", PEER_A, "remove"]),
                cmd("ip", &["link", "delete", "dev", "ozma0"]),
            ]
        );
        assert_eq!(kernel.status().state, DataplaneState::Stopped);
        runner.clear();
        kernel.stop().unwrap();
        assert!(runner.calls().is_empty());
    }
}
