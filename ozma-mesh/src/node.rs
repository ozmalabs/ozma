//! [`MeshNode`] — identity and addressing for a single ozma mesh peer, plus
//! WireGuard key types and key-generation helpers.

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use x25519_dalek::{PublicKey, StaticSecret};

use crate::error::{MeshError, Result};

// ── Key types ────────────────────────────────────────────────────────────────

/// A WireGuard X25519 public key, stored as base64.
///
/// Never contains secret material — safe to log and serialise.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WgPublicKey(pub String);

impl WgPublicKey {
    /// Decode the base64 public key to raw 32 bytes (needed by boringtun).
    pub fn to_bytes(&self) -> Result<[u8; 32]> {
        let bytes = B64.decode(&self.0)?;
        bytes
            .try_into()
            .map_err(|_| MeshError::KeyError("public key must be 32 bytes".into()))
    }
}

impl std::fmt::Display for WgPublicKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A WireGuard X25519 private key.
///
/// Kept in a newtype so it is never accidentally serialised or logged.
#[derive(Clone)]
pub struct WgPrivateKey(pub(crate) StaticSecret);

impl WgPrivateKey {
    /// Generate a fresh random private key using the OS RNG.
    pub fn generate() -> Self {
        Self(StaticSecret::random_from_rng(OsRng))
    }

    /// Derive the corresponding [`WgPublicKey`].
    pub fn public_key(&self) -> WgPublicKey {
        let pk = PublicKey::from(&self.0);
        WgPublicKey(B64.encode(pk.as_bytes()))
    }

    /// Encode the private key as base64 (for persistent storage).
    pub fn to_base64(&self) -> String {
        B64.encode(self.0.as_bytes())
    }

    /// Restore a private key from base64.
    pub fn from_base64(s: &str) -> Result<Self> {
        let bytes = B64.decode(s)?;
        let arr: [u8; 32] = bytes
            .try_into()
            .map_err(|_| MeshError::KeyError("private key must be 32 bytes".into()))?;
        Ok(Self(StaticSecret::from(arr)))
    }

    /// Return the raw 32-byte secret key material.
    pub(crate) fn to_bytes(&self) -> [u8; 32] {
        *self.0.as_bytes()
    }
}

// ── MeshNode ─────────────────────────────────────────────────────────────────

/// Identity record for a node in the ozma mesh.
///
/// Exchanged during mDNS peer discovery and stored by the controller.
///
/// # Example
///
/// ```rust
/// use ozma_mesh::MeshNode;
///
/// let (node, _sk) = MeshNode::generate("living-room", "10.200.1.1", 51820);
/// println!("{}: {} @ {}", node.id, node.wg_pubkey, node.mesh_ip);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeshNode {
    /// Stable, human-readable node identifier (e.g. `"living-room-node"`).
    pub id: String,

    /// WireGuard X25519 public key (base64).
    pub wg_pubkey: WgPublicKey,

    /// IPv4 address assigned to this node on the ozma mesh (e.g. `"10.200.1.1"`).
    pub mesh_ip: String,

    /// UDP port this node listens on for WireGuard traffic.
    pub wg_port: u16,
}

impl MeshNode {
    /// Create a new [`MeshNode`] with a freshly generated WireGuard key pair.
    ///
    /// Returns `(node, private_key)`.  The caller must keep `private_key`
    /// secret and pass it to [`crate::manager::MeshManager::new`].
    pub fn generate(
        id: impl Into<String>,
        mesh_ip: impl Into<String>,
        wg_port: u16,
    ) -> (Self, WgPrivateKey) {
        let sk = WgPrivateKey::generate();
        let pk = sk.public_key();
        let node = Self {
            id: id.into(),
            wg_pubkey: pk,
            mesh_ip: mesh_ip.into(),
            wg_port,
        };
        (node, sk)
    }

    /// Serialise to a compact JSON string (used in mDNS TXT records).
    pub fn to_txt(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// Deserialise from a JSON string (mDNS TXT record value).
    pub fn from_txt(s: &str) -> std::result::Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }
}
