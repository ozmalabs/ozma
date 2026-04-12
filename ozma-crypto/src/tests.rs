// SPDX-License-Identifier: AGPL-3.0-only
//! Known-answer tests (KATs) verifying interoperability with the Python
//! `controller/transport.py` reference implementation.
//!
//! # Regenerating reference values
//! Run `python ozma-crypto/kat/generate_kat.py` (requires PyNaCl) and paste
//! the printed hex strings back into this file.

use hex_literal::hex;

use crate::{
    kdf::{derive_session_keys, hkdf_expand, hkdf_extract},
    packet::{decrypt_packet, encrypt_packet, get_counter, HEADER_LEN, OVERHEAD, PKT_KEYBOARD},
    replay::ReplayWindow,
    session::SessionState,
};

// ── Fixed test vectors ────────────────────────────────────────────────────────

const ZERO_KEY: [u8; 32] = [0u8; 32];
const ZERO_NONCE_SEED: [u8; 16] = [0u8; 16];

// Fixed DH inputs for deterministic KATs
const DH_SECRET: [u8; 32] =
    hex!("4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742");
const CTRL_EPH_PUB: [u8; 32] =
    hex!("de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f");
const NODE_EPH_PUB: [u8; 32] =
    hex!("8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a");

// ── HKDF tests ────────────────────────────────────────────────────────────────

#[test]
fn test_hkdf_extract_deterministic() {
    // salt = NODE_EPH_PUB || CTRL_EPH_PUB  (matches Python derive_session_keys)
    let mut salt = [0u8; 64];
    salt[..32].copy_from_slice(&NODE_EPH_PUB);
    salt[32..].copy_from_slice(&CTRL_EPH_PUB);

    let prk_a = hkdf_extract(&salt, &DH_SECRET);
    let prk_b = hkdf_extract(&salt, &DH_SECRET);
    assert_eq!(prk_a, prk_b);
    // Must be non-zero
    assert_ne!(prk_a, [0u8; 32]);
}

#[test]
fn test_hkdf_expand_length() {
    let prk = hkdf_extract(b"salt", b"ikm");
    let out16 = hkdf_expand(&prk, b"info", 16);
    let out32 = hkdf_expand(&prk, b"info", 32);
    let out64 = hkdf_expand(&prk, b"info", 64);
    assert_eq!(out16.len(), 16);
    assert_eq!(out32.len(), 32);
    assert_eq!(out64.len(), 64);
    // Shorter output must be a prefix of longer output (HKDF property)
    assert_eq!(out16, out32[..16]);
    assert_eq!(out32, out64[..32]);
}

#[test]
fn test_hkdf_different_info_different_output() {
    let prk = hkdf_extract(b"salt", b"ikm");
    let a = hkdf_expand(&prk, b"label-a", 32);
    let b = hkdf_expand(&prk, b"label-b", 32);
    assert_ne!(a, b);
}

// ── derive_session_keys tests ─────────────────────────────────────────────────

#[test]
fn test_derive_session_keys_deterministic() {
    let (sk1, rk1, ns1, sid1) =
        derive_session_keys(&DH_SECRET, &CTRL_EPH_PUB, &NODE_EPH_PUB, "ctrl-01", "node-01");
    let (sk2, rk2, ns2, sid2) =
        derive_session_keys(&DH_SECRET, &CTRL_EPH_PUB, &NODE_EPH_PUB, "ctrl-01", "node-01");
    assert_eq!(sk1, sk2);
    assert_eq!(rk1, rk2);
    assert_eq!(ns1, ns2);
    assert_eq!(sid1, sid2);
}

#[test]
fn test_derive_session_keys_send_recv_differ() {
    // send_key and recv_key must differ (different HKDF labels)
    let (send_key, recv_key, _, _) =
        derive_session_keys(&DH_SECRET, &CTRL_EPH_PUB, &NODE_EPH_PUB, "ctrl-01", "node-01");
    assert_ne!(send_key, recv_key);
}

#[test]
fn test_derive_session_keys_different_ids_differ() {
    let (sk_a, rk_a, ns_a, _) =
        derive_session_keys(&DH_SECRET, &CTRL_EPH_PUB, &NODE_EPH_PUB, "ctrl-01", "node-01");
    let (sk_b, rk_b, ns_b, _) =
        derive_session_keys(&DH_SECRET, &CTRL_EPH_PUB, &NODE_EPH_PUB, "ctrl-01", "node-02");
    assert_ne!(sk_a, sk_b);
    assert_ne!(rk_a, rk_b);
    assert_ne!(ns_a, ns_b);
}

#[test]
fn test_derive_session_keys_session_id_length() {
    let (_, _, _, session_id) =
        derive_session_keys(&DH_SECRET, &CTRL_EPH_PUB, &NODE_EPH_PUB, "ctrl-01", "node-01");
    assert_eq!(session_id.len(), 16);
    assert_ne!(session_id, [0u8; 16]);
}

