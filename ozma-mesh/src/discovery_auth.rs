//! Authenticated mesh advertisements.
//!
//! mDNS records are unauthenticated by themselves.  When an operator configures
//! a shared cluster key (`--mesh-cluster-key-file`), every advertisement is
//! signed with Ed25519 over a canonical payload covering the node id, overlay
//! address, WireGuard public key/port, rotation grace keys, capabilities, a
//! wall-clock timestamp, and a random nonce.  Peers verify the signature with
//! the cluster public key before any peer, route, or tunnel is installed.
//!
//! The signing key is never serialised, logged, or placed in TXT.  Only the
//! public key id (a short SHA-256 fingerprint), timestamp, nonce, and signature
//! are advertised.
//!
//! # Canonical payload
//!
//! ```text
//! ozma-mesh-ad-v1
//! id:<node id>
//! mesh_ip:<overlay IPv4>
//! pubkey:<base64 X25519 public key>
//! wg_port:<u16>
//! prev_pubkey:<sorted comma-separated previous keys>
//! ts:<unix seconds>
//! nonce:<random hex>
//! cap:<sorted comma-separated capabilities>
//! ```
//!
//! Field values are validated before signing/verifying and cannot contain
//! newlines, so the line-oriented encoding is unambiguous.  Capabilities and
//! previous keys are sorted for the canonical form even when a responder
//! happened to advertise them in a different order.

use std::collections::BTreeSet;
use std::fmt;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand::rngs::OsRng;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{MeshError, Result};

/// Default maximum age (and future skew) accepted for a signed advertisement.
pub const DEFAULT_MAX_SKEW: Duration = Duration::from_secs(120);
/// Default interval after which the advertiser re-signs and re-registers.
pub const DEFAULT_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
/// Hard upper bound for the configured skew/age window.
pub const MAX_SKEW: Duration = Duration::from_secs(3_600);
/// Nonce length in bytes (hex-encoded on the wire).
pub const NONCE_BYTES: usize = 16;
/// Maximum accepted nonce text length.
pub const MAX_NONCE_BYTES: usize = 64;
/// Maximum accepted key-id text length.
pub const MAX_KEY_ID_BYTES: usize = 64;
/// Maximum accepted signature text length (base64 of 64 bytes is 88 chars).
pub const MAX_SIGNATURE_BYTES: usize = 128;
/// Domain separator for the canonical advertisement payload.
pub const PAYLOAD_PREFIX: &str = "ozma-mesh-ad-v1";

/// Secret-free description of the authentication fields in one advertisement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdvertisementAuth {
    /// Short public fingerprint of the cluster signing key.
    pub key_id: String,
    /// Unix seconds at signing time.
    pub timestamp: u64,
    /// Random per-advertisement nonce (hex).
    pub nonce: String,
    /// Base64 Ed25519 signature over the canonical payload.
    pub signature: String,
}

/// Why a signed advertisement was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiscoveryAuthError {
    /// Missing, mismatched, or cryptographically invalid signature.
    SignatureInvalid(String),
    /// Timestamp outside the bounded skew window, or a replayed nonce.
    Stale(String),
}

impl fmt::Display for DiscoveryAuthError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SignatureInvalid(reason) | Self::Stale(reason) => formatter.write_str(reason),
        }
    }
}

impl std::error::Error for DiscoveryAuthError {}

impl From<DiscoveryAuthError> for MeshError {
    fn from(error: DiscoveryAuthError) -> Self {
        match error {
            DiscoveryAuthError::SignatureInvalid(reason) => {
                MeshError::DiscoverySignatureInvalid(reason)
            }
            DiscoveryAuthError::Stale(reason) => MeshError::DiscoveryStale(reason),
        }
    }
}

/// Public verification policy for one cluster key.
#[derive(Clone, Debug)]
pub struct DiscoveryAuthConfig {
    /// Short public key id (`sha256(public)[..8]` hex).
    pub key_id: String,
    /// Raw 32-byte Ed25519 public key.
    pub public_key: [u8; 32],
    /// Maximum accepted age/future skew for the signed timestamp.
    pub max_skew: Duration,
}

