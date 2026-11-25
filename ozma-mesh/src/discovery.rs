//! Opt-in mDNS peer discovery for the ozma mesh.
//!
//! The mesh service advertises itself on `_ozma-mesh._udp.local` (a dedicated
//! service type so mesh TCP/UDP ports do not collide with the node's
//! controller-facing `_ozma._udp` advertisement).  This module provides:
//!
//! * a strict DNS packet parser/query builder used by [`MdnsBrowser`];
//! * [`MdnsBrowser`], a bounded, non-blocking mDNS browse client that returns
//!   [`RawAdvertisement`] descriptors;
//! * [`DiscoveryEngine`], the deterministic add/update/expire/conflict state
//!   machine shared by production and tests.
//!
//! # Trust model
//!
//! An mDNS record is **unauthenticated** and must never be believed on its own.
//! [`DiscoveryEngine`] only authorizes an advertisement when:
//!
//! 1. it is exactly one of the configured static peers (`--mesh-peer`), or
//! 2. the advertised node id and WireGuard public key match a trusted entry
//!    (`--mesh-trust`), with any configured mesh-IP restriction.
//!
//! Static peers are authoritative: a conflicting advertisement is reported as
//! a conflict and never replaces or adds a peer.  Duplicate advertisements are
//! deduplicated by id and mesh IP, and entries expire after the configured TTL.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::error::{MeshError, Result};
use crate::node::{MeshNode, WgPublicKey};

/// Fully-qualified DNS-SD service type browsed for mesh peers.
pub const MDNS_FQDN: &str = "_ozma-mesh._udp.local";
/// Service type without the `.local` suffix, as used by libmdns.
pub const MDNS_SERVICE_TYPE: &str = "_ozma-mesh._udp";
/// IPv4 mDNS group.
pub const MDNS_GROUP: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
/// mDNS UDP port.
pub const MDNS_PORT: u16 = 5353;
/// Default time a peer may be absent before it is expired.
pub const DEFAULT_DISCOVERY_TTL: Duration = Duration::from_secs(60);
/// Default interval between browse queries.
pub const DEFAULT_QUERY_INTERVAL: Duration = Duration::from_secs(5);
/// Default maximum number of simultaneously discovered peers.
pub const DEFAULT_MAX_PEERS: usize = 32;
/// Hard upper bound for discovered peers.
pub const MAX_PEERS: usize = 256;
/// Maximum accepted node id length in an advertisement.
pub const MAX_ID_BYTES: usize = 128;
/// Maximum accepted capabilities per peer.
pub const MAX_CAPABILITIES: usize = 16;
/// Maximum accepted length of one capability.
pub const MAX_CAPABILITY_BYTES: usize = 64;
/// Records split across packets are merged for this long.
const PENDING_MERGE_WINDOW: Duration = Duration::from_secs(2);
/// Socket read timeout used by [`MdnsBrowser::poll`].
const BROWSER_READ_TIMEOUT: Duration = Duration::from_millis(250);

// DNS record types used by the browse parser.
const TYPE_A: u16 = 1;
const TYPE_PTR: u16 = 12;
const TYPE_TXT: u16 = 16;
const TYPE_SRV: u16 = 33;
// DNS class IN with the mDNS unicast-response (QU) bit.
const CLASS_IN_QU: u16 = 0x8001;

// ── Configuration / authorization ────────────────────────────────────────────

/// A key/host authorization entry for non-static discovered peers.
///
/// `mesh_ip` is optional; when present the advertisement must use exactly that
/// address.  Configured static peers are trusted automatically and do not need
/// a duplicate entry here.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustedPeer {
    /// Node id advertised in the TXT record.
    pub id: String,
    /// Base64 X25519 WireGuard public key.
    pub wg_pubkey: String,
    /// Optional mesh-IP restriction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mesh_ip: Option<String>,
}

impl TrustedPeer {
    pub fn new(id: impl Into<String>, wg_pubkey: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            wg_pubkey: wg_pubkey.into(),
            mesh_ip: None,
        }
    }

    /// Restrict this authorization to one mesh IPv4 address.
    pub fn with_mesh_ip(mut self, mesh_ip: impl Into<String>) -> Self {
        self.mesh_ip = Some(mesh_ip.into());
        self
    }
}

/// Discovery policy and authorization material.
#[derive(Clone, Debug)]
pub struct DiscoveryConfig {
    /// This node's id; its own advertisements are ignored.
    pub local_id: String,
    /// Static `--mesh-peer` entries. They are authoritative and pre-trusted.
    pub static_peers: Vec<MeshNode>,
    /// Additional key/identity authorizations for discovered peers.
    pub trusted: Vec<TrustedPeer>,
    /// Maximum simultaneously discovered peers.
    pub max_peers: usize,
    /// How long a discovered peer may be absent before removal.
    pub ttl: Duration,
    /// Interval between multicast browse queries.
    pub query_interval: Duration,
}

impl DiscoveryConfig {
    /// Build a configuration with default bounds.
    pub fn new(
        local_id: impl Into<String>,
        static_peers: Vec<MeshNode>,
        trusted: Vec<TrustedPeer>,
    ) -> Self {
        Self {
            local_id: local_id.into(),
            static_peers,
            trusted,
            max_peers: DEFAULT_MAX_PEERS,
            ttl: DEFAULT_DISCOVERY_TTL,
            query_interval: DEFAULT_QUERY_INTERVAL,
        }
    }

    pub fn with_max_peers(mut self, max_peers: usize) -> Self {
        self.max_peers = max_peers;
        self
    }

    pub fn with_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    pub fn with_query_interval(mut self, query_interval: Duration) -> Self {
        self.query_interval = query_interval;
        self
    }

