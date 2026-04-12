//! [`MeshNode`] — the identity record for a node in the ozma mesh.

use serde::{Deserialize, Serialize};

use crate::keys::WgPublicKey;

/// A node's identity in the ozma mesh network.
///
/// This is the data exchanged between nodes during peer discovery (mDNS TXT
/// records) and stored by the controller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeshNode {
    /// Stable, human-readable node identifier (e.g. `"living-room-node"`).
    pub id: String,

    /// WireGuard X25519 public key (base64).
    pub wg_pubkey: WgPublicKey,

    /// IPv4 address on the ozma mesh (e.g. `"10.200.1.1"`).
    pub mesh_ip: String,

    /// UDP port the WireGuard endpoint listens on.
    pub wg_port: u16,
}

impl MeshNode {
    /// Create a new [`MeshNode`].
    pub fn new(
        id: impl Into<String>,
        wg_pubkey: WgPublicKey,
        mesh_ip: impl Into<String>,
        wg_port: u16,
    ) -> Self {
        Self {
            id: id.into(),
            wg_pubkey,
            mesh_ip: mesh_ip.into(),
            wg_port,
        }
    }

    /// Serialise to a compact JSON string (used in mDNS TXT records).
    pub fn to_txt(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// Deserialise from a JSON string (mDNS TXT record value).
    pub fn from_txt(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }
}