impl DiscoveryAuthConfig {
    /// Build a verification policy from a raw public key and its key id.
    pub fn new(
        public_key: [u8; 32],
        key_id: impl Into<String>,
        max_skew: Duration,
    ) -> Result<Self> {
        let key_id = key_id.into();
        if key_id.trim().is_empty() || key_id.len() > MAX_KEY_ID_BYTES {
            return Err(MeshError::DiscoveryInvalid(
                "cluster key id must be non-empty and bounded".to_owned(),
            ));
        }
        validate_skew(max_skew)?;
        // Reject a key that is not a canonically decodable Ed25519 point.
        VerifyingKey::from_bytes(&public_key).map_err(|error| {
            MeshError::DiscoveryInvalid(format!("invalid cluster public key: {error}"))
        })?;
        Ok(Self {
            key_id,
            public_key,
            max_skew,
        })
    }

    /// Derive the verification policy from a signing key.
    pub fn from_signing_key(signing_key: &ClusterSigningKey, max_skew: Duration) -> Result<Self> {
        Self::new(signing_key.public_key(), signing_key.key_id(), max_skew)
    }

    /// Verify one advertisement's signature and timestamp.
    ///
    /// The nonce/replay cache is owned by [`crate::discovery::DiscoveryEngine`]
    /// because replay detection needs mutable state shared across observations.
    #[allow(clippy::too_many_arguments)]
    pub fn verify(
        &self,
        id: &str,
        mesh_ip: &str,
        wg_pubkey: &str,
        wg_port: u16,
        previous_pubkeys: &[String],
        capabilities: &[String],
        auth: &AdvertisementAuth,
        now: SystemTime,
    ) -> std::result::Result<(), DiscoveryAuthError> {
        if auth.key_id != self.key_id {
            return Err(DiscoveryAuthError::SignatureInvalid(format!(
                "advertisement key id {:?} does not match the configured cluster key {}",
                truncate(&auth.key_id, MAX_KEY_ID_BYTES),
                self.key_id
            )));
        }
        validate_auth_fields(auth).map_err(DiscoveryAuthError::SignatureInvalid)?;
        validate_timestamp(auth.timestamp, now, self.max_skew)
            .map_err(DiscoveryAuthError::Stale)?;

        let payload = canonical_payload(
            id,
            mesh_ip,
            wg_pubkey,
            wg_port,
            previous_pubkeys,
            capabilities,
            auth.timestamp,
            &auth.nonce,
        );
        let signature =
            decode_signature(&auth.signature).map_err(DiscoveryAuthError::SignatureInvalid)?;
        let verifying_key = VerifyingKey::from_bytes(&self.public_key).map_err(|error| {
            DiscoveryAuthError::SignatureInvalid(format!("invalid cluster public key: {error}"))
        })?;
        verifying_key
            .verify(payload.as_bytes(), &signature)
            .map_err(|_| {
                DiscoveryAuthError::SignatureInvalid(
                    "advertisement signature does not verify against the cluster key".to_owned(),
                )
            })
    }
}

/// Ed25519 cluster signing key loaded from a mode-0600 file.
///
/// `Debug` intentionally prints only the public key id and fingerprint, never
/// private key material.
pub struct ClusterSigningKey {
    signing: SigningKey,
    key_id: String,
}

impl ClusterSigningKey {
    /// Generate a random cluster key (used by tests and first-run setup).
    pub fn generate() -> Self {
        Self::from_signing_key(SigningKey::generate(&mut OsRng))
    }

    /// Build from a 32-byte Ed25519 seed.
    pub fn from_seed(seed: [u8; 32]) -> Self {
        Self::from_signing_key(SigningKey::from_bytes(&seed))
    }