    /// Validate bounds and authorization material.
    pub fn validate(&self) -> Result<()> {
        if self.local_id.trim().is_empty() || self.local_id.len() > MAX_ID_BYTES {
            return Err(MeshError::DiscoveryInvalid(
                "local node id must be non-empty and bounded".to_owned(),
            ));
        }
        if self.max_peers == 0 || self.max_peers > MAX_PEERS {
            return Err(MeshError::DiscoveryInvalid(format!(
                "discovery max_peers must be in 1..={MAX_PEERS}"
            )));
        }
        if self.ttl.is_zero() || self.ttl > Duration::from_secs(3_600) {
            return Err(MeshError::DiscoveryInvalid(
                "discovery TTL must be in 1..=3600 seconds".to_owned(),
            ));
        }
        if self.query_interval.is_zero() || self.query_interval > Duration::from_secs(60) {
            return Err(MeshError::DiscoveryInvalid(
                "discovery query interval must be in 1..=60 seconds".to_owned(),
            ));
        }

        let mut static_ids = BTreeSet::new();
        for peer in &self.static_peers {
            validate_peer_node(peer)?;
            if !static_ids.insert(peer.id.clone()) {
                return Err(MeshError::DiscoveryInvalid(format!(
                    "duplicate static peer id {}",
                    peer.id
                )));
            }
        }

        let mut trusted_ids = BTreeSet::new();
        for trusted in &self.trusted {
            if trusted.id.trim().is_empty() || trusted.id.len() > MAX_ID_BYTES {
                return Err(MeshError::DiscoveryInvalid(
                    "trusted peer id must be non-empty and bounded".to_owned(),
                ));
            }
            WgPublicKey(trusted.wg_pubkey.clone()).to_bytes()?;
            if let Some(mesh_ip) = &trusted.mesh_ip {
                validate_mesh_ip_str(mesh_ip)?;
            }
            if !trusted_ids.insert(trusted.id.clone()) {
                return Err(MeshError::DiscoveryInvalid(format!(
                    "duplicate trusted peer id {}",
                    trusted.id
                )));
            }
        }
        Ok(())
    }
}

/// Describe one advertised peer after validation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredPeer {
    pub node: MeshNode,
    pub capabilities: Vec<String>,
    /// Underlay address observed in the SRV/A records (diagnostics only).
    pub underlay_ip: Option<Ipv4Addr>,
}

/// Raw descriptor produced by an mDNS browse (or a test stream).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RawAdvertisement {
    pub id: Option<String>,
    pub wg_pubkey: Option<String>,
    pub mesh_ip: Option<String>,
    pub wg_port: Option<u16>,
    pub capabilities: Vec<String>,
    pub instance: Option<String>,
    pub underlay_ip: Option<Ipv4Addr>,
    pub source: Option<SocketAddr>,
    pub ttl: Option<Duration>,
}

/// One state-machine transition produced by [`DiscoveryEngine::observe`] or
/// [`DiscoveryEngine::expire`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiscoveryAction {
    /// Authorized peer not seen before.
    Add(DiscoveredPeer),
    /// Authorized peer whose mesh IP or WireGuard endpoint changed.
    Update {
        peer: DiscoveredPeer,
        previous: MeshNode,
    },
    /// Previously discovered peer expired.
    Remove(DiscoveredPeer),
    /// Repeated advertisement for an unchanged peer.
    Unchanged { id: String },
    /// Advertisement exactly matches a static peer; no dynamic peer is added.
    StaticMatched { id: String },
    /// This node's own advertisement.
    IgnoredSelf { id: String },
    /// Malformed advertisement.
    Ignored { id: Option<String>, reason: String },
    /// Advertisement lacks a configured key/host authorization.
    Unauthorized { id: Option<String>, reason: String },
    /// Advertisement conflicts with a static peer or another discovered peer.
    Conflict { id: Option<String>, reason: String },
    /// The configured discovered-peer bound is full.
    LimitReached { id: String, max_peers: usize },
}

/// Counters exposed in node status.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveryCounters {
    pub advertisements: u64,
    pub authorized: u64,
    pub added: u64,
    pub updated: u64,
    pub removed: u64,
    pub duplicates: u64,
    pub expired: u64,
    pub conflicts: u64,
    pub unauthorized: u64,
    pub invalid: u64,
    pub limit_dropped: u64,
    pub self_ignored: u64,
    pub static_matched: u64,
}

/// Read-only snapshot of one discovered peer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredPeerView {
    pub id: String,
    pub mesh_ip: String,
    pub wg_port: u16,
    pub wg_pubkey: String,
    pub capabilities: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub underlay_ip: Option<String>,
    pub age_secs: u64,
    pub expires_in_secs: u64,
}

/// Discovery status document for `/api/v1/connect/status`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveryStatus {
    pub enabled: bool,
    pub local_id: String,
    pub max_peers: usize,
    pub ttl_secs: u64,
    pub static_peer_count: usize,
    pub trusted_key_count: usize,
    pub discovered_count: usize,
    pub peers: Vec<DiscoveredPeerView>,
    pub counters: DiscoveryCounters,
}

#[derive(Clone, Debug)]
struct SeenPeer {
    peer: DiscoveredPeer,
    last_seen: Instant,
}

/// Deterministic discovery state machine.
///
/// The engine performs no I/O. Call [`DiscoveryEngine::observe`] with each
/// browse result and [`DiscoveryEngine::expire`] on a timer; callers apply the
/// returned actions to their peer table and dataplane.
pub struct DiscoveryEngine {
    config: DiscoveryConfig,
    peers: BTreeMap<String, SeenPeer>,
    counters: DiscoveryCounters,
}

