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
//! An mDNS record is **unauthenticated by itself** and must never be believed on
//! its own. [`DiscoveryEngine`] authorizes an advertisement by the following
//! precedence:
//!
//! 1. it exactly matches one of the configured static peers (`--mesh-peer`);
//! 2. when a shared cluster key is configured (see
//!    [`crate::discovery_auth`]), it must carry a valid Ed25519 signature, a
//!    fresh timestamp, and a non-replayed nonce (a matching `--mesh-trust`
//!    entry, if present, is still enforced);
//! 3. otherwise its node id and WireGuard public key must match a trusted entry
//!    (`--mesh-trust`), with any configured mesh-IP restriction.
//!
//! Static peers are authoritative: a conflicting advertisement is reported as
//! a conflict and never replaces or adds a peer.  Duplicate advertisements are
//! deduplicated by id and mesh IP, and entries expire after the configured TTL.

use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::hash::{Hash, Hasher};
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::discovery_auth::{
    canonical_payload, AdvertisementAuth, DiscoveryAuthConfig, DiscoveryAuthError,
};
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
    /// Optional shared cluster-key signature verification. When present, a
    /// non-static advertisement must carry a valid signature from this key
    /// before it can be authorized. A matching `trusted` entry, when present,
    /// is still enforced as an additional identity/key/IP restriction.
    pub auth: Option<DiscoveryAuthConfig>,
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
            auth: None,
            max_peers: DEFAULT_MAX_PEERS,
            ttl: DEFAULT_DISCOVERY_TTL,
            query_interval: DEFAULT_QUERY_INTERVAL,
        }
    }

    /// Require advertisements to be signed by this cluster key.
    pub fn with_auth(mut self, auth: DiscoveryAuthConfig) -> Self {
        self.auth = Some(auth);
        self
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
        if let Some(auth) = &self.auth {
            DiscoveryAuthConfig::new(auth.public_key, auth.key_id.clone(), auth.max_skew)?;
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
    /// Public keys the advertiser says it rotated away from, bounded to the
    /// current grace window. Used only to authorize a key change; never
    /// trusted as proof on its own.
    pub previous_pubkeys: Vec<String>,
}

/// Raw descriptor produced by an mDNS browse (or a test stream).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RawAdvertisement {
    pub id: Option<String>,
    pub wg_pubkey: Option<String>,
    pub mesh_ip: Option<String>,
    pub wg_port: Option<u16>,
    pub capabilities: Vec<String>,
    /// Comma-separated `prev_pubkey` TXT value, split by the browser.
    pub previous_pubkeys: Vec<String>,
    /// Cluster signature TXT fields, when the advertiser is signed.
    pub cluster_key_id: Option<String>,
    pub cluster_signature: Option<String>,
    pub timestamp: Option<u64>,
    pub nonce: Option<String>,
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
    /// Authorized peer whose public key changed during a proven grace window.
    KeyRotated {
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
    /// Advertisement is missing a signature or its signature is invalid.
    SignatureInvalid { id: Option<String>, reason: String },
    /// Advertisement timestamp is outside the freshness window or its nonce
    /// was already bound to a different advertisement.
    Stale { id: Option<String>, reason: String },
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
    pub rotated: u64,
    pub removed: u64,
    pub duplicates: u64,
    pub expired: u64,
    pub conflicts: u64,
    pub unauthorized: u64,
    pub signature_invalid: u64,
    pub stale: u64,
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

/// Secret-free cluster-signature policy reported in discovery status.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveryAuthStatus {
    pub enabled: bool,
    /// Public cluster key id; never private key material.
    pub key_id: String,
    pub max_skew_secs: u64,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<DiscoveryAuthStatus>,
    pub discovered_count: usize,
    pub peers: Vec<DiscoveredPeerView>,
    pub counters: DiscoveryCounters,
}

#[derive(Clone, Debug)]
struct SeenPeer {
    peer: DiscoveredPeer,
    last_seen: Instant,
}

/// Bounded cache of nonces already accepted with a signed payload.
#[derive(Clone, Debug)]
struct SeenNonce {
    payload_hash: u64,
    accepted_at: SystemTime,
}

/// Maximum nonces retained by the replay cache.
const MAX_REPLAY_NONCES: usize = 4_096;

/// Deterministic discovery state machine.
///
/// The engine performs no I/O. Call [`DiscoveryEngine::observe`] with each
/// browse result and [`DiscoveryEngine::expire`] on a timer; callers apply the
/// returned actions to their peer table and dataplane.
pub struct DiscoveryEngine {
    config: DiscoveryConfig,
    peers: BTreeMap<String, SeenPeer>,
    counters: DiscoveryCounters,
    /// Nonce -> signed-payload binding. Identical repeats (normal mDNS
    /// re-announcements) are duplicates; a nonce reused for different signed
    /// fields is a replay and is refused.
    replays: HashMap<String, SeenNonce>,
}

impl DiscoveryEngine {
    pub fn new(config: DiscoveryConfig) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            config,
            peers: BTreeMap::new(),
            counters: DiscoveryCounters::default(),
            replays: HashMap::new(),
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
            auth: self.config.auth.as_ref().map(|auth| DiscoveryAuthStatus {
                enabled: true,
                key_id: auth.key_id.clone(),
                max_skew_secs: auth.max_skew.as_secs(),
            }),
            discovered_count: self.peers.len(),
            peers,
            counters: self.counters.clone(),
        }
    }

    /// Feed one browse result into the state machine.
    pub fn observe(&mut self, raw: RawAdvertisement, now: Instant) -> Vec<DiscoveryAction> {
        self.observe_at(raw, now, SystemTime::now())
    }

    /// Like [`DiscoveryEngine::observe`], with an explicit wall clock used for
    /// cluster-signature freshness and nonce replay checks.
    pub fn observe_at(
        &mut self,
        raw: RawAdvertisement,
        now: Instant,
        wall: SystemTime,
    ) -> Vec<DiscoveryAction> {
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

        // Cluster-key authentication is required whenever it is configured.
        // Static peers above are already authoritative out-of-band; this covers
        // every dynamically discovered advertisement.
        if let Some(auth_config) = self.config.auth.clone() {
            match self.verify_signature(&raw, &advertisement, &auth_config, wall) {
                Ok(()) => {}
                Err(DiscoveryAuthError::SignatureInvalid(reason)) => {
                    self.counters.signature_invalid += 1;
                    return vec![DiscoveryAction::SignatureInvalid {
                        id: Some(id),
                        reason,
                    }];
                }
                Err(DiscoveryAuthError::Stale(reason)) => {
                    self.counters.stale += 1;
                    return vec![DiscoveryAction::Stale {
                        id: Some(id),
                        reason,
                    }];
                }
            }
        }

        let trusted_index = self
            .config
            .trusted
            .iter()
            .position(|trusted| trusted.id == advertisement.node.id);
        if let Some(trusted_index) = trusted_index {
            // A matching trust entry is an additional restriction even when a
            // valid cluster signature is present.
            let trusted = &self.config.trusted[trusted_index];
            let key_matches = trusted.wg_pubkey == advertisement.node.wg_pubkey.0;
            let rotated_from_trust = !key_matches
                && advertisement
                    .previous_pubkeys
                    .iter()
                    .any(|key| key == &trusted.wg_pubkey);
            if !key_matches && !rotated_from_trust {
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
            // Adopt the rotated key in memory so a later rotation chain is
            // still authorized. This never rewrites the operator's trust file;
            // a restart falls back to the configured key.
            if rotated_from_trust {
                self.config.trusted[trusted_index].wg_pubkey =
                    advertisement.node.wg_pubkey.0.clone();
            }
        } else if self.config.auth.is_none() {
            // A valid cluster signature authenticates cluster membership even
            // without a per-node --mesh-trust allowlist entry. Without a
            // cluster key the old explicit trust model remains mandatory.
            self.counters.unauthorized += 1;
            return vec![DiscoveryAction::Unauthorized {
                id: Some(id),
                reason: "no --mesh-trust entry authorizes this node id/key".to_owned(),
            }];
        }
        self.counters.authorized += 1;

        let mesh_ip_owner = self.owner_of(&advertisement.node.mesh_ip);
        if let Some(existing) = self.peers.get_mut(&id) {
            let key_changed = existing.peer.node.wg_pubkey != advertisement.node.wg_pubkey;
            if key_changed {
                let authorized_previous = advertisement
                    .previous_pubkeys
                    .iter()
                    .any(|key| key == &existing.peer.node.wg_pubkey.0);
                if !authorized_previous {
                    self.counters.conflicts += 1;
                    return vec![DiscoveryAction::Conflict {
                        id: Some(id),
                        reason:
                            "a discovered peer with this id already uses a different public key"
                                .to_owned(),
                    }];
                }
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
            if key_changed {
                self.counters.rotated += 1;
                return vec![DiscoveryAction::KeyRotated {
                    peer: advertisement,
                    previous,
                }];
            }
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

    /// Verify a signed advertisement and enforce nonce replay protection.
    fn verify_signature(
        &mut self,
        raw: &RawAdvertisement,
        peer: &DiscoveredPeer,
        config: &DiscoveryAuthConfig,
        wall: SystemTime,
    ) -> std::result::Result<(), DiscoveryAuthError> {
        let key_id = raw.cluster_key_id.as_deref().ok_or_else(|| {
            DiscoveryAuthError::SignatureInvalid(
                "cluster key is configured but the advertisement has no key id".to_owned(),
            )
        })?;
        let signature = raw.cluster_signature.as_deref().ok_or_else(|| {
            DiscoveryAuthError::SignatureInvalid(
                "cluster key is configured but the advertisement has no signature".to_owned(),
            )
        })?;
        let timestamp = raw.timestamp.ok_or_else(|| {
            DiscoveryAuthError::SignatureInvalid(
                "cluster key is configured but the advertisement has no timestamp".to_owned(),
            )
        })?;
        let nonce = raw.nonce.as_deref().ok_or_else(|| {
            DiscoveryAuthError::SignatureInvalid(
                "cluster key is configured but the advertisement has no nonce".to_owned(),
            )
        })?;
        let auth = AdvertisementAuth {
            key_id: key_id.to_owned(),
            timestamp,
            nonce: nonce.to_owned(),
            signature: signature.to_owned(),
        };
        config.verify(
            &peer.node.id,
            &peer.node.mesh_ip,
            &peer.node.wg_pubkey.0,
            peer.node.wg_port,
            &peer.previous_pubkeys,
            &peer.capabilities,
            &auth,
            wall,
        )?;

        // Signature and freshness are valid. The same nonce with the same
        // signed payload is a normal mDNS re-announcement (the engine dedupes
        // it); the same nonce bound to different fields is a replay.
        let payload = canonical_payload(
            &peer.node.id,
            &peer.node.mesh_ip,
            &peer.node.wg_pubkey.0,
            peer.node.wg_port,
            &peer.previous_pubkeys,
            &peer.capabilities,
            timestamp,
            nonce,
        );
        let mut hasher = DefaultHasher::new();
        payload.hash(&mut hasher);
        let payload_hash = hasher.finish();
        if let Some(seen) = self.replays.get(nonce) {
            if seen.payload_hash != payload_hash {
                return Err(DiscoveryAuthError::Stale(format!(
                    "advertisement nonce {nonce} was already used for a different signed payload"
                )));
            }
            return Ok(());
        }
        self.prune_replays(wall, config.max_skew);
        if self.replays.len() >= MAX_REPLAY_NONCES {
            if let Some(oldest) = self
                .replays
                .iter()
                .min_by_key(|(_, seen)| seen.accepted_at)
                .map(|(nonce, _)| nonce.clone())
            {
                self.replays.remove(&oldest);
            }
        }
        self.replays.insert(
            nonce.to_owned(),
            SeenNonce {
                payload_hash,
                accepted_at: wall,
            },
        );
        Ok(())
    }

    fn prune_replays(&mut self, now: SystemTime, max_skew: Duration) {
        let window = max_skew.saturating_mul(2);
        self.replays.retain(|_, seen| {
            now.duration_since(seen.accepted_at)
                .map_or(true, |age| age <= window)
        });
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
    let previous_pubkeys = normalize_previous_pubkeys(&raw.previous_pubkeys, pubkey)?;

    Ok(DiscoveredPeer {
        node: MeshNode {
            id: id.to_owned(),
            wg_pubkey: WgPublicKey(pubkey.to_owned()),
            mesh_ip: address.to_string(),
            wg_port,
        },
        capabilities,
        underlay_ip: raw.underlay_ip,
        previous_pubkeys,
    })
}

/// Bound and validate `prev_pubkey` grace entries.
pub(crate) fn normalize_previous_pubkeys(raw: &[String], current: &str) -> Result<Vec<String>> {
    let mut keys = Vec::new();
    for entry in raw {
        for key in entry.split(',') {
            let key = key.trim();
            if key.is_empty() {
                continue;
            }
            if keys.len() >= 2 {
                return Err(MeshError::DiscoveryInvalid(
                    "advertisement lists more than two previous public keys".to_owned(),
                ));
            }
            if key == current {
                return Err(MeshError::DiscoveryInvalid(
                    "advertisement lists its current public key as previous".to_owned(),
                ));
            }
            WgPublicKey(key.to_owned()).to_bytes()?;
            if !keys.iter().any(|existing| existing == key) {
                keys.push(key.to_owned());
            }
        }
    }
    Ok(keys)
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
            let previous_pubkeys = pending
                .txt
                .get("prev_pubkey")
                .map(|value| {
                    value
                        .split(',')
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            let cluster_key_id = pending
                .txt
                .get("key_id")
                .or_else(|| pending.txt.get("cluster_key_id"))
                .cloned();
            let cluster_signature = pending
                .txt
                .get("sig")
                .or_else(|| pending.txt.get("signature"))
                .cloned();
            let timestamp = pending
                .txt
                .get("ts")
                .or_else(|| pending.txt.get("timestamp"))
                .and_then(|value| value.parse::<u64>().ok());
            let nonce = pending.txt.get("nonce").cloned();
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
                previous_pubkeys,
                cluster_key_id,
                cluster_signature,
                timestamp,
                nonce,
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
    use crate::discovery_auth::ClusterSigningKey;
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

    // ── Cluster signature tests ─────────────────────────────────────────────

    fn auth_engine(signer: &ClusterSigningKey, trusted: Vec<TrustedPeer>) -> DiscoveryEngine {
        DiscoveryEngine::new(
            DiscoveryConfig::new("self", Vec::new(), trusted)
                .with_ttl(Duration::from_secs(10))
                .with_query_interval(Duration::from_secs(1))
                .with_auth(signer.verifier(Duration::from_secs(30)).unwrap()),
        )
        .expect("signed discovery config")
    }

    #[allow(clippy::too_many_arguments)]
    fn signed_advertisement(
        signer: &ClusterSigningKey,
        peer: &MeshNode,
        mesh_ip: &str,
        previous: &[String],
        capabilities: &[String],
        wall: SystemTime,
        nonce: &str,
    ) -> RawAdvertisement {
        let port = 51_820;
        let auth = signer.sign_advertisement_with_nonce(
            &peer.id,
            mesh_ip,
            &peer.wg_pubkey.0,
            port,
            previous,
            capabilities,
            crate::discovery_auth::unix_seconds(wall),
            nonce,
        );
        RawAdvertisement {
            id: Some(peer.id.clone()),
            wg_pubkey: Some(peer.wg_pubkey.0.clone()),
            mesh_ip: Some(mesh_ip.to_owned()),
            wg_port: Some(port),
            capabilities: capabilities.to_vec(),
            previous_pubkeys: previous.to_vec(),
            cluster_key_id: Some(auth.key_id),
            cluster_signature: Some(auth.signature),
            timestamp: Some(auth.timestamp),
            nonce: Some(auth.nonce),
            ..RawAdvertisement::default()
        }
    }

    #[test]
    fn cluster_key_requires_a_signature() {
        let signer = ClusterSigningKey::generate();
        let (peer, pubkey) = key(20);
        let mut engine = auth_engine(&signer, Vec::new());
        let actions = engine.observe(
            advertisement(&peer.id, "10.200.1.2", &pubkey),
            Instant::now(),
        );
        assert!(matches!(
            actions.as_slice(),
            [DiscoveryAction::SignatureInvalid { .. }]
        ));
        assert_eq!(engine.counters().signature_invalid, 1);
        assert_eq!(engine.counters().added, 0);
    }

    #[test]
    fn signed_advertisement_adds_without_a_trust_entry() {
        let signer = ClusterSigningKey::generate();
        let (peer, _) = key(21);
        let mut engine = auth_engine(&signer, Vec::new());
        let wall = SystemTime::now();
        let now = Instant::now();
        let raw = signed_advertisement(
            &signer,
            &peer,
            "10.200.1.2",
            &[],
            &["video".to_owned(), "audio".to_owned()],
            wall,
            "00112233445566778899aabbccddeeff",
        );
        let actions = engine.observe_at(raw.clone(), now, wall);
        assert!(matches!(actions.as_slice(), [DiscoveryAction::Add(_)]));
        assert_eq!(engine.counters().authorized, 1);

        // An identical mDNS re-announcement (same nonce/payload) is a
        // duplicate, not a replay.
        let actions = engine.observe_at(raw, now + Duration::from_secs(1), wall);
        assert!(matches!(
            actions.as_slice(),
            [DiscoveryAction::Unchanged { .. }]
        ));
        assert_eq!(engine.counters().stale, 0);
    }

    #[test]
    fn signed_advertisement_with_tampered_payload_is_rejected() {
        let signer = ClusterSigningKey::generate();
        let (peer, _) = key(22);
        let mut engine = auth_engine(&signer, Vec::new());
        let wall = SystemTime::now();
        let mut raw = signed_advertisement(
            &signer,
            &peer,
            "10.200.1.2",
            &[],
            &["video".to_owned()],
            wall,
            "ffeeddccbbaa99887766554433221100",
        );
        raw.mesh_ip = Some("10.200.1.99".to_owned());
        let actions = engine.observe_at(raw, Instant::now(), wall);
        assert!(matches!(
            actions.as_slice(),
            [DiscoveryAction::SignatureInvalid { .. }]
        ));
        assert_eq!(engine.counters().signature_invalid, 1);
    }

    #[test]
    fn signed_advertisement_with_wrong_cluster_key_is_rejected() {
        let signer = ClusterSigningKey::generate();
        let other = ClusterSigningKey::generate();
        let (peer, _) = key(23);
        let mut engine = auth_engine(&signer, Vec::new());
        let wall = SystemTime::now();
        let raw = signed_advertisement(
            &other,
            &peer,
            "10.200.1.2",
            &[],
            &[],
            wall,
            "0123456789abcdef0123456789abcdef",
        );
        let actions = engine.observe_at(raw, Instant::now(), wall);
        assert!(matches!(
            actions.as_slice(),
            [DiscoveryAction::SignatureInvalid { .. }]
        ));
        assert_eq!(engine.counters().signature_invalid, 1);
    }

    #[test]
    fn signed_advertisement_with_expired_timestamp_is_stale() {
        let signer = ClusterSigningKey::generate();
        let (peer, _) = key(24);
        let mut engine = auth_engine(&signer, Vec::new());
        let wall = SystemTime::now();
        let raw = signed_advertisement(
            &signer,
            &peer,
            "10.200.1.2",
            &[],
            &[],
            wall,
            "11112222333344445555666677778888",
        );
        let actions = engine.observe_at(raw, Instant::now(), wall + Duration::from_secs(31));
        assert!(matches!(
            actions.as_slice(),
            [DiscoveryAction::Stale { .. }]
        ));
        assert_eq!(engine.counters().stale, 1);
    }

    #[test]
    fn replayed_nonce_with_different_payload_is_stale() {
        let signer = ClusterSigningKey::generate();
        let (peer, _) = key(25);
        let mut engine = auth_engine(&signer, Vec::new());
        let wall = SystemTime::now();
        let nonce = "aaaabbbbccccddddeeeeffff00001111";
        let first = signed_advertisement(&signer, &peer, "10.200.1.2", &[], &[], wall, nonce);
        assert!(matches!(
            engine.observe_at(first, Instant::now(), wall).as_slice(),
            [DiscoveryAction::Add(_)]
        ));

        // A second, correctly signed advertisement reuses the same nonce but
        // changes the payload. This is a replay, not a normal duplicate.
        let second = signed_advertisement(&signer, &peer, "10.200.1.3", &[], &[], wall, nonce);
        let actions = engine.observe_at(second, Instant::now(), wall);
        assert!(matches!(
            actions.as_slice(),
            [DiscoveryAction::Stale { .. }]
        ));
        assert_eq!(engine.counters().stale, 1);
    }

    #[test]
    fn signed_rotation_advertisement_covers_previous_key() {
        let signer = ClusterSigningKey::generate();
        let old_public = key(26).1;
        let (peer, _) = key(27);
        let mut engine = auth_engine(&signer, vec![TrustedPeer::new(&peer.id, &old_public)]);
        let wall = SystemTime::now();
        let raw = signed_advertisement(
            &signer,
            &peer,
            "10.200.1.2",
            &[old_public.clone()],
            &["video".to_owned()],
            wall,
            "55556666777788889999000011112222",
        );
        let actions = engine.observe_at(raw.clone(), Instant::now(), wall);
        assert!(matches!(actions.as_slice(), [DiscoveryAction::Add(_)]));

        // Altering the grace list breaks the signature over the whole payload.
        let mut tampered = raw;
        tampered.previous_pubkeys.clear();
        let actions = engine.observe_at(tampered, Instant::now(), wall + Duration::from_secs(1));
        assert!(matches!(
            actions.as_slice(),
            [DiscoveryAction::SignatureInvalid { .. }]
        ));
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
    fn key_rotation_requires_the_previous_trusted_key() {
        let (peer, old_pubkey) = key(30);
        let (_, new_pubkey) = key(31);
        let mut engine = engine_with(vec![], vec![TrustedPeer::new(&peer.id, &old_pubkey)]);
        let now = Instant::now();

        let first = engine.observe(advertisement(&peer.id, &peer.mesh_ip, &old_pubkey), now);
        assert!(matches!(first.as_slice(), [DiscoveryAction::Add(_)]));

        let mut unrelated = advertisement(&peer.id, &peer.mesh_ip, &new_pubkey);
        let actions = engine.observe(unrelated.clone(), now);
        assert!(
            matches!(actions.as_slice(), [DiscoveryAction::Unauthorized { .. }]),
            "a key change without a previous key is not authorized: {actions:?}"
        );

        unrelated.previous_pubkeys = vec![old_pubkey.clone()];
        let actions = engine.observe(unrelated, now);
        assert!(
            matches!(actions.as_slice(), [DiscoveryAction::KeyRotated { .. }]),
            "a grace advertisement must rotate the peer: {actions:?}"
        );
        assert_eq!(engine.counters().rotated, 1);
        assert_eq!(engine.peer(&peer.id).unwrap().node.wg_pubkey.0, new_pubkey);

        // The in-memory trust entry follows the rotation, so a later
        // advertisement of the new key needs no previous-key field. Dropping
        // the grace field is an ordinary update, not another conflict.
        let actions = engine.observe(advertisement(&peer.id, &peer.mesh_ip, &new_pubkey), now);
        assert!(
            matches!(actions.as_slice(), [DiscoveryAction::Update { .. }]),
            "an advertisement without the grace field updates the peer: {actions:?}"
        );
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