    /// Build from either a 32-byte seed or a 64-byte libsodium private key
    /// (`seed || public_key`).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() == 32 {
            let seed: [u8; 32] = bytes
                .try_into()
                .map_err(|_| MeshError::KeyError("cluster key seed must be 32 bytes".into()))?;
            return Ok(Self::from_seed(seed));
        }
        if bytes.len() == 64 {
            let seed: [u8; 32] = bytes[..32]
                .try_into()
                .map_err(|_| MeshError::KeyError("cluster key seed must be 32 bytes".into()))?;
            let expected: [u8; 32] = bytes[32..].try_into().map_err(|_| {
                MeshError::KeyError("cluster key public half must be 32 bytes".into())
            })?;
            let key = Self::from_seed(seed);
            if key.public_key() != expected {
                return Err(MeshError::KeyError(
                    "cluster key public half does not match its seed".to_owned(),
                ));
            }
            return Ok(key);
        }
        Err(MeshError::KeyError(format!(
            "cluster key must be a 32-byte seed or 64-byte libsodium private key, got {} bytes",
            bytes.len()
        )))
    }

    /// Build from base64 or hex text (32-byte seed or 64-byte libsodium form).
    pub fn from_text(text: &str) -> Result<Self> {
        let text = text.trim();
        if text.is_empty() {
            return Err(MeshError::KeyError("cluster key file is empty".to_owned()));
        }
        if let Some(bytes) = decode_hex(text) {
            return Self::from_bytes(&bytes);
        }
        let bytes = B64
            .decode(text)
            .map_err(|error| MeshError::KeyError(format!("cluster key base64: {error}")))?;
        Self::from_bytes(&bytes)
    }

    /// Load a cluster key from a file.
    ///
    /// On Unix the file must not be readable or writable by group/other
    /// (`0600`/`0400`); a broader mode is a typed error rather than silently
    /// weakening the cluster identity.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        check_key_file_mode(path)?;
        let raw = std::fs::read(path).map_err(|error| {
            MeshError::Io(std::io::Error::new(
                error.kind(),
                format!("read cluster key {}: {error}", path.display()),
            ))
        })?;
        let key = match std::str::from_utf8(&raw) {
            Ok(text)
                if text.chars().all(|character| {
                    !character.is_control()
                        || character == '\n'
                        || character == '\r'
                        || character == '\t'
                }) =>
            {
                // A file may also be raw 32/64-byte key material that happens
                // to be printable; fall back to the binary interpretation.
                Self::from_text(text).or_else(|_| Self::from_bytes(&raw))
            }
            // Raw binary key material straight from a 32/64-byte file.
            _ => Self::from_bytes(&raw),
        };
        key.map_err(|error| {
            MeshError::KeyError(format!("load cluster key {}: {error}", path.display()))
        })
    }

    fn from_signing_key(signing: SigningKey) -> Self {
        let public = signing.verifying_key().to_bytes();
        Self {
            signing,
            key_id: key_id_for(&public),
        }
    }

    /// Raw 32-byte Ed25519 public key.
    pub fn public_key(&self) -> [u8; 32] {
        self.signing.verifying_key().to_bytes()
    }

    /// Short public key id used in TXT and status.
    pub fn key_id(&self) -> &str {
        &self.key_id
    }

    /// Sign the canonical advertisement payload for `now`.
    #[allow(clippy::too_many_arguments)]
    pub fn sign_advertisement(
        &self,
        id: &str,
        mesh_ip: &str,
        wg_pubkey: &str,
        wg_port: u16,
        previous_pubkeys: &[String],
        capabilities: &[String],
        now: SystemTime,
    ) -> AdvertisementAuth {
        self.sign_advertisement_with_nonce(
            id,
            mesh_ip,
            wg_pubkey,
            wg_port,
            previous_pubkeys,
            capabilities,
            unix_seconds(now),
            &random_nonce(),
        )
    }

    /// Sign an advertisement with an explicit timestamp and nonce.
    ///
    /// Production callers use [`ClusterSigningKey::sign_advertisement`], which
    /// picks a fresh nonce.  This explicit form is useful for deterministic
    /// rotation/replay tests.
    #[allow(clippy::too_many_arguments)]
    pub fn sign_advertisement_with_nonce(
        &self,
        id: &str,
        mesh_ip: &str,
        wg_pubkey: &str,
        wg_port: u16,
        previous_pubkeys: &[String],
        capabilities: &[String],
        timestamp: u64,
        nonce: &str,
    ) -> AdvertisementAuth {
        let payload = canonical_payload(
            id,
            mesh_ip,
            wg_pubkey,
            wg_port,
            previous_pubkeys,
            capabilities,
            timestamp,
            nonce,
        );
        let signature = self.signing.sign(payload.as_bytes()).to_bytes();
        AdvertisementAuth {
            key_id: self.key_id.clone(),
            timestamp,
            nonce: nonce.to_owned(),
            signature: B64.encode(signature),
        }
    }

    /// Verification policy for peers using this cluster key.
    pub fn verifier(&self, max_skew: Duration) -> Result<DiscoveryAuthConfig> {
        DiscoveryAuthConfig::from_signing_key(self, max_skew)
    }
}