impl DiscoveryEngine {
    pub fn new(config: DiscoveryConfig) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            config,
            peers: BTreeMap::new(),
            counters: DiscoveryCounters::default(),
        })
    }

    pub fn config(&self) -> &DiscoveryConfig {
        &self.config
    }

    pub fn counters(&self) -> &DiscoveryCounters {
        &self.counters
    }

    /// Describe the current engine. This is safe to classify as "configured
    /// but not started" until the node service marks it enabled.
    pub fn status(&self, now: Instant) -> DiscoveryStatus {
        let peers = self
            .peers
            .values()
            .map(|seen| {
                let age = now.saturating_duration_since(seen.last_seen);
                DiscoveredPeerView {
                    id: seen.peer.node.id.clone(),
                    mesh_ip: seen.peer.node.mesh_ip.clone(),
                    wg_port: seen.peer.node.wg_port,
                    wg_pubkey: seen.peer.node.wg_pubkey.0.clone(),
                    capabilities: seen.peer.capabilities.clone(),
                    underlay_ip: seen.peer.underlay_ip.map(|ip| ip.to_string()),
                    age_secs: age.as_secs(),
                    expires_in_secs: self.config.ttl.saturating_sub(age).as_secs(),
                }
            })
            .collect();
        DiscoveryStatus {
            enabled: true,
            local_id: self.config.local_id.clone(),
            max_peers: self.config.max_peers,
            ttl_secs: self.config.ttl.as_secs(),
            static_peer_count: self.config.static_peers.len(),
            trusted_key_count: self.config.trusted.len(),
            discovered_count: self.peers.len(),
            peers,
            counters: self.counters.clone(),
        }
    }

    /// Feed one browse result into the state machine.
    pub fn observe(&mut self, raw: RawAdvertisement, now: Instant) -> Vec<DiscoveryAction> {
        self.counters.advertisements += 1;
        let advertisement = match validate_advertisement(&raw) {
            Ok(advertisement) => advertisement,
            Err(error) => {
                self.counters.invalid += 1;
                return vec![DiscoveryAction::Ignored {
                    id: raw.id.clone(),
                    reason: error.to_string(),
                }];
            }
        };
        let id = advertisement.node.id.clone();

        if id == self.config.local_id {
            self.counters.self_ignored += 1;
            return vec![DiscoveryAction::IgnoredSelf { id }];
        }

        // Static configuration is authoritative. An exact match is observed
        // but never duplicated as a dynamic peer.
        if let Some(static_peer) = self
            .config
            .static_peers
            .iter()
            .find(|peer| peer.id == advertisement.node.id)
        {
            if static_peer == &advertisement.node {
                self.counters.static_matched += 1;
                return vec![DiscoveryAction::StaticMatched { id }];
            }
            self.counters.conflicts += 1;
            return vec![DiscoveryAction::Conflict {
                id: Some(id),
                reason: format!(
                    "advertisement conflicts with static --mesh-peer {} \
                     (advertised {} @ {}:{}); static configuration is authoritative",
                    static_peer.id,
                    advertisement.node.wg_pubkey,
                    advertisement.node.mesh_ip,
                    advertisement.node.wg_port
                ),
            }];
        }

        if let Some(owner) = self
            .config
            .static_peers
            .iter()
            .find(|peer| peer.mesh_ip == advertisement.node.mesh_ip)
        {
            self.counters.conflicts += 1;
            return vec![DiscoveryAction::Conflict {
                id: Some(id),
                reason: format!(
                    "advertised mesh IP {} is already owned by static peer {}",
                    advertisement.node.mesh_ip, owner.id
                ),
            }];
        }

        let Some(trusted) = self
            .config
            .trusted
            .iter()
            .find(|trusted| trusted.id == advertisement.node.id)
        else {
            self.counters.unauthorized += 1;
            return vec![DiscoveryAction::Unauthorized {
                id: Some(id),
                reason: "no --mesh-trust entry authorizes this node id/key".to_owned(),
            }];
        };

        if trusted.wg_pubkey != advertisement.node.wg_pubkey.0 {
            self.counters.unauthorized += 1;
            return vec![DiscoveryAction::Unauthorized {
                id: Some(id),
                reason: "advertised public key does not match the configured trust entry"
                    .to_owned(),
            }];
        }
        if let Some(mesh_ip) = &trusted.mesh_ip {
            if mesh_ip != &advertisement.node.mesh_ip {
                self.counters.unauthorized += 1;
                return vec![DiscoveryAction::Unauthorized {
                    id: Some(id),
                    reason: format!(
                        "advertised mesh IP {} does not match the authorized address {mesh_ip}",
                        advertisement.node.mesh_ip
                    ),
                }];
            }
        }
        self.counters.authorized += 1;

        let mesh_ip_owner = self.owner_of(&advertisement.node.mesh_ip);
        if let Some(existing) = self.peers.get_mut(&id) {
            if existing.peer.node.wg_pubkey != advertisement.node.wg_pubkey {
                self.counters.conflicts += 1;
                return vec![DiscoveryAction::Conflict {
                    id: Some(id),
                    reason: "a discovered peer with this id already uses a different public key"
                        .to_owned(),
                }];
            }
            if existing.peer == advertisement {
                existing.last_seen = now;
                self.counters.duplicates += 1;
                return vec![DiscoveryAction::Unchanged { id }];
            }
            if let Some(other) = mesh_ip_owner.as_ref() {
                if other != &id {
                    self.counters.conflicts += 1;
                    return vec![DiscoveryAction::Conflict {
                        id: Some(id),
                        reason: format!(
                            "advertised mesh IP {} is already owned by discovered peer {other}",
                            advertisement.node.mesh_ip
                        ),
                    }];
                }
            }
            let previous = existing.peer.node.clone();
            existing.peer = advertisement.clone();
            existing.last_seen = now;
            self.counters.updated += 1;
            return vec![DiscoveryAction::Update {
                peer: advertisement,
                previous,
            }];
        }

        if self.peers.len() >= self.config.max_peers {
            self.counters.limit_dropped += 1;
            return vec![DiscoveryAction::LimitReached {
                id,
                max_peers: self.config.max_peers,
            }];
        }
        if let Some(other) = self.owner_of(&advertisement.node.mesh_ip) {
            self.counters.conflicts += 1;
            return vec![DiscoveryAction::Conflict {
                id: Some(id),
                reason: format!(
                    "advertised mesh IP {} is already owned by discovered peer {other}",
                    advertisement.node.mesh_ip
                ),
            }];
        }

        self.peers.insert(
            id.clone(),
            SeenPeer {
                peer: advertisement.clone(),
                last_seen: now,
            },
        );
        self.counters.added += 1;
        vec![DiscoveryAction::Add(advertisement)]
    }

    /// Expire peers that have not been advertised within the configured TTL.
    pub fn expire(&mut self, now: Instant) -> Vec<DiscoveryAction> {
        let ttl = self.config.ttl;
        let expired: Vec<String> = self
            .peers
            .iter()
            .filter(|(_, seen)| now.saturating_duration_since(seen.last_seen) >= ttl)
            .map(|(id, _)| id.clone())
            .collect();
        let mut actions = Vec::with_capacity(expired.len());
        for id in expired {
            if let Some(seen) = self.peers.remove(&id) {
                self.counters.expired += 1;
                self.counters.removed += 1;
                actions.push(DiscoveryAction::Remove(seen.peer));
            }
        }
        actions
    }

    /// Forget one peer without counting a network removal.
    ///
    /// Used to roll back a discovered peer whose route could not be programmed,
    /// so a later advertisement is retried instead of being deduplicated.
    pub fn forget(&mut self, id: &str) -> Option<DiscoveredPeer> {
        self.peers.remove(id).map(|seen| seen.peer)
    }

    /// Clear all discovered peers (service stop/restart).
    pub fn clear(&mut self) -> usize {
        let removed = self.peers.len();
        self.peers.clear();
        removed
    }

    pub fn peer(&self, id: &str) -> Option<&DiscoveredPeer> {
        self.peers.get(id).map(|seen| &seen.peer)
    }

    fn owner_of(&self, mesh_ip: &str) -> Option<String> {
        self.peers
            .iter()
            .find(|(_, seen)| seen.peer.node.mesh_ip == mesh_ip)
            .map(|(id, _)| id.clone())
    }
}

