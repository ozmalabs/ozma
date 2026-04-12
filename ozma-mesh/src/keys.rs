//! WireGuard X25519 key generation and encoding helpers.

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use x25519_dalek::{PublicKey, StaticSecret};

use crate::error::MeshError;

/// A WireGuard X25519 public key (base64-encoded).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WgPublicKey(pub String);

impl WgPublicKey {
    /// Decode the base64 public key to raw 32 bytes.
    pub fn to_bytes(&self) -> Result<[u8; 32], MeshError> {
        let bytes = B64
            .decode(&self.0)
            .map_err(|e| MeshError::KeyError(format!("base64 decode: {e}")))?;
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

/// A WireGuard X25519 keypair (private key kept in memory only).
#[derive(Clone)]
pub struct WgKeypair {
    secret: StaticSecret,
    /// Base64-encoded public key — safe to share.
    pub public: WgPublicKey,
}

impl WgKeypair {
    /// Generate a fresh keypair using the OS CSPRNG.
    pub fn generate() -> Self {
        let secret = StaticSecret::random_from_rng(OsRng);
        let public_bytes: [u8; 32] = PublicKey::from(&secret).to_bytes();
        let public = WgPublicKey(B64.encode(public_bytes));
        Self { secret, public }
    }

    /// Return the raw 32-byte private key.
    pub fn private_bytes(&self) -> [u8; 32] {
        self.secret.to_bytes()
    }

    /// Return the raw 32-byte public key.
    pub fn public_bytes(&self) -> [u8; 32] {
        PublicKey::from(&self.secret).to_bytes()
    }
}

impl std::fmt::Debug for WgKeypair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WgKeypair")
            .field("public", &self.public)
            .field("secret", &"<redacted>")
            .finish()
    }
}
