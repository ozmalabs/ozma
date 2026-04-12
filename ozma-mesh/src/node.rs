//! [`MeshNode`] — identity record for a node in the ozma mesh, plus WireGuard
//! key types and key-generation helpers.

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use x25519_dalek::{PublicKey, StaticSecret};

use crate::error::MeshError;

// ── Key types ────────────────────────────────────────────────────────────────

/// A WireGuard X25519 private key.
///
/// Kept in a newtype so it is never accidentally serialised or logged.
#[derive(Clone)]
pub struct WgPrivateKey(pub(crate) StaticSecret);

impl WgPrivateKey {
    /// Raw 32-byte representation (needed by boringtun).
    pub fn to_bytes(&self) -> [u8; 32] {
        self.0.to_bytes()
    }
}

/// A WireGuard X25519 public key, stored as a base64 string.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WgPublicKey(pub String);

impl WgPublicKey {
    /// Decode the base64 public key to raw 32 bytes.
    pub fn to_bytes(&self) -> Result<[u8; 32], MeshError> {
        let bytes = B64
            .decode(&self.0)
            .map_err(|e| MeshError::InvalidKey(e.to_string()))?;
        bytes
            .try_into()
            .map_err(|_| MeshError::InvalidKey("expected 32 bytes".into()))
    }
}

impl std::fmt::Display for WgPublicKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Generate a fresh WireGuard X25519 keypair using the OS RNG.
///
/// Returns `(private_key, public_key)`.
pub fn generate_keypair() -> (WgPrivateKey, WgPublicKey) {
    let secret = StaticSecret::random_from_rng(OsRng);
    let public = PublicKey::from(&secret);
    let pubkey_b64 = B64.encode(public.as_bytes());
    (WgPrivateKey(secret), WgPublicKey(pubkey_b64))
}

// ── MeshNode ─────────────────────────────────────────────────────────────────

/// Identity record for a node in the ozma mesh.
///
/// Exchanged during mDNS peer discovery (as a JSON TXT record) and stored by
/// the controller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeshNode {
    /// Stable, human-readable node identifier (e.g. `"living-room-node"`).
    pub id: String,

    /// WireGuard X25519 public key (base64).
    pub wg_pubkey: WgPublicKey,

    /// IPv4 address assigned to this node on the ozma mesh (e.g. `"10.200.1.1"`).
    pub mesh_ip: String,
}

impl MeshNode {
    /// Serialise to a compact JSON string (used in mDNS TXT records).
    pub fn to_txt(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// Deserialise from a JSON string (mDNS TXT record value).
    pub fn from_txt(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }
}