// ── Advertisement validation ─────────────────────────────────────────────────

/// Validate the untrusted wire fields of one advertisement.
///
/// This is deliberately independent of authorization: callers must still match
/// the result against [`DiscoveryConfig::static_peers`] / `trusted`.
pub fn validate_advertisement(raw: &RawAdvertisement) -> Result<DiscoveredPeer> {
    let id = raw
        .id
        .as_deref()
        .or(raw.instance.as_deref())
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| {
            MeshError::DiscoveryInvalid("advertisement has no node id in its TXT record".to_owned())
        })?;
    if id.len() > MAX_ID_BYTES {
        return Err(MeshError::DiscoveryInvalid(format!(
            "node id is {} bytes (limit {MAX_ID_BYTES})",
            id.len()
        )));
    }
    if id.chars().any(char::is_control) {
        return Err(MeshError::DiscoveryInvalid(
            "node id contains control characters".to_owned(),
        ));
    }

    let pubkey = raw
        .wg_pubkey
        .as_deref()
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .ok_or_else(|| {
            MeshError::DiscoveryInvalid("advertisement has no WireGuard public key".to_owned())
        })?;
    // Decodes to exactly 32 bytes or returns a typed key error.
    WgPublicKey(pubkey.to_owned()).to_bytes()?;

    let mesh_ip = raw
        .mesh_ip
        .as_deref()
        .map(str::trim)
        .filter(|ip| !ip.is_empty())
        .ok_or_else(|| {
            MeshError::DiscoveryInvalid("advertisement has no mesh IPv4 address".to_owned())
        })?;
    let address = validate_mesh_ip_str(mesh_ip)?;

    let wg_port = raw.wg_port.filter(|port| *port != 0).ok_or_else(|| {
        MeshError::DiscoveryInvalid("advertisement has no usable WireGuard port".to_owned())
    })?;

    let capabilities = normalize_capabilities(&raw.capabilities)?;

    Ok(DiscoveredPeer {
        node: MeshNode {
            id: id.to_owned(),
            wg_pubkey: WgPublicKey(pubkey.to_owned()),
            mesh_ip: address.to_string(),
            wg_port,
        },
        capabilities,
        underlay_ip: raw.underlay_ip,
    })
}

/// Validate a peer identity before it is installed in a tunnel table.
pub fn validate_peer_node(peer: &MeshNode) -> Result<()> {
    if peer.id.trim().is_empty() || peer.id.len() > MAX_ID_BYTES {
        return Err(MeshError::DiscoveryInvalid(
            "static peer id must be non-empty and bounded".to_owned(),
        ));
    }
    WgPublicKey(peer.wg_pubkey.0.clone()).to_bytes()?;
    validate_mesh_ip_str(&peer.mesh_ip)?;
    if peer.wg_port == 0 {
        return Err(MeshError::DiscoveryInvalid(format!(
            "static peer {} has a zero WireGuard port",
            peer.id
        )));
    }
    Ok(())
}

fn validate_mesh_ip_str(value: &str) -> Result<Ipv4Addr> {
    let address = value
        .trim()
        .parse::<Ipv4Addr>()
        .map_err(|_| MeshError::DiscoveryInvalid(format!("invalid mesh IPv4 address {value:?}")))?;
    if !usable_mesh_ipv4(address) {
        return Err(MeshError::DiscoveryInvalid(format!(
            "mesh address {address} is not usable unicast IPv4"
        )));
    }
    Ok(address)
}

fn usable_mesh_ipv4(address: Ipv4Addr) -> bool {
    let octets = address.octets();
    octets[0] != 0
        && octets[0] < 224
        && !address.is_loopback()
        && !address.is_broadcast()
        && address != Ipv4Addr::UNSPECIFIED
}

pub(crate) fn normalize_capabilities(raw: &[String]) -> Result<Vec<String>> {
    let mut normalized = BTreeSet::new();
    for entry in raw {
        for capability in entry.split(',') {
            let capability = capability.trim();
            if capability.is_empty() {
                continue;
            }
            if capability.len() > MAX_CAPABILITY_BYTES {
                return Err(MeshError::DiscoveryInvalid(format!(
                    "capability is {} bytes (limit {MAX_CAPABILITY_BYTES})",
                    capability.len()
                )));
            }
            if capability.chars().any(char::is_control) {
                return Err(MeshError::DiscoveryInvalid(
                    "capability contains control characters".to_owned(),
                ));
            }
            normalized.insert(capability.to_owned());
        }
    }
    if normalized.len() > MAX_CAPABILITIES {
        return Err(MeshError::DiscoveryInvalid(format!(
            "advertisement has {} capabilities (limit {MAX_CAPABILITIES})",
            normalized.len()
        )));
    }
    Ok(normalized.into_iter().collect())
}

// ── mDNS browse client ──────────────────────────────────────────────────────

#[derive(Debug, Default)]
struct PendingAdvertisement {
    txt: BTreeMap<String, String>,
    port: Option<u16>,
    target: Option<String>,
    ttl: Option<u32>,
    updated: Option<Instant>,
    source: Option<SocketAddr>,
}

/// Small blocking mDNS browse client.
///
/// Queries are sent multicast to `224.0.0.251:5353` with the DNS QU
/// (unicast-response) bit set from an ephemeral UDP socket, so the browser does
/// not need to share port 5353 with the local `libmdns` responder.  Responses
/// are accumulated per service instance for a short window because responders
/// may split PTR/SRV/TXT/A records across packets.
pub struct MdnsBrowser {
    socket: UdpSocket,
    query_interval: Duration,
    last_query: Option<Instant>,
    pending: HashMap<String, PendingAdvertisement>,
}

