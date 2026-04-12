// SPDX-License-Identifier: AGPL-3.0-only
//! `ozma-crypto` — encrypted transport layer for Ozma.
//!
//! Ports `controller/transport.py` to Rust.
//!
//! # Wire format (per packet)
//! ```text
//! Byte 0:      Version (0x01)
//! Byte 1:      Packet type  (plaintext, in AAD — not encrypted)
//! Byte 2-9:    Nonce counter (8 bytes, big-endian, monotonic)
//! Byte 10-N:   Ciphertext + 16-byte Poly1305 MAC
//! ```
//!
//! AEAD: XChaCha20-Poly1305
//!   Key:   32-byte symmetric key from session establishment
//!   Nonce: nonce_seed(16 bytes) || counter(8 bytes) = 24 bytes
//!   AAD:   version || packet_type || counter
//!
//! # `no_std` usage
//! Disable the default `std` feature.  The crate requires `alloc`.
//!
//! ```toml
//! ozma-crypto = { version = "0.1", default-features = false }
//! ```

#![cfg_attr(not(feature = "std"), no_std)]
extern crate alloc;

pub mod kdf;
pub mod keys;
pub mod packet;
pub mod replay;
pub mod session;

pub use kdf::derive_session_keys;
pub use keys::{CryptoError, EphemeralKeyPair, IdentityKeyPair};
pub use packet::{decrypt_packet, encrypt_packet, get_counter};
pub use replay::ReplayWindow;
pub use session::SessionState;

// ── Wire-format constants (re-exported for downstream crates) ───────────────
pub use packet::{
    COUNTER_LEN, HEADER_LEN, MAC_LEN, NONCE_LEN, NONCE_SEED_LEN, OVERHEAD, WIRE_VERSION,
};

// ── Packet-type constants ───────────────────────────────────────────────────
pub use packet::{PKT_AUDIO, PKT_CONTROL, PKT_KEYBOARD, PKT_MOUSE, PKT_MOUSE_REL};

// ── Replay-window size constants ────────────────────────────────────────────
pub use replay::{REPLAY_WINDOW_AUDIO, REPLAY_WINDOW_HID};

#[cfg(test)]
mod tests;
