#!/usr/bin/env python3
"""
Generate known-answer test (KAT) vectors for ozma-crypto from the Python
reference implementation in controller/transport.py.

Usage (from repo root):
    pip install pynacl
    python ozma-crypto/kat/generate_kat.py

Paste the printed hex strings into ozma-crypto/src/tests.rs.
"""
import sys
import os

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "../.."))

from controller.transport import (
    hkdf_extract,
    hkdf_expand,
    derive_session_keys,
    encrypt_packet,
    decrypt_packet,
    get_counter,
    PKT_KEYBOARD,
)

# ── Fixed inputs ──────────────────────────────────────────────────────────────

DH_SECRET    = bytes.fromhex("4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742")
CTRL_EPH_PUB = bytes.fromhex("de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f")
NODE_EPH_PUB = bytes.fromhex("8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a")

ZERO_KEY        = bytes(32)
ZERO_NONCE_SEED = bytes(16)

def h(b: bytes) -> str:
    return b.hex()

# ── HKDF-Extract ─────────────────────────────────────────────────────────────

print("=== HKDF-Extract ===")
salt = NODE_EPH_PUB + CTRL_EPH_PUB
prk = hkdf_extract(salt, DH_SECRET)
print(f"PRK = {h(prk)}")

# ── derive_session_keys ───────────────────────────────────────────────────────

print("\n=== derive_session_keys(ctrl-01, node-01) ===")
send_key, recv_key, nonce_seed, session_id = derive_session_keys(
    DH_SECRET, CTRL_EPH_PUB, NODE_EPH_PUB, "ctrl-01", "node-01"
)
print(f"send_key   = {h(send_key)}")
print(f"recv_key   = {h(recv_key)}")
print(f"nonce_seed = {h(nonce_seed)}")
print(f"session_id = {h(session_id)}")

# ── encrypt_packet KAT ────────────────────────────────────────────────────────

print("\n=== encrypt_packet(zero_key, zero_nonce_seed, counter=0, PKT_KEYBOARD, b'ozma') ===")
pkt = encrypt_packet(ZERO_KEY, ZERO_NONCE_SEED, 0, PKT_KEYBOARD, b"ozma")
print(f"full packet = {h(pkt)}")
print(f"header      = {h(pkt[:10])}")
print(f"ciphertext  = {h(pkt[10:])}")
print(f"length      = {len(pkt)}")

result = decrypt_packet(ZERO_KEY, ZERO_NONCE_SEED, pkt)
assert result == (PKT_KEYBOARD, b"ozma"), f"Round-trip failed: {result}"
print("Round-trip: OK")

# ── get_counter ───────────────────────────────────────────────────────────────

print("\n=== get_counter ===")
pkt42 = encrypt_packet(ZERO_KEY, ZERO_NONCE_SEED, 42, PKT_KEYBOARD, b"x")
print(f"counter from packet with counter=42: {get_counter(pkt42)}")
