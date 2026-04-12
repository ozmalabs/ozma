// SPDX-License-Identifier: AGPL-3.0-only
//! Identity (Ed25519) and ephemeral (X25519) key types.
//!
//! Mirrors `IdentityKeyPair` and `EphemeralKeyPair` in `controller/transport.py`.

use alloc::{string::String, vec::Vec};

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};
use x25519_dalek::{PublicKey as X25519Public, StaticSecret};
use zeroize::Zeroizing;

// ── Error type ───────────────────────────────────────────────────────────────

#[derive(Debug, PartialEq, Eq)]
pub enum CryptoError {
    InvalidKeyLength { expected: usize, got: usize },
    SignatureVerificationFailed,
    DecryptionFailed,
}

#[cfg(feature = "std")]
impl std::fmt::Display for CryptoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CryptoError::InvalidKeyLength { expected, got } => {
                write!(f, "invalid key length: expected {expected}, got {got}")
            }
            CryptoError::SignatureVerificationFailed => {
                write!(f, "signature verification failed")
            }
            CryptoError::DecryptionFailed => write!(f, "decryption failed"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for CryptoError {}

// ── Identity keypair (Ed25519) ───────────────────────────────────────────────

/// Ed25519 signing keypair used as a long-term node/controller identity.
///
/// Mirrors `IdentityKeyPair` in `controller/transport.py`.
pub struct IdentityKeyPair {
    signing_key: SigningKey,
}

impl IdentityKeyPair {
    /// Generate a new random identity keypair.
    pub fn generate<R: rand_core::CryptoRng + rand_core::RngCore>(rng: &mut R) -> Self {
        Self {
            signing_key: SigningKey::generate(rng),
        }
    }

    /// Reconstruct from a 32-byte seed (the private scalar).
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        Self {
            signing_key: SigningKey::from_bytes(seed),
        }
    }

    /// Reconstruct from a 64-byte libsodium-format private key
    /// (first 32 bytes = seed, last 32 bytes = public key).
    ///
    /// Mirrors `IdentityKeyPair.from_private_bytes()` in Python.
    pub fn from_private_bytes(bytes: &[u8]) -> Result<Self, CryptoError> {
        if bytes.len() != 64 {
            return Err(CryptoError::InvalidKeyLength {
                expected: 64,
                got: bytes.len(),
            });
        }
        let seed: &[u8; 32] = bytes[..32].try_into().unwrap();
        Ok(Self::from_seed(seed))
    }

    /// 32-byte Ed25519 public key.
    pub fn public_key(&self) -> [u8; 32] {
        self.signing_key.verifying_key().to_bytes()
    }

    /// 64-byte private key in libsodium format (`seed || public_key`).
    pub fn private_key_bytes(&self) -> Zeroizing<Vec<u8>> {
        let seed = self.signing_key.to_bytes();
        let pk = self.signing_key.verifying_key().to_bytes();
        let mut out = Zeroizing::new(Vec::with_capacity(64));
        out.extend_from_slice(&seed);
        out.extend_from_slice(&pk);
        out
    }

    /// Sign a message. Returns a 64-byte Ed25519 signature.
    ///
    /// Mirrors `IdentityKeyPair.sign()` in Python.
    pub fn sign(&self, message: &[u8]) -> [u8; 64] {
        self.signing_key.sign(message).to_bytes()
    }

    /// Verify an Ed25519 signature against a raw 32-byte public key.
    ///
    /// Mirrors `IdentityKeyPair.verify()` in Python.
    pub fn verify(message: &[u8], signature: &[u8; 64], public_key: &[u8; 32]) -> bool {
        let Ok(vk) = VerifyingKey::from_bytes(public_key) else {
            return false;
        };
        let sig = Signature::from_bytes(signature);
        vk.verify(message, &sig).is_ok()
    }

    /// Human-readable fingerprint: SHA-256 of public key displayed as
    /// space-separated 4-hex-char groups.
    ///
    /// Mirrors `IdentityKeyPair.fingerprint()` in Python.
    pub fn fingerprint(&self) -> String {
        let hash = Sha256::digest(self.public_key());
        // hex-encode all 32 bytes, then take first 32 hex chars (16 bytes),
        // group into 4-char chunks and uppercase — matches Python output.
        let hex: String = hash.iter().map(|b| alloc::format!("{b:02x}")).collect();
        hex[..32]
            .as_bytes()
            .chunks(4)
            .map(|c| {
                let s = core::str::from_utf8(c).unwrap();
                let mut upper = String::with_capacity(4);
                for ch in s.chars() {
                    upper.push(ch.to_ascii_uppercase());
                }
                upper
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

// ── Ephemeral keypair (X25519) ───────────────────────────────────────────────

/// X25519 key-exchange keypair used for session establishment.
///
/// The private key is held as a `StaticSecret` (zeroized on drop) so the
/// public key can be read before the DH step.
///
/// Mirrors `EphemeralKeyPair` in `controller/transport.py`.
pub struct EphemeralKeyPair {
    secret: StaticSecret,
    public: X25519Public,
}

impl EphemeralKeyPair {
    /// Generate a new random ephemeral keypair.
    pub fn generate<R: rand_core::CryptoRng + rand_core::RngCore>(rng: &mut R) -> Self {
        let secret = StaticSecret::random_from_rng(rng);
        let public = X25519Public::from(&secret);
        Self { secret, public }
    }

    /// 32-byte X25519 public key.
    pub fn public_key(&self) -> [u8; 32] {
        self.public.to_bytes()
    }

    /// Compute X25519 shared secret with a peer's public key.
    ///
    /// Mirrors `EphemeralKeyPair.dh()` in Python.
    pub fn dh(&self, peer_public: &[u8; 32]) -> [u8; 32] {
        let peer = X25519Public::from(*peer_public);
        self.secret.diffie_hellman(&peer).to_bytes()
    }
}
