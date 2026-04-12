// SPDX-License-Identifier: AGPL-3.0-only
//! HKDF-SHA256 key derivation.
//!
//! Mirrors `hkdf_extract`, `hkdf_expand`, and `derive_session_keys` in
//! `controller/transport.py`.

use alloc::vec::Vec;

use hkdf::Hkdf;
use sha2::{Digest, Sha256};

use crate::packet::NONCE_SEED_LEN;

// ── HKDF primitives ──────────────────────────────────────────────────────────

/// HKDF-Extract: `PRK = HMAC-SHA256(salt, ikm)`.
pub fn hkdf_extract(salt: &[u8], ikm: &[u8]) -> [u8; 32] {
    let (prk, _) = Hkdf::<Sha256>::extract(Some(salt), ikm);
    prk.into()
}

/// HKDF-Expand: derive `length` bytes of key material from `prk`.
pub fn hkdf_expand(prk: &[u8; 32], info: &[u8], length: usize) -> Vec<u8> {
    let hk = Hkdf::<Sha256>::from_prk(prk).expect("PRK is always 32 bytes — length is valid");
    let mut okm = alloc::vec![0u8; length];
    hk.expand(info, &mut okm).expect("HKDF expand length is valid");
    okm
}

// ── Session key derivation ───────────────────────────────────────────────────

/// Derive symmetric session keys from a completed DH exchange.
///
/// Mirrors `derive_session_keys()` in `controller/transport.py`.
///
/// Returns `(send_key, recv_key, nonce_seed, session_id)`:
///
/// | Output     | Length | Purpose                               |
/// |------------|--------|---------------------------------------|
/// | send_key   | 32 B   | Encrypt packets controller → node     |
/// | recv_key   | 32 B   | Decrypt packets node → controller     |
/// | nonce_seed | 16 B   | Combined with counter → 24-byte nonce |
/// | session_id | 16 B   | Channel-binding transcript hash       |
pub fn derive_session_keys(
    dh_secret: &[u8],
    ctrl_eph_pub: &[u8; 32],
    node_eph_pub: &[u8; 32],
    ctrl_id: &str,
    node_id: &str,
) -> ([u8; 32], [u8; 32], [u8; NONCE_SEED_LEN], [u8; 16]) {
    // salt = node_eph_pub || ctrl_eph_pub  (matches Python)
    let mut salt = alloc::vec::Vec::with_capacity(64);
    salt.extend_from_slice(node_eph_pub);
    salt.extend_from_slice(ctrl_eph_pub);

    let prk = hkdf_extract(&salt, dh_secret);

    let ctx = alloc::format!("{ctrl_id}|{node_id}");
    let ctx_bytes = ctx.as_bytes();

    let mut send_info = alloc::vec::Vec::from(&b"ozma-v1|ctrl-to-node|"[..]);
    send_info.extend_from_slice(ctx_bytes);

    let mut recv_info = alloc::vec::Vec::from(&b"ozma-v1|node-to-ctrl|"[..]);
    recv_info.extend_from_slice(ctx_bytes);

    let mut nonce_info = alloc::vec::Vec::from(&b"ozma-v1|nonce-seed|"[..]);
    nonce_info.extend_from_slice(ctx_bytes);

    let send_key: [u8; 32] = hkdf_expand(&prk, &send_info, 32).try_into().unwrap();
    let recv_key: [u8; 32] = hkdf_expand(&prk, &recv_info, 32).try_into().unwrap();
    let nonce_seed: [u8; NONCE_SEED_LEN] =
        hkdf_expand(&prk, &nonce_info, NONCE_SEED_LEN).try_into().unwrap();

    // session_id = SHA-256(ctrl_eph_pub || node_eph_pub || dh_secret)[:16]
    let mut hasher = Sha256::new();
    hasher.update(ctrl_eph_pub);
    hasher.update(node_eph_pub);
    hasher.update(dh_secret);
    let hash = hasher.finalize();
    let session_id: [u8; 16] = hash[..16].try_into().unwrap();

    (send_key, recv_key, nonce_seed, session_id)
}