impl fmt::Debug for ClusterSigningKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClusterSigningKey")
            .field("key_id", &self.key_id)
            .field("public_key", &self.public_key())
            .finish_non_exhaustive()
    }
}

/// Build the canonical signed payload.
///
/// Capabilities and previous public keys are sorted/deduplicated so the same
/// logical advertisement always produces the same bytes regardless of record
/// order.
#[allow(clippy::too_many_arguments)]
pub fn canonical_payload(
    id: &str,
    mesh_ip: &str,
    wg_pubkey: &str,
    wg_port: u16,
    previous_pubkeys: &[String],
    capabilities: &[String],
    timestamp: u64,
    nonce: &str,
) -> String {
    let previous = canonical_list(previous_pubkeys);
    let capabilities = canonical_list(capabilities);
    format!(
        "{PAYLOAD_PREFIX}\nid:{id}\nmesh_ip:{mesh_ip}\npubkey:{wg_pubkey}\n\
         wg_port:{wg_port}\nprev_pubkey:{previous}\nts:{timestamp}\n\
         nonce:{nonce}\ncap:{capabilities}"
    )
}

fn canonical_list(values: &[String]) -> String {
    let mut unique = BTreeSet::new();
    for entry in values {
        for value in entry.split(',') {
            let value = value.trim();
            if !value.is_empty() {
                unique.insert(value.to_owned());
            }
        }
    }
    unique.into_iter().collect::<Vec<_>>().join(",")
}

/// Stable, secret-free key id for a public key.
pub fn key_id_for(public_key: &[u8; 32]) -> String {
    let digest = Sha256::digest(public_key);
    let mut key_id = String::with_capacity(16);
    for byte in &digest[..8] {
        use std::fmt::Write as _;
        let _ = write!(key_id, "{byte:02x}");
    }
    key_id
}

/// Unix seconds, saturating at zero for clocks before the epoch.
pub fn unix_seconds(now: SystemTime) -> u64 {
    now.duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn random_nonce() -> String {
    let mut bytes = [0u8; NONCE_BYTES];
    OsRng.fill_bytes(&mut bytes);
    let mut nonce = String::with_capacity(NONCE_BYTES * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(nonce, "{byte:02x}");
    }
    nonce
}

fn validate_auth_fields(auth: &AdvertisementAuth) -> std::result::Result<(), String> {
    if auth.key_id.is_empty() || auth.key_id.len() > MAX_KEY_ID_BYTES {
        return Err("advertisement key id is missing or too long".to_owned());
    }
    if auth.key_id.chars().any(char::is_control) {
        return Err("advertisement key id contains control characters".to_owned());
    }
    if auth.nonce.is_empty() || auth.nonce.len() > MAX_NONCE_BYTES {
        return Err("advertisement nonce is missing or too long".to_owned());
    }
    if auth.nonce.chars().any(char::is_control) {
        return Err("advertisement nonce contains control characters".to_owned());
    }
    if auth.signature.is_empty() || auth.signature.len() > MAX_SIGNATURE_BYTES {
        return Err("advertisement signature is missing or too long".to_owned());
    }
    Ok(())
}

fn validate_timestamp(
    timestamp: u64,
    now: SystemTime,
    max_skew: Duration,
) -> std::result::Result<(), String> {
    let now = unix_seconds(now);
    let skew = max_skew.as_secs();
    if timestamp > now.saturating_add(skew) {
        return Err(format!(
            "advertisement timestamp {timestamp} is more than {skew}s in the future"
        ));
    }
    if now.saturating_sub(timestamp) > skew {
        return Err(format!(
            "advertisement timestamp {timestamp} is older than the {skew}s freshness window"
        ));
    }
    Ok(())
}

fn validate_skew(max_skew: Duration) -> Result<()> {
    if max_skew.is_zero() || max_skew > MAX_SKEW {
        return Err(MeshError::DiscoveryInvalid(format!(
            "cluster signature skew must be in 1..={} seconds",
            MAX_SKEW.as_secs()
        )));
    }
    Ok(())
}

fn decode_signature(text: &str) -> std::result::Result<Signature, String> {
    if text.len() > MAX_SIGNATURE_BYTES {
        return Err("advertisement signature is too long".to_owned());
    }
    let bytes = B64
        .decode(text)
        .map_err(|error| format!("advertisement signature base64: {error}"))?;
    let bytes: [u8; 64] = bytes
        .try_into()
        .map_err(|_| "advertisement signature must be 64 bytes".to_owned())?;
    Ok(Signature::from_bytes(&bytes))
}

fn decode_hex(text: &str) -> Option<Vec<u8>> {
    // Only seed/private-key sized hex strings are accepted so a base64 key that
    // happens to use only hex characters is not misdetected.
    if !(text.len() == 64 || text.len() == 128)
        || !text.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return None;
    }
    let mut bytes = Vec::with_capacity(text.len() / 2);
    let raw = text.as_bytes();
    for pair in raw.chunks_exact(2) {
        let high = (pair[0] as char).to_digit(16)?;
        let low = (pair[1] as char).to_digit(16)?;
        bytes.push(((high << 4) | low) as u8);
    }
    Some(bytes)
}

