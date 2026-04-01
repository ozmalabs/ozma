# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Encrypted transport — AEAD encrypted packets between controller and nodes.

Implements the security architecture from 17-security-architecture.md (v2).

Wire format (per packet):
  Byte 0:      Version (0x01)
  Byte 1:      Packet type (plaintext, in AAD — not encrypted)
  Byte 2-9:    Nonce counter (8 bytes, big-endian, monotonic)
  Byte 10-N:   Ciphertext + 16-byte Poly1305 MAC

AEAD: XChaCha20-Poly1305 (libsodium crypto_aead_xchacha20poly1305_ietf)
  Key:    32-byte symmetric key from session establishment
  Nonce:  nonce_seed(16 bytes) || counter(8 bytes) = 24 bytes
  AAD:    version || packet_type || counter
  Plain:  payload bytes

Overhead: 26 bytes per packet (1 ver + 1 type + 8 counter + 16 MAC).

This module is designed for easy porting to Rust (sodiumoxide crate).
All crypto operations are stateless functions operating on bytes.
No classes with hidden state, no inheritance, no magic.
"""

from __future__ import annotations

import hashlib
import hmac
import os
import struct
from dataclasses import dataclass

# Try to import libsodium via PyNaCl
_HAS_NACL = False
try:
    import nacl.bindings
    _HAS_NACL = True
except ImportError:
    pass

WIRE_VERSION = 0x01
NONCE_SEED_LEN = 16
COUNTER_LEN = 8
NONCE_LEN = 24  # nonce_seed(16) + counter(8)
MAC_LEN = 16
HEADER_LEN = 10  # 1 ver + 1 type + 8 counter
OVERHEAD = HEADER_LEN + MAC_LEN  # 26 bytes

# Packet types
PKT_KEYBOARD = 0x01
PKT_MOUSE = 0x02
PKT_AUDIO = 0x03
PKT_CONTROL = 0x04

# Replay window sizes (per stream type)
REPLAY_WINDOW_HID = 64
REPLAY_WINDOW_AUDIO = 512


# ── Key types ───────────────────────────────────────────────────────────────

@dataclass(frozen=True)
class IdentityKeyPair:
    """Ed25519 signing keypair."""
    public_key: bytes   # 32 bytes
    private_key: bytes  # 64 bytes (libsodium format: seed + public)

    @staticmethod
    def generate() -> "IdentityKeyPair":
        if _HAS_NACL:
            pk, sk = nacl.bindings.crypto_sign_keypair()
            return IdentityKeyPair(public_key=pk, private_key=sk)
        raise RuntimeError("PyNaCl required for key generation")

    def sign(self, message: bytes) -> bytes:
        """Sign a message. Returns 64-byte Ed25519 signature."""
        if _HAS_NACL:
            # crypto_sign returns signature(64) + message
            signed = nacl.bindings.crypto_sign(message, self.private_key)
            return signed[:64]
        raise RuntimeError("PyNaCl required")

    @staticmethod
    def verify(message: bytes, signature: bytes, public_key: bytes) -> bool:
        """Verify an Ed25519 signature."""
        if _HAS_NACL:
            try:
                signed = signature + message
                nacl.bindings.crypto_sign_open(signed, public_key)
                return True
            except Exception:
                return False
        raise RuntimeError("PyNaCl required")

    def fingerprint(self) -> str:
        """Human-readable fingerprint: SHA256 of public key, displayed as hex groups."""
        h = hashlib.sha256(self.public_key).hexdigest()
        return " ".join(h[i:i+4].upper() for i in range(0, 32, 4))


@dataclass(frozen=True)
class EphemeralKeyPair:
    """X25519 key exchange keypair."""
    public_key: bytes   # 32 bytes
    private_key: bytes  # 32 bytes

    @staticmethod
    def generate() -> "EphemeralKeyPair":
        if _HAS_NACL:
            pk, sk = nacl.bindings.crypto_box_keypair()
            return EphemeralKeyPair(public_key=pk, private_key=sk)
        raise RuntimeError("PyNaCl required")

    def dh(self, peer_public: bytes) -> bytes:
        """Compute X25519 shared secret."""
        if _HAS_NACL:
            return nacl.bindings.crypto_scalarmult(self.private_key, peer_public)
        raise RuntimeError("PyNaCl required")


# ── Key derivation ──────────────────────────────────────────────────────────

def hkdf_extract(salt: bytes, ikm: bytes) -> bytes:
    """HKDF-Extract: PRK = HMAC-SHA256(salt, ikm)."""
    return hmac.new(salt, ikm, hashlib.sha256).digest()


def hkdf_expand(prk: bytes, info: bytes, length: int = 32) -> bytes:
    """HKDF-Expand: derive key material from PRK."""
    n = (length + 31) // 32
    okm = b""
    t = b""
    for i in range(1, n + 1):
        t = hmac.new(prk, t + info + bytes([i]), hashlib.sha256).digest()
        okm += t
    return okm[:length]


def derive_session_keys(
    dh_secret: bytes,
    ctrl_eph_pub: bytes,
    node_eph_pub: bytes,
    ctrl_id: str,
    node_id: str,
) -> tuple[bytes, bytes, bytes, bytes]:
    """
    Derive symmetric session keys from DH shared secret.

    Returns: (ctrl_to_node_key, node_to_ctrl_key, nonce_seed, session_id)
    All 32 bytes except nonce_seed (16 bytes) and session_id (16 bytes).
    """
    salt = node_eph_pub + ctrl_eph_pub
    prk = hkdf_extract(salt, dh_secret)

    ctx = f"{ctrl_id}|{node_id}".encode()

    send_key = hkdf_expand(prk, b"ozma-v1|ctrl-to-node|" + ctx)
    recv_key = hkdf_expand(prk, b"ozma-v1|node-to-ctrl|" + ctx)
    nonce_seed = hkdf_expand(prk, b"ozma-v1|nonce-seed|" + ctx, NONCE_SEED_LEN)

    # Session ID = transcript hash (for channel binding verification)
    session_id = hashlib.sha256(ctrl_eph_pub + node_eph_pub + dh_secret).digest()[:16]

    return send_key, recv_key, nonce_seed, session_id


# ── AEAD encrypt/decrypt ────────────────────────────────────────────────────

def encrypt_packet(
    key: bytes,
    nonce_seed: bytes,
    counter: int,
    packet_type: int,
    payload: bytes,
) -> bytes:
    """
    Encrypt a packet with XChaCha20-Poly1305 AEAD.

    Returns the complete wire-format packet (header + ciphertext + MAC).
    """
    counter_bytes = struct.pack(">Q", counter)
    nonce = nonce_seed + counter_bytes  # 16 + 8 = 24 bytes
    aad = bytes([WIRE_VERSION, packet_type]) + counter_bytes

    if _HAS_NACL:
        ciphertext = nacl.bindings.crypto_aead_xchacha20poly1305_ietf_encrypt(
            payload, aad, nonce, key
        )
    else:
        # Development fallback: HMAC authentication only (NOT ENCRYPTED)
        mac = hmac.new(key, aad + payload, hashlib.sha256).digest()[:MAC_LEN]
        ciphertext = mac + payload

    return bytes([WIRE_VERSION, packet_type]) + counter_bytes + ciphertext


def decrypt_packet(
    key: bytes,
    nonce_seed: bytes,
    packet: bytes,
) -> tuple[int, bytes] | None:
    """
    Decrypt a packet. Returns (packet_type, payload) or None if auth fails.

    Does NOT check replay window — caller must do that.
    """
    if len(packet) < OVERHEAD:
        return None

    version = packet[0]
    if version != WIRE_VERSION:
        return None

    packet_type = packet[1]
    counter_bytes = packet[2:10]
    ciphertext = packet[10:]

    nonce = nonce_seed + counter_bytes
    aad = bytes([version, packet_type]) + counter_bytes

    if _HAS_NACL:
        try:
            payload = nacl.bindings.crypto_aead_xchacha20poly1305_ietf_decrypt(
                ciphertext, aad, nonce, key
            )
            return packet_type, payload
        except Exception:
            return None
    else:
        # Development fallback: verify HMAC
        if len(ciphertext) < MAC_LEN:
            return None
        mac = ciphertext[:MAC_LEN]
        payload = ciphertext[MAC_LEN:]
        expected = hmac.new(key, aad + payload, hashlib.sha256).digest()[:MAC_LEN]
        if hmac.compare_digest(mac, expected):
            return packet_type, payload
        return None


def get_counter(packet: bytes) -> int | None:
    """Extract the counter from an encrypted packet (without decrypting)."""
    if len(packet) < HEADER_LEN:
        return None
    return struct.unpack(">Q", packet[2:10])[0]


# ── Replay window ───────────────────────────────────────────────────────────

class ReplayWindow:
    """
    Sliding window replay protection.

    Tracks the highest seen counter and a bitmap of recent counters.
    Rejects packets with counter ≤ (highest - window_size) or already seen.
    """

    def __init__(self, window_size: int = REPLAY_WINDOW_HID) -> None:
        self._window_size = window_size
        self._highest = 0
        self._bitmap = 0  # bit N = 1 means (highest - N) has been seen

    def check_and_advance(self, counter: int) -> bool:
        """Return True if the counter is valid (not replayed). Advances the window."""
        if counter > self._highest:
            # New highest — shift the bitmap
            shift = counter - self._highest
            if shift < self._window_size:
                self._bitmap = (self._bitmap << shift) | 1
            else:
                self._bitmap = 1
            self._highest = counter
            return True

        # Counter is within or below the window
        diff = self._highest - counter
        if diff >= self._window_size:
            return False  # Too old

        bit = 1 << diff
        if self._bitmap & bit:
            return False  # Already seen (replay)

        self._bitmap |= bit
        return True


# ── Session state ───────────────────────────────────────────────────────────

@dataclass
class SessionState:
    """
    Holds the symmetric state for an active session with a node.

    Created after session establishment (DH + HKDF).
    """
    node_id: str
    send_key: bytes       # 32 bytes — for encrypting packets TO the node
    recv_key: bytes       # 32 bytes — for decrypting packets FROM the node
    nonce_seed: bytes     # 16 bytes — combined with counter for 24-byte nonce
    session_id: bytes     # 16 bytes — for channel binding verification
    send_counter: int = 0
    hid_replay: ReplayWindow | None = None
    audio_replay: ReplayWindow | None = None

    def __post_init__(self):
        if self.hid_replay is None:
            self.hid_replay = ReplayWindow(REPLAY_WINDOW_HID)
        if self.audio_replay is None:
            self.audio_replay = ReplayWindow(REPLAY_WINDOW_AUDIO)

    def encrypt(self, packet_type: int, payload: bytes) -> bytes:
        """Encrypt a packet for this node."""
        pkt = encrypt_packet(
            self.send_key, self.nonce_seed,
            self.send_counter, packet_type, payload,
        )
        self.send_counter += 1
        return pkt

    def decrypt(self, packet: bytes) -> tuple[int, bytes] | None:
        """Decrypt a packet from this node. Returns (type, payload) or None."""
        counter = get_counter(packet)
        if counter is None:
            return None

        # Check replay window based on packet type (peeked from header)
        pkt_type = packet[1] if len(packet) > 1 else 0
        window = self.audio_replay if pkt_type == PKT_AUDIO else self.hid_replay
        if not window.check_and_advance(counter):
            return None

        return decrypt_packet(self.recv_key, self.nonce_seed, packet)

    @staticmethod
    def from_dh(
        dh_secret: bytes,
        ctrl_eph_pub: bytes,
        node_eph_pub: bytes,
        ctrl_id: str,
        node_id: str,
    ) -> "SessionState":
        """Create a SessionState from a completed DH key exchange."""
        send_key, recv_key, nonce_seed, session_id = derive_session_keys(
            dh_secret, ctrl_eph_pub, node_eph_pub, ctrl_id, node_id,
        )
        return SessionState(
            node_id=node_id,
            send_key=send_key,
            recv_key=recv_key,
            nonce_seed=nonce_seed,
            session_id=session_id,
        )