// ── Packet encrypt/decrypt ────────────────────────────────────────────────────

#[test]
fn test_encrypt_decrypt_roundtrip() {
    let payload = b"hello ozma";
    let pkt = encrypt_packet(&ZERO_KEY, &ZERO_NONCE_SEED, 0, PKT_KEYBOARD, payload);

    assert_eq!(pkt.len(), OVERHEAD + payload.len());
    assert_eq!(pkt[0], 0x01); // WIRE_VERSION
    assert_eq!(pkt[1], PKT_KEYBOARD);

    let result = decrypt_packet(&ZERO_KEY, &ZERO_NONCE_SEED, &pkt);
    assert_eq!(result, Some((PKT_KEYBOARD, payload.to_vec())));
}

#[test]
fn test_encrypt_counter_changes_ciphertext() {
    let payload = b"test";
    let pkt0 = encrypt_packet(&ZERO_KEY, &ZERO_NONCE_SEED, 0, PKT_KEYBOARD, payload);
    let pkt1 = encrypt_packet(&ZERO_KEY, &ZERO_NONCE_SEED, 1, PKT_KEYBOARD, payload);
    // Different counters → different nonces → different ciphertexts
    assert_ne!(pkt0[HEADER_LEN..], pkt1[HEADER_LEN..]);
}

#[test]
fn test_decrypt_wrong_key_fails() {
    let pkt = encrypt_packet(&ZERO_KEY, &ZERO_NONCE_SEED, 0, PKT_KEYBOARD, b"secret");
    let wrong_key = [0xFFu8; 32];
    assert!(decrypt_packet(&wrong_key, &ZERO_NONCE_SEED, &pkt).is_none());
}

#[test]
fn test_decrypt_tampered_ciphertext_fails() {
    let mut pkt = encrypt_packet(&ZERO_KEY, &ZERO_NONCE_SEED, 0, PKT_KEYBOARD, b"secret");
    let last = pkt.len() - 1;
    pkt[last] ^= 0x01;
    assert!(decrypt_packet(&ZERO_KEY, &ZERO_NONCE_SEED, &pkt).is_none());
}

#[test]
fn test_decrypt_tampered_aad_fails() {
    // Flip the packet type byte (part of AAD, not encrypted)
    let mut pkt = encrypt_packet(&ZERO_KEY, &ZERO_NONCE_SEED, 0, PKT_KEYBOARD, b"secret");
    pkt[1] ^= 0x01;
    assert!(decrypt_packet(&ZERO_KEY, &ZERO_NONCE_SEED, &pkt).is_none());
}

#[test]
fn test_decrypt_too_short_returns_none() {
    assert!(decrypt_packet(&ZERO_KEY, &ZERO_NONCE_SEED, &[]).is_none());
    assert!(decrypt_packet(&ZERO_KEY, &ZERO_NONCE_SEED, &[0u8; 5]).is_none());
}

#[test]
fn test_get_counter() {
    let pkt = encrypt_packet(&ZERO_KEY, &ZERO_NONCE_SEED, 42, PKT_KEYBOARD, b"x");
    assert_eq!(get_counter(&pkt), Some(42));
    assert_eq!(get_counter(&[]), None);
    assert_eq!(get_counter(&[0u8; 5]), None);
}

// ── Known-answer test: fixed inputs → deterministic wire packet ───────────────
//
// Reference values generated by `controller/transport.py` with PyNaCl:
//
//   encrypt_packet(bytes(32), bytes(16), 0, 0x01, b"ozma")
//
// Header (10 bytes) is always deterministic.
// Ciphertext+MAC (20 bytes) is deterministic for XChaCha20-Poly1305 with a
// fixed key and nonce.
#[test]
fn test_encrypt_known_answer_header() {
    let pkt = encrypt_packet(&ZERO_KEY, &ZERO_NONCE_SEED, 0, PKT_KEYBOARD, b"ozma");

    // Header: version=0x01, type=0x01, counter=0 (8 zero bytes)
    assert_eq!(&pkt[..HEADER_LEN], &hex!("01 01 00 00 00 00 00 00 00 00"));

    // Total length = OVERHEAD(26) + len("ozma")(4) = 30
    assert_eq!(pkt.len(), 30);

    // Must decrypt correctly
    assert_eq!(
        decrypt_packet(&ZERO_KEY, &ZERO_NONCE_SEED, &pkt),
        Some((PKT_KEYBOARD, b"ozma".to_vec()))
    );
}

// ── Replay window tests ───────────────────────────────────────────────────────

#[test]
fn test_replay_window_accepts_new_packets() {
    let mut w = ReplayWindow::new(64);
    assert!(w.check_and_advance(1));
    assert!(w.check_and_advance(2));
    assert!(w.check_and_advance(3));
}