fn truncate(value: &str, max: usize) -> &str {
    if value.len() <= max {
        return value;
    }
    let mut end = max;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

#[cfg(unix)]
fn check_key_file_mode(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = std::fs::metadata(path).map_err(|error| {
        MeshError::Io(std::io::Error::new(
            error.kind(),
            format!("stat cluster key {}: {error}", path.display()),
        ))
    })?;
    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(MeshError::KeyError(format!(
            "cluster key {} has mode {mode:04o}; require 0600 (or 0400) so the \
             shared signing key is not group/other accessible",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_key_file_mode(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields() -> (String, String, String, u16, Vec<String>, Vec<String>) {
        (
            "node-a".to_owned(),
            "10.200.1.1".to_owned(),
            B64.encode([7u8; 32]),
            51_820,
            vec![B64.encode([9u8; 32])],
            vec!["audio".to_owned(), "video".to_owned()],
        )
    }

    fn signed(signer: &ClusterSigningKey, now: SystemTime) -> AdvertisementAuth {
        let (id, ip, key, port, previous, capabilities) = fields();
        signer.sign_advertisement(&id, &ip, &key, port, &previous, &capabilities, now)
    }

    #[test]
    fn sign_verify_round_trip() {
        let signer = ClusterSigningKey::generate();
        let verifier = signer.verifier(Duration::from_secs(30)).unwrap();
        let now = SystemTime::now();
        let auth = signed(&signer, now);
        let (id, ip, key, port, previous, capabilities) = fields();
        verifier
            .verify(&id, &ip, &key, port, &previous, &capabilities, &auth, now)
            .expect("signature verifies");
    }

    #[test]
    fn tampered_payload_is_rejected() {
        let signer = ClusterSigningKey::generate();
        let verifier = signer.verifier(Duration::from_secs(30)).unwrap();
        let now = SystemTime::now();
        let auth = signed(&signer, now);
        let (id, _ip, key, port, previous, capabilities) = fields();
        let error = verifier
            .verify(
                &id,
                "10.200.1.99",
                &key,
                port,
                &previous,
                &capabilities,
                &auth,
                now,
            )
            .unwrap_err();
        assert!(matches!(error, DiscoveryAuthError::SignatureInvalid(_)));
    }

    #[test]
    fn tampered_signature_is_rejected() {
        let signer = ClusterSigningKey::generate();
        let verifier = signer.verifier(Duration::from_secs(30)).unwrap();
        let now = SystemTime::now();
        let mut auth = signed(&signer, now);
        auth.signature = B64.encode([0u8; 64]);
        let (id, ip, key, port, previous, capabilities) = fields();
        let error = verifier
            .verify(&id, &ip, &key, port, &previous, &capabilities, &auth, now)
            .unwrap_err();
        assert!(matches!(error, DiscoveryAuthError::SignatureInvalid(_)));
    }

    #[test]
    fn expired_timestamp_is_stale() {
        let signer = ClusterSigningKey::generate();
        let verifier = signer.verifier(Duration::from_secs(10)).unwrap();
        let now = SystemTime::now();
        let auth = signed(&signer, now);
        let (id, ip, key, port, previous, capabilities) = fields();
        let error = verifier
            .verify(
                &id,
                &ip,
                &key,
                port,
                &previous,
                &capabilities,
                &auth,
                now + Duration::from_secs(11),
            )
            .unwrap_err();
        assert!(matches!(error, DiscoveryAuthError::Stale(_)));
    }

    #[test]
    fn future_timestamp_is_stale() {
        let signer = ClusterSigningKey::generate();
        let verifier = signer.verifier(Duration::from_secs(10)).unwrap();
        let now = SystemTime::now();
        let auth = signed(&signer, now);
        let (id, ip, key, port, previous, capabilities) = fields();
        let error = verifier
            .verify(
                &id,
                &ip,
                &key,
                port,
                &previous,
                &capabilities,
                &auth,
                now - Duration::from_secs(11),
            )
            .unwrap_err();
        assert!(matches!(error, DiscoveryAuthError::Stale(_)));
    }

    #[test]
    fn wrong_cluster_key_is_rejected() {
        let signer = ClusterSigningKey::generate();
        let other = ClusterSigningKey::generate();
        let verifier = other.verifier(Duration::from_secs(30)).unwrap();
        let now = SystemTime::now();
        let auth = signed(&signer, now);
        let (id, ip, key, port, previous, capabilities) = fields();
        let error = verifier
            .verify(&id, &ip, &key, port, &previous, &capabilities, &auth, now)
            .unwrap_err();
        assert!(matches!(error, DiscoveryAuthError::SignatureInvalid(_)));
    }

    #[test]
    fn missing_auth_fields_are_rejected() {
        let signer = ClusterSigningKey::generate();
        let verifier = signer.verifier(Duration::from_secs(30)).unwrap();
        let now = SystemTime::now();
        let (id, ip, key, port, previous, capabilities) = fields();
        let auth = AdvertisementAuth {
            key_id: signer.key_id().to_owned(),
            timestamp: unix_seconds(now),
            nonce: String::new(),
            signature: String::new(),
        };
        let error = verifier
            .verify(&id, &ip, &key, port, &previous, &capabilities, &auth, now)
            .unwrap_err();
        assert!(matches!(error, DiscoveryAuthError::SignatureInvalid(_)));
    }

    #[test]
    fn canonical_payload_is_order_independent() {
        let (id, ip, key, port, _, _) = fields();
        let first = canonical_payload(
            &id,
            &ip,
            &key,
            port,
            &[B64.encode([9u8; 32]), B64.encode([8u8; 32])],
            &["video".to_owned(), "audio".to_owned()],
            42,
            "abcd",
        );
        let second = canonical_payload(
            &id,
            &ip,
            &key,
            port,
            &[B64.encode([8u8; 32]), B64.encode([9u8; 32])],
            &["audio".to_owned(), "video".to_owned()],
            42,
            "abcd",
        );
        assert_eq!(first, second);
        assert!(first.len() < 512, "bounded canonical payload");
    }

    #[test]
    fn key_file_round_trip_and_permissions() {
        let dir = std::env::temp_dir().join(format!(
            "ozma-cluster-key-{}-{}",
            std::process::id(),
            unix_seconds(SystemTime::now())
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cluster.key");
        let signer = ClusterSigningKey::generate();
        std::fs::write(&path, B64.encode(signer.signing.to_bytes())).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let loaded = ClusterSigningKey::load(&path).unwrap();
        assert_eq!(loaded.key_id(), signer.key_id());
        assert_eq!(loaded.public_key(), signer.public_key());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            let error = ClusterSigningKey::load(&path).unwrap_err();
            assert!(error.to_string().contains("0600"), "{error}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_or_empty_cluster_key_file_is_typed() {
        let missing = std::env::temp_dir().join(format!(
            "ozma-missing-cluster-key-{}-{}",
            std::process::id(),
            unix_seconds(SystemTime::now())
        ));
        let error = ClusterSigningKey::load(&missing).unwrap_err();
        assert!(
            matches!(error, MeshError::Io(_) | MeshError::KeyError(_)),
            "{error:?}"
        );
        assert!(ClusterSigningKey::from_text("").is_err());
        assert!(ClusterSigningKey::from_bytes(&[1u8; 31]).is_err());
    }

    #[test]
    fn debug_never_prints_private_bytes() {
        let signer = ClusterSigningKey::generate();
        let debug = format!("{signer:?}");
        let secret = B64.encode(signer.signing.to_bytes());
        assert!(!debug.contains(&secret));
        assert!(debug.contains(signer.key_id()));
    }
}
