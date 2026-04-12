// SPDX-License-Identifier: AGPL-3.0-only
//! `SessionState` — symmetric state for an active session with a node.
//!
//! Mirrors `SessionState` in `controller/transport.py`.

use alloc::{string::String, vec::Vec};

use crate::{
    kdf::derive_session_keys,
    packet::{decrypt_packet, encrypt_packet, get_counter, PKT_AUDIO, NONCE_SEED_LEN},
    replay::{ReplayWindow, REPLAY_WINDOW_AUDIO, REPLAY_WINDOW_HID},
};

/// Symmetric session state created after a completed DH key exchange.
///
/// Mirrors `SessionState` in `controller/transport.py`.
pub struct SessionState {
    pub node_id: String,
    /// 32-byte key for encrypting packets TO the node.
    pub send_key: [u8; 32],
    /// 32-byte key for decrypting packets FROM the node.
    pub recv_key: [u8; 32],
    /// 16-byte nonce seed combined with counter to form 24-byte XChaCha20 nonce.
    pub nonce_seed: [u8; NONCE_SEED_LEN],
    /// 16-byte session ID for channel-binding verification.
    pub session_id: [u8; 16],
    /// Monotonically increasing send counter.
    pub send_counter: u64,
    hid_replay: ReplayWindow,
    audio_replay: ReplayWindow,
}

impl SessionState {
    /// Create a `SessionState` from already-derived keys.
    pub fn new(
        node_id: String,
        send_key: [u8; 32],
        recv_key: [u8; 32],
        nonce_seed: [u8; NONCE_SEED_LEN],
        session_id: [u8; 16],
    ) -> Self {
        Self {
            node_id,
            send_key,
            recv_key,
            nonce_seed,
            session_id,
            send_counter: 0,
            hid_replay: ReplayWindow::new(REPLAY_WINDOW_HID),
            audio_replay: ReplayWindow::new(REPLAY_WINDOW_AUDIO),
        }
    }

    /// Create a `SessionState` from a completed DH key exchange.
    ///
    /// Mirrors `SessionState.from_dh()` in `controller/transport.py`.
    pub fn from_dh(
        dh_secret: &[u8],
        ctrl_eph_pub: &[u8; 32],
        node_eph_pub: &[u8; 32],
        ctrl_id: &str,
        node_id: &str,
    ) -> Self {
        let (send_key, recv_key, nonce_seed, session_id) =
            derive_session_keys(dh_secret, ctrl_eph_pub, node_eph_pub, ctrl_id, node_id);
        Self::new(node_id.into(), send_key, recv_key, nonce_seed, session_id)
    }

    /// Encrypt a packet for this node and advance the send counter.
    ///
    /// Mirrors `SessionState.encrypt()` in `controller/transport.py`.
    pub fn encrypt(&mut self, packet_type: u8, payload: &[u8]) -> Vec<u8> {
        let pkt = encrypt_packet(
            &self.send_key,
            &self.nonce_seed,
            self.send_counter,
            packet_type,
            payload,
        );
        self.send_counter += 1;
        pkt
    }

    /// Decrypt a packet from this node.
    ///
    /// Returns `(packet_type, payload)` or `None` if authentication fails or
    /// the packet is a replay.
    ///
    /// Mirrors `SessionState.decrypt()` in `controller/transport.py`.
    pub fn decrypt(&mut self, packet: &[u8]) -> Option<(u8, Vec<u8>)> {
        let counter = get_counter(packet)?;

        // Choose replay window based on packet type (peeked from header)
        let pkt_type = *packet.get(1)?;
        let window = if pkt_type == PKT_AUDIO {
            &mut self.audio_replay
        } else {
            &mut self.hid_replay
        };

        if !window.check_and_advance(counter) {
            return None;
        }

        decrypt_packet(&self.recv_key, &self.nonce_seed, packet)
    }
}
