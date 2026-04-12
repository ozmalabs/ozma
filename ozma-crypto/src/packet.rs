// SPDX-License-Identifier: AGPL-3.0-only
//! Wire-format constants and AEAD encrypt/decrypt functions.
//!
//! Mirrors the packet-level functions in `controller/transport.py`.

use alloc::vec::Vec;

use chacha20poly1305::{
    aead::{Aead, KeyInit, Payload},
    XChaCha20Poly1305, XNonce,
};

// ── Wire-format constants ────────────────────────────────────────────────────

pub const WIRE_VERSION: u8 = 0x01;
pub const NONCE_SEED_LEN: usize = 16;
pub const COUNTER_LEN: usize = 8;
pub const NONCE_LEN: usize = 24; // nonce_seed(16) + counter(8)
pub const MAC_LEN: usize = 16;
pub const HEADER_LEN: usize = 10; // 1 ver + 1 type + 8 counter
pub const OVERHEAD: usize = HEADER_LEN + MAC_LEN; // 26 bytes

// ── Packet-type constants ────────────────────────────────────────────────────

pub const PKT_KEYBOARD: u8 = 0x01;
pub const PKT_MOUSE: u8 = 0x02; // Absolute mouse (x/y 0-32767)
pub const PKT_AUDIO: u8 = 0x03;
pub const PKT_CONTROL: u8 = 0x04;
pub const PKT_MOUSE_REL: u8 = 0x05; // Relative mouse (dx/dy signed 16-bit deltas)

// ── Encrypt ──────────────────────────────────────────────────────────────────

/// Encrypt a packet with XChaCha20-Poly1305 AEAD.
///
/// Returns the complete wire-format packet: `header || ciphertext || MAC`.
///
/// Mirrors `encrypt_packet()` in `controller/transport.py`.
pub fn encrypt_packet(
    key: &[u8; 32],
    nonce_seed: &[u8; NONCE_SEED_LEN],
    counter: u64,
    packet_type: u8,
    payload: &[u8],
) -> Vec<u8> {
    let counter_bytes = counter.to_be_bytes();

    // nonce = nonce_seed(16) || counter(8) = 24 bytes
    let mut nonce_bytes = [0u8; NONCE_LEN];
    nonce_bytes[..NONCE_SEED_LEN].copy_from_slice(nonce_seed);
    nonce_bytes[NONCE_SEED_LEN..].copy_from_slice(&counter_bytes);
    let nonce = XNonce::from(nonce_bytes);

    // AAD = version || packet_type || counter  (matches Python)
    let mut aad = [0u8; HEADER_LEN];
    aad[0] = WIRE_VERSION;
    aad[1] = packet_type;
    aad[2..].copy_from_slice(&counter_bytes);

    let cipher = XChaCha20Poly1305::new(key.into());
    let ciphertext = cipher
        .encrypt(&nonce, Payload { msg: payload, aad: &aad })
        .expect("XChaCha20-Poly1305 encryption is infallible for valid inputs");

    // wire packet = header || ciphertext+MAC
    let mut out = Vec::with_capacity(HEADER_LEN + ciphertext.len());
    out.push(WIRE_VERSION);
    out.push(packet_type);
    out.extend_from_slice(&counter_bytes);
    out.extend_from_slice(&ciphertext);
    out
}

// ── Decrypt ──────────────────────────────────────────────────────────────────

/// Decrypt a packet. Returns `(packet_type, payload)` or `None` if auth fails.
///
/// Does **not** check the replay window — the caller must do that.
///
/// Mirrors `decrypt_packet()` in `controller/transport.py`.
pub fn decrypt_packet(
    key: &[u8; 32],
    nonce_seed: &[u8; NONCE_SEED_LEN],
    packet: &[u8],
) -> Option<(u8, Vec<u8>)> {
    if packet.len() < OVERHEAD {
        return None;
    }

    let version = packet[0];
    if version != WIRE_VERSION {
        return None;
    }

    let packet_type = packet[1];
    let counter_bytes: [u8; 8] = packet[2..10].try_into().ok()?;
    let ciphertext = &packet[HEADER_LEN..];

    // nonce = nonce_seed || counter
    let mut nonce_bytes = [0u8; NONCE_LEN];
    nonce_bytes[..NONCE_SEED_LEN].copy_from_slice(nonce_seed);
    nonce_bytes[NONCE_SEED_LEN..].copy_from_slice(&counter_bytes);
    let nonce = XNonce::from(nonce_bytes);

    // AAD = version || packet_type || counter
    let mut aad = [0u8; HEADER_LEN];
    aad[0] = version;
    aad[1] = packet_type;
    aad[2..].copy_from_slice(&counter_bytes);

    let cipher = XChaCha20Poly1305::new(key.into());
    let plaintext = cipher
        .decrypt(&nonce, Payload { msg: ciphertext, aad: &aad })
        .ok()?;

    Some((packet_type, plaintext))
}

// ── Counter extraction ───────────────────────────────────────────────────────

/// Extract the counter from an encrypted packet without decrypting it.
///
/// Mirrors `get_counter()` in `controller/transport.py`.
pub fn get_counter(packet: &[u8]) -> Option<u64> {
    if packet.len() < HEADER_LEN {
        return None;
    }
    let bytes: [u8; 8] = packet[2..10].try_into().ok()?;
    Some(u64::from_be_bytes(bytes))
}