impl MdnsBrowser {
    /// Bind an ephemeral UDP socket for browsing.
    pub fn new(query_interval: Duration) -> io::Result<Self> {
        let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0))?;
        socket.set_read_timeout(Some(BROWSER_READ_TIMEOUT))?;
        socket.set_multicast_ttl_v4(255)?;
        Ok(Self {
            socket,
            query_interval,
            last_query: None,
            pending: HashMap::new(),
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// Send a query if the interval elapsed and drain currently available
    /// responses. Returns complete descriptors only.
    pub fn poll(&mut self) -> Result<Vec<RawAdvertisement>> {
        let now = Instant::now();
        if self
            .last_query
            .is_none_or(|last| now.saturating_duration_since(last) >= self.query_interval)
        {
            self.send_query(now);
        }

        let mut buf = [0u8; 9_000];
        loop {
            match self.socket.recv_from(&mut buf) {
                Ok((length, source)) => match self.absorb_packet(&buf[..length], source, now) {
                    Ok(advertisements) => {
                        if !advertisements.is_empty() {
                            return Ok(advertisements);
                        }
                    }
                    Err(error) => {
                        debug!(%source, %error, "mDNS: ignoring malformed browse response");
                    }
                },
                Err(error)
                    if error.kind() == io::ErrorKind::WouldBlock
                        || error.kind() == io::ErrorKind::TimedOut =>
                {
                    break;
                }
                Err(error) => {
                    return Err(MeshError::Mdns(format!("browse receive: {error}")));
                }
            }
        }

        self.expire_pending(now);
        Ok(Vec::new())
    }

    fn send_query(&mut self, now: Instant) {
        self.last_query = Some(now);
        let query = build_query(MDNS_SERVICE_TYPE);
        if let Err(error) = self
            .socket
            .send_to(&query, SocketAddr::new(IpAddr::V4(MDNS_GROUP), MDNS_PORT))
        {
            debug!(%error, "mDNS: browse query could not be sent");
        }
    }

    fn expire_pending(&mut self, now: Instant) {
        self.pending.retain(|_, pending| {
            pending
                .updated
                .is_none_or(|updated| now.saturating_duration_since(updated) < PENDING_MERGE_WINDOW)
        });
    }

    /// Parse and merge one mDNS response packet. Exposed for tests.
    pub fn absorb_packet(
        &mut self,
        packet: &[u8],
        source: SocketAddr,
        now: Instant,
    ) -> Result<Vec<RawAdvertisement>> {
        let parsed = parse_dns_packet(packet).map_err(|reason| MeshError::Mdns(reason))?;
        if !parsed.response {
            return Ok(Vec::new());
        }

        // A records may arrive before or after SRV; collect them first.
        let mut address_by_target: HashMap<String, Ipv4Addr> = HashMap::new();
        for record in &parsed.records {
            if let RData::A(address) = record.data {
                address_by_target.insert(record.name.to_ascii_lowercase(), address);
            }
        }

        let mut touched = BTreeSet::new();
        for record in &parsed.records {
            match &record.data {
                RData::Ptr(target) => {
                    if let Some(instance) = instance_from_service_name(target) {
                        let pending = self.pending.entry(instance.clone()).or_default();
                        pending.updated = Some(now);
                        pending.source = Some(source);
                        touched.insert(instance);
                    }
                }
                RData::Srv { port, target } => {
                    if let Some(instance) = instance_from_service_name(&record.name) {
                        let pending = self.pending.entry(instance.clone()).or_default();
                        pending.port = Some(*port);
                        pending.target = Some(target.to_ascii_lowercase());
                        pending.ttl = Some(record.ttl);
                        pending.updated = Some(now);
                        pending.source = Some(source);
                        touched.insert(instance);
                    }
                }
                RData::Txt(entries) => {
                    if let Some(instance) = instance_from_service_name(&record.name) {
                        let pending = self.pending.entry(instance.clone()).or_default();
                        for entry in entries {
                            if let Some((key, value)) = entry.split_once('=') {
                                pending
                                    .txt
                                    .insert(key.to_ascii_lowercase(), value.to_owned());
                            }
                        }
                        pending.ttl = Some(record.ttl);
                        pending.updated = Some(now);
                        pending.source = Some(source);
                        touched.insert(instance);
                    }
                }
                RData::A(_) | RData::Other => {}
            }
        }

        let mut advertisements = Vec::new();
        for instance in touched {
            let Some(pending) = self.pending.get(&instance) else {
                continue;
            };
            let Some(mesh_ip) = pending.txt.get("mesh_ip").cloned() else {
                continue;
            };
            let Some(wg_pubkey) = pending
                .txt
                .get("pubkey")
                .or_else(|| pending.txt.get("wg_pubkey"))
                .cloned()
            else {
                continue;
            };
            let Some(wg_port) = pending.port.or_else(|| {
                pending
                    .txt
                    .get("wg_port")
                    .and_then(|port| port.parse().ok())
            }) else {
                continue;
            };
            let id = pending
                .txt
                .get("id")
                .or_else(|| pending.txt.get("node_id"))
                .cloned()
                .or_else(|| instance.split('.').next().map(str::to_owned));
            let capabilities = pending
                .txt
                .get("cap")
                .or_else(|| pending.txt.get("capabilities"))
                .map(|value| {
                    value
                        .split(',')
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            // The response source is the interface address that actually
            // reached us; prefer it over an A record, which may list unrelated
            // bridge addresses on a multi-homed peer.
            let underlay_ip = pending
                .source
                .and_then(|source| match source.ip() {
                    IpAddr::V4(address) => Some(address),
                    IpAddr::V6(_) => None,
                })
                .or_else(|| {
                    pending
                        .target
                        .as_ref()
                        .and_then(|target| address_by_target.get(target).copied())
                });
            advertisements.push(RawAdvertisement {
                id,
                wg_pubkey: Some(wg_pubkey),
                mesh_ip: Some(mesh_ip),
                wg_port: Some(wg_port),
                capabilities,
                instance: Some(instance.clone()),
                underlay_ip,
                source: pending.source,
                ttl: pending.ttl.map(|ttl| Duration::from_secs(u64::from(ttl))),
            });
            self.pending.remove(&instance);
        }
        Ok(advertisements)
    }
}

fn instance_from_service_name(name: &str) -> Option<String> {
    let name = name.trim_end_matches('.').to_ascii_lowercase();
    let suffix = format!(".{MDNS_FQDN}");
    name.strip_suffix(&suffix)
        .filter(|instance| !instance.is_empty())
        .map(str::to_owned)
}

// ── DNS wire parsing ────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
enum RData {
    A(Ipv4Addr),
    Ptr(String),
    Txt(Vec<String>),
    Srv { port: u16, target: String },
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DnsRecord {
    name: String,
    ttl: u32,
    data: RData,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DnsPacket {
    response: bool,
    records: Vec<DnsRecord>,
}

fn parse_dns_packet(packet: &[u8]) -> std::result::Result<DnsPacket, String> {
    if packet.len() < 12 {
        return Err("DNS packet shorter than a header".to_owned());
    }
    let flags = u16::from_be_bytes([packet[2], packet[3]]);
    let response = flags & 0x8000 != 0;
    if flags & 0x0200 != 0 {
        return Err("truncated DNS response is not usable".to_owned());
    }
    let questions = u16::from_be_bytes([packet[4], packet[5]]);
    let answers = u16::from_be_bytes([packet[6], packet[7]]);
    let authority = u16::from_be_bytes([packet[8], packet[9]]);
    let additional = u16::from_be_bytes([packet[10], packet[11]]);
    let total_records = usize::from(answers) + usize::from(authority) + usize::from(additional);
    if total_records > 512 {
        return Err("DNS packet claims too many records".to_owned());
    }

    let mut offset = 12;
    for _ in 0..questions {
        let (_name, next) = read_name(packet, offset)?;
        offset = next;
        if offset + 4 > packet.len() {
            return Err("DNS question is truncated".to_owned());
        }
        offset += 4;
    }

    let mut records = Vec::with_capacity(total_records);
    for _ in 0..total_records {
        let (name, next) = read_name(packet, offset)?;
        offset = next;
        if offset + 10 > packet.len() {
            return Err("DNS resource record header is truncated".to_owned());
        }
        let rtype = u16::from_be_bytes([packet[offset], packet[offset + 1]]);
        let ttl = u32::from_be_bytes([
            packet[offset + 4],
            packet[offset + 5],
            packet[offset + 6],
            packet[offset + 7],
        ]);
        let length = usize::from(u16::from_be_bytes([packet[offset + 8], packet[offset + 9]]));
        offset += 10;
        let end = offset
            .checked_add(length)
            .filter(|end| *end <= packet.len())
            .ok_or_else(|| "DNS record data is truncated".to_owned())?;

        let data = match rtype {
            TYPE_A if length == 4 => RData::A(Ipv4Addr::new(
                packet[offset],
                packet[offset + 1],
                packet[offset + 2],
                packet[offset + 3],
            )),
            TYPE_PTR => {
                let (target, _) = read_name(packet, offset)?;
                RData::Ptr(target)
            }
            TYPE_SRV if length >= 7 => {
                let port = u16::from_be_bytes([packet[offset + 4], packet[offset + 5]]);
                let (target, _) = read_name(packet, offset + 6)?;
                RData::Srv { port, target }
            }
            TYPE_TXT => {
                let mut entries = Vec::new();
                let mut cursor = offset;
                while cursor < end {
                    let entry_len = usize::from(packet[cursor]);
                    cursor += 1;
                    let entry_end = cursor
                        .checked_add(entry_len)
                        .filter(|entry_end| *entry_end <= end)
                        .ok_or_else(|| "TXT entry is truncated".to_owned())?;
                    if let Ok(entry) = std::str::from_utf8(&packet[cursor..entry_end]) {
                        entries.push(entry.to_owned());
                    }
                    cursor = entry_end;
                }
                RData::Txt(entries)
            }
            _ => RData::Other,
        };

        records.push(DnsRecord { name, ttl, data });
        offset = end;
    }

    Ok(DnsPacket { response, records })
}

fn read_name(packet: &[u8], start: usize) -> std::result::Result<(String, usize), String> {
    let mut labels = Vec::new();
    let mut cursor = start;
    let mut next = start;
    let mut jumped = false;
    let mut steps = 0usize;

    loop {
        steps += 1;
        if steps > 128 {
            return Err("DNS name compression loop".to_owned());
        }
        let length = *packet
            .get(cursor)
            .ok_or_else(|| "DNS name is truncated".to_owned())?;
        if length & 0xC0 == 0xC0 {
            let low = *packet
                .get(cursor + 1)
                .ok_or_else(|| "DNS compression pointer is truncated".to_owned())?;
            let pointer = (usize::from(length & 0x3F) << 8) | usize::from(low);
            if pointer >= packet.len() {
                return Err("DNS compression pointer is out of range".to_owned());
            }
            if !jumped {
                next = cursor + 2;
                jumped = true;
            }
            cursor = pointer;
            continue;
        }
        if length & 0xC0 != 0 {
            return Err("invalid DNS label length".to_owned());
        }
        if length == 0 {
            if !jumped {
                next = cursor + 1;
            }
            break;
        }
        let label_end = cursor + 1 + usize::from(length);
        let label = packet
            .get(cursor + 1..label_end)
            .ok_or_else(|| "DNS label is truncated".to_owned())?;
        labels.push(String::from_utf8_lossy(label).into_owned());
        if !jumped {
            next = label_end;
        }
        cursor = label_end;
    }

    Ok((labels.join("."), next))
}

fn build_query(service: &str) -> Vec<u8> {
    let mut packet = Vec::with_capacity(64);
    packet.extend_from_slice(&[0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0]);
    for label in service.trim_end_matches(".local").split('.') {
        let bytes = label.as_bytes();
        packet.push(bytes.len() as u8);
        packet.extend_from_slice(bytes);
    }
    packet.push(5);
    packet.extend_from_slice(b"local");
    packet.push(0);
    packet.extend_from_slice(&TYPE_PTR.to_be_bytes());
    packet.extend_from_slice(&CLASS_IN_QU.to_be_bytes());
    packet
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::MeshNode;

    fn key(seed: u8) -> (MeshNode, String) {
        let (mut node, _private) = MeshNode::generate(format!("node-{seed}"), "10.200.1.1", 0);
        // Regenerate a stable key by varying the local part; each generate is
        // random, so the returned node owns its public key.
        node.wg_pubkey.0 = node.wg_pubkey.0.clone();
        let pubkey = node.wg_pubkey.0.clone();
        (node, pubkey)
    }

    fn advertisement(id: &str, mesh_ip: &str, pubkey: &str) -> RawAdvertisement {
        RawAdvertisement {
            id: Some(id.to_owned()),
            wg_pubkey: Some(pubkey.to_owned()),
            mesh_ip: Some(mesh_ip.to_owned()),
            wg_port: Some(51_820),
            capabilities: vec!["video".to_owned(), "audio".to_owned()],
            ..RawAdvertisement::default()
        }
    }

    fn engine_with(static_peers: Vec<MeshNode>, trusted: Vec<TrustedPeer>) -> DiscoveryEngine {
        DiscoveryEngine::new(
            DiscoveryConfig::new("self", static_peers, trusted)
                .with_ttl(Duration::from_secs(10))
                .with_query_interval(Duration::from_secs(1)),
        )
        .expect("test discovery config")
    }

    #[test]
    fn valid_untrusted_advertisement_is_unauthorized() {
        let (peer, pubkey) = key(1);
        let mut engine = engine_with(Vec::new(), Vec::new());
        let actions = engine.observe(
            advertisement(&peer.id, "10.200.1.2", &pubkey),
            Instant::now(),
        );
        assert!(matches!(
            actions.as_slice(),
            [DiscoveryAction::Unauthorized { .. }]
        ));
        assert_eq!(engine.counters().unauthorized, 1);
        assert_eq!(engine.counters().added, 0);
    }

    #[test]
    fn trusted_advertisement_is_added_and_duplicate_is_deduplicated() {
        let (peer, pubkey) = key(2);
        let mut engine = engine_with(Vec::new(), vec![TrustedPeer::new(&peer.id, &pubkey)]);
        let now = Instant::now();
        let actions = engine.observe(advertisement(&peer.id, "10.200.1.2", &pubkey), now);
        assert!(matches!(actions.as_slice(), [DiscoveryAction::Add(_)]));

        let actions = engine.observe(
            advertisement(&peer.id, "10.200.1.2", &pubkey),
            now + Duration::from_secs(1),
        );
        assert!(matches!(
            actions.as_slice(),
            [DiscoveryAction::Unchanged { .. }]
        ));
        assert_eq!(engine.counters().added, 1);
        assert_eq!(engine.counters().duplicates, 1);
        assert_eq!(engine.status(now).discovered_count, 1);
    }

    #[test]
    fn ttl_expiry_removes_the_peer() {
        let (peer, pubkey) = key(3);
        let mut engine = engine_with(Vec::new(), vec![TrustedPeer::new(&peer.id, &pubkey)]);
        let now = Instant::now();
        engine.observe(advertisement(&peer.id, "10.200.1.2", &pubkey), now);
        assert!(engine.expire(now + Duration::from_secs(9)).is_empty());
        let actions = engine.expire(now + Duration::from_secs(10));
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0], DiscoveryAction::Remove(_)));
        assert_eq!(engine.counters().expired, 1);
        assert_eq!(engine.status(now).discovered_count, 0);
    }

    #[test]
    fn mesh_ip_change_updates_and_reports_previous() {
        let (peer, pubkey) = key(4);
        let mut engine = engine_with(Vec::new(), vec![TrustedPeer::new(&peer.id, &pubkey)]);
        let now = Instant::now();
        engine.observe(advertisement(&peer.id, "10.200.1.2", &pubkey), now);
        let actions = engine.observe(
            advertisement(&peer.id, "10.200.1.3", &pubkey),
            now + Duration::from_secs(1),
        );
        match actions.as_slice() {
            [DiscoveryAction::Update { peer, previous }] => {
                assert_eq!(peer.node.mesh_ip, "10.200.1.3");
                assert_eq!(previous.mesh_ip, "10.200.1.2");
            }
            other => panic!("unexpected actions: {other:?}"),
        }
        assert_eq!(engine.counters().updated, 1);
    }

    #[test]
    fn static_peer_is_authoritative_and_never_overwritten() {
        let (mut peer, pubkey) = key(5);
        peer.mesh_ip = "10.200.1.2".to_owned();
        peer.wg_port = 51_820;
        let mut engine = engine_with(vec![peer.clone()], Vec::new());
        let now = Instant::now();

        let actions = engine.observe(advertisement(&peer.id, "10.200.1.2", &pubkey), now);
        assert!(matches!(
            actions.as_slice(),
            [DiscoveryAction::StaticMatched { .. }]
        ));
        assert_eq!(engine.counters().static_matched, 1);

        let actions = engine.observe(
            advertisement(&peer.id, "10.200.1.99", &pubkey),
            now + Duration::from_secs(1),
        );
        match actions.as_slice() {
            [DiscoveryAction::Conflict { reason, .. }] => {
                assert!(reason.contains("static"), "{reason}");
            }
            other => panic!("unexpected actions: {other:?}"),
        }
        assert_eq!(engine.counters().conflicts, 1);
        assert_eq!(engine.counters().added, 0);
    }

    #[test]
    fn a_second_node_cannot_take_an_owned_mesh_ip() {
        let (first, first_key) = key(6);
        let (second, second_key) = key(7);
        let mut engine = engine_with(
            Vec::new(),
            vec![
                TrustedPeer::new(&first.id, &first_key),
                TrustedPeer::new(&second.id, &second_key),
            ],
        );
        let now = Instant::now();
        engine.observe(advertisement(&first.id, "10.200.1.2", &first_key), now);
        let actions = engine.observe(
            advertisement(&second.id, "10.200.1.2", &second_key),
            now + Duration::from_secs(1),
        );
        assert!(matches!(
            actions.as_slice(),
            [DiscoveryAction::Conflict { .. }]
        ));
        assert_eq!(engine.counters().added, 1);
    }

    #[test]
    fn max_peers_bound_limits_new_entries() {
        let (first, first_key) = key(8);
        let (second, second_key) = key(9);
        let config = DiscoveryConfig::new(
            "self",
            Vec::new(),
            vec![
                TrustedPeer::new(&first.id, &first_key),
                TrustedPeer::new(&second.id, &second_key),
            ],
        )
        .with_max_peers(1);
        let mut engine = DiscoveryEngine::new(config).unwrap();
        let now = Instant::now();
        engine.observe(advertisement(&first.id, "10.200.1.2", &first_key), now);
        let actions = engine.observe(
            advertisement(&second.id, "10.200.1.3", &second_key),
            now + Duration::from_secs(1),
        );
        assert!(matches!(
            actions.as_slice(),
            [DiscoveryAction::LimitReached { max_peers: 1, .. }]
        ));
        assert_eq!(engine.counters().limit_dropped, 1);
    }

    #[test]
    fn malformed_fields_are_typed_invalid() {
        let invalid_key = RawAdvertisement {
            id: Some("peer".to_owned()),
            wg_pubkey: Some("not-base64".to_owned()),
            mesh_ip: Some("10.200.1.2".to_owned()),
            wg_port: Some(51_820),
            ..RawAdvertisement::default()
        };
        assert!(matches!(
            validate_advertisement(&invalid_key),
            Err(MeshError::KeyError(_)) | Err(MeshError::Base64(_))
        ));

        let invalid_ip = RawAdvertisement {
            wg_pubkey: Some(key(10).1),
            mesh_ip: Some("not-an-ip".to_owned()),
            wg_port: Some(51_820),
            id: Some("peer".to_owned()),
            ..RawAdvertisement::default()
        };
        assert!(matches!(
            validate_advertisement(&invalid_ip),
            Err(MeshError::DiscoveryInvalid(_))
        ));

        let zero_port = RawAdvertisement {
            id: Some("peer".to_owned()),
            wg_pubkey: Some(key(10).1),
            mesh_ip: Some("10.200.1.2".to_owned()),
            wg_port: Some(0),
            ..RawAdvertisement::default()
        };
        assert!(matches!(
            validate_advertisement(&zero_port),
            Err(MeshError::DiscoveryInvalid(_))
        ));
    }

    #[test]
    fn own_advertisement_is_ignored() {
        let (peer, pubkey) = key(11);
        let mut engine = engine_with(Vec::new(), vec![TrustedPeer::new(&peer.id, &pubkey)]);
        engine.config.local_id = peer.id.clone();
        let actions = engine.observe(
            advertisement(&peer.id, "10.200.1.2", &pubkey),
            Instant::now(),
        );
        assert!(matches!(
            actions.as_slice(),
            [DiscoveryAction::IgnoredSelf { .. }]
        ));
        assert_eq!(engine.counters().self_ignored, 1);
    }

    // ── DNS wire tests ──────────────────────────────────────────────────────

    fn dns_name(name: &str) -> Vec<u8> {
        let mut out = Vec::new();
        for label in name.trim_end_matches('.').split('.') {
            out.push(label.len() as u8);
            out.extend_from_slice(label.as_bytes());
        }
        out.push(0);
        out
    }

    fn record(name: &str, rtype: u16, ttl: u32, data: &[u8]) -> Vec<u8> {
        let mut out = dns_name(name);
        out.extend_from_slice(&rtype.to_be_bytes());
        out.extend_from_slice(&1u16.to_be_bytes());
        out.extend_from_slice(&ttl.to_be_bytes());
        out.extend_from_slice(&(data.len() as u16).to_be_bytes());
        out.extend_from_slice(data);
        out
    }

    fn response(records: Vec<Vec<u8>>) -> Vec<u8> {
        let mut out = vec![0, 0, 0x84, 0, 0, 0, 0, records.len() as u8, 0, 0, 0, 0];
        for record in records {
            out.extend_from_slice(&record);
        }
        out
    }

    fn txt(entries: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        for entry in entries {
            out.push(entry.len() as u8);
            out.extend_from_slice(entry.as_bytes());
        }
        out
    }

    #[test]
    fn mdn_response_is_assembled_into_a_peer_descriptor() {
        let (peer, pubkey) = key(12);
        let instance = format!("{}.{MDNS_FQDN}", peer.id);
        let target = "peer-host.local";
        let srv = {
            let mut data = vec![0, 0, 0, 0];
            data.extend_from_slice(&51_820u16.to_be_bytes());
            data.extend_from_slice(&dns_name(target));
            data
        };
        let ptr_target = dns_name(&instance);
        let packet = response(vec![
            record(MDNS_FQDN, TYPE_PTR, 120, &ptr_target),
            record(&instance, TYPE_SRV, 120, &srv),
            record(
                &instance,
                TYPE_TXT,
                120,
                &txt(&[
                    &format!("id={}", peer.id),
                    &format!("pubkey={pubkey}"),
                    "mesh_ip=10.200.1.2",
                    "cap=video,audio",
                    "proto=1",
                ]),
            ),
            record(target, TYPE_A, 120, &[10, 10, 10, 2]),
        ]);

        let mut browser = MdnsBrowser::new(Duration::from_secs(5)).expect("browse socket");
        let source: SocketAddr = "10.10.10.2:5353".parse().unwrap();
        let advertisements = browser
            .absorb_packet(&packet, source, Instant::now())
            .expect("parse response");
        assert_eq!(advertisements.len(), 1);
        let advertisement = &advertisements[0];
        assert_eq!(advertisement.id.as_deref(), Some(peer.id.as_str()));
        assert_eq!(advertisement.wg_port, Some(51_820));
        assert_eq!(advertisement.mesh_ip.as_deref(), Some("10.200.1.2"));
        assert_eq!(advertisement.wg_pubkey.as_deref(), Some(pubkey.as_str()));
        assert_eq!(advertisement.capabilities, vec!["video", "audio"]);
        assert_eq!(
            advertisement.underlay_ip,
            Some(Ipv4Addr::new(10, 10, 10, 2))
        );

        let parsed = validate_advertisement(advertisement).expect("validated descriptor");
        assert_eq!(parsed.node.id, peer.id);
        assert_eq!(parsed.capabilities, vec!["audio", "video"]);
    }

    #[test]
    fn truncated_or_malformed_packets_are_rejected_without_panicking() {
        assert!(parse_dns_packet(&[]).is_err());
        assert!(parse_dns_packet(&[0; 11]).is_err());
        // Header claims one answer but no record bytes follow.
        assert!(parse_dns_packet(&[0, 0, 0x84, 0, 0, 0, 0, 1, 0, 0, 0, 0]).is_err());
        // Compression pointer out of range.
        let mut packet = response(vec![]);
        packet[7] = 1;
        packet.extend_from_slice(&[0xC0, 0xFF, 0, 1, 0, 1, 0, 0, 0, 0, 0, 0]);
        assert!(parse_dns_packet(&packet).is_err());
    }

    #[test]
    fn browse_query_uses_the_mesh_service_and_unicast_response_bit() {
        let query = build_query(MDNS_SERVICE_TYPE);
        let (name, offset) = read_name(&query, 12).expect("query name");
        assert_eq!(name, MDNS_FQDN);
        assert_eq!(&query[offset..offset + 2], &TYPE_PTR.to_be_bytes());
        assert_eq!(&query[offset + 2..offset + 4], &CLASS_IN_QU.to_be_bytes());
    }
}