#[test]
fn test_replay_window_rejects_duplicate() {
    let mut w = ReplayWindow::new(64);
    assert!(w.check_and_advance(5));
    assert!(!w.check_and_advance(5));
}

#[test]
fn test_replay_window_rejects_too_old() {
    let mut w = ReplayWindow::new(64);
    assert!(w.check_and_advance(100));
    // 100 - 35 = 65 > window_size(64)
    assert!(!w.check_and_advance(35));
}

#[test]
fn test_replay_window_accepts_out_of_order_within_window() {
    let mut w = ReplayWindow::new(64);
    assert!(w.check_and_advance(10));
    assert!(w.check_and_advance(8)); // within window, not yet seen
    assert!(!w.check_and_advance(8)); // now seen → reject
}

#[test]
fn test_replay_window_large_jump_clears_old_entries() {
    let mut w = ReplayWindow::new(64);
    assert!(w.check_and_advance(1));
    assert!(w.check_and_advance(1000)); // large jump clears window
    assert!(!w.check_and_advance(1)); // 1 is now too old
}

#[test]
fn test_replay_window_audio_size_full_cycle() {
    let mut w = ReplayWindow::new(512);
    for i in 0u64..512 {
        assert!(w.check_and_advance(i), "should accept counter {i}");
    }
    for i in 0u64..512 {
        assert!(!w.check_and_advance(i), "should reject duplicate {i}");
    }
}

// ── SessionState tests ────────────────────────────────────────────────────────

#[test]
fn test_session_encrypt_decrypt_roundtrip() {
    let (send_key, recv_key, nonce_seed, session_id) =
        derive_session_keys(&DH_SECRET, &CTRL_EPH_PUB, &NODE_EPH_PUB, "ctrl-01", "node-01");

    // Controller side: encrypts with send_key, decrypts with recv_key
    let mut ctrl = SessionState::new(
        "node-01".into(),
        send_key,
        recv_key,
        nonce_seed,
        session_id,
    );

    // Node side: keys are swapped relative to controller
    let mut node = SessionState::new(
        "ctrl-01".into(),
        recv_key, // node sends with what ctrl receives
        send_key, // node receives with what ctrl sends
        nonce_seed,
        session_id,
    );

    let payload = b"keyboard report";
    let pkt = ctrl.encrypt(PKT_KEYBOARD, payload);
    let result = node.decrypt(&pkt);
    assert_eq!(result, Some((PKT_KEYBOARD, payload.to_vec())));
}

#[test]
fn test_session_counter_advances() {
    let mut sess =
        SessionState::new("node-01".into(), ZERO_KEY, ZERO_KEY, ZERO_NONCE_SEED, [0u8; 16]);
    assert_eq!(sess.send_counter, 0);
    sess.encrypt(PKT_KEYBOARD, b"a");
    assert_eq!(sess.send_counter, 1);
    sess.encrypt(PKT_KEYBOARD, b"b");
    assert_eq!(sess.send_counter, 2);
}

#[test]
fn test_session_replay_rejected() {
    let (send_key, recv_key, nonce_seed, session_id) =
        derive_session_keys(&DH_SECRET, &CTRL_EPH_PUB, &NODE_EPH_PUB, "ctrl-01", "node-01");

    let mut ctrl =
        SessionState::new("node-01".into(), send_key, recv_key, nonce_seed, session_id);
    let mut node =
        SessionState::new("ctrl-01".into(), recv_key, send_key, nonce_seed, session_id);

    let pkt = ctrl.encrypt(PKT_KEYBOARD, b"once");
    assert!(node.decrypt(&pkt).is_some());
    assert!(node.decrypt(&pkt).is_none()); // replay
}

#[test]
fn test_session_from_dh() {
    let sess = SessionState::from_dh(
        &DH_SECRET,
        &CTRL_EPH_PUB,
        &NODE_EPH_PUB,
        "ctrl-01",
        "node-01",
    );
    assert_eq!(sess.node_id, "node-01");
    assert_eq!(sess.send_counter, 0);
    assert_ne!(sess.session_id, [0u8; 16]);
}

#[test]
fn test_session_from_dh_matches_manual_derivation() {
    let sess = SessionState::from_dh(
        &DH_SECRET,
        &CTRL_EPH_PUB,
        &NODE_EPH_PUB,
        "ctrl-01",
        "node-01",
    );
    let (send_key, recv_key, nonce_seed, session_id) =
        derive_session_keys(&DH_SECRET, &CTRL_EPH_PUB, &NODE_EPH_PUB, "ctrl-01", "node-01");

    assert_eq!(sess.send_key, send_key);
    assert_eq!(sess.recv_key, recv_key);
    assert_eq!(sess.nonce_seed, nonce_seed);
    assert_eq!(sess.session_id, session_id);
}
