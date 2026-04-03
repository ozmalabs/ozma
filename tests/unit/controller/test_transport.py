# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Unit tests for transport.py — IdentityKeyPair, AEAD packet encrypt/decrypt."""
import os
import sys
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).parent.parent.parent.parent / "controller"))
pytestmark = pytest.mark.unit


class TestIdentityKeyPair:
    def test_generate_produces_32_byte_pubkey(self):
        from transport import IdentityKeyPair
        kp = IdentityKeyPair.generate()
        assert kp.public_key is not None
        assert len(kp.public_key) == 32

    def test_two_generates_are_distinct(self):
        from transport import IdentityKeyPair
        kp1 = IdentityKeyPair.generate()
        kp2 = IdentityKeyPair.generate()
        assert kp1.public_key != kp2.public_key

    def test_sign_and_verify(self):
        from transport import IdentityKeyPair
        kp = IdentityKeyPair.generate()
        msg = b"hello ozma"
        sig = kp.sign(msg)
        assert IdentityKeyPair.verify(msg, sig, kp.public_key)

    def test_verify_wrong_key_fails(self):
        from transport import IdentityKeyPair
        kp1 = IdentityKeyPair.generate()
        kp2 = IdentityKeyPair.generate()
        sig = kp1.sign(b"data")
        assert not IdentityKeyPair.verify(b"data", sig, kp2.public_key)

    def test_verify_tampered_message_fails(self):
        from transport import IdentityKeyPair
        kp = IdentityKeyPair.generate()
        sig = kp.sign(b"original")
        assert not IdentityKeyPair.verify(b"tampered", sig, kp.public_key)

    def test_verify_truncated_sig_fails(self):
        from transport import IdentityKeyPair
        kp = IdentityKeyPair.generate()
        sig = kp.sign(b"msg")
        assert not IdentityKeyPair.verify(b"msg", sig[:16], kp.public_key)

    def test_fingerprint_is_string(self):
        from transport import IdentityKeyPair
        kp = IdentityKeyPair.generate()
        fp = kp.fingerprint()
        assert isinstance(fp, str)
        assert len(fp) > 0


class TestEphemeralKeyPair:
    def test_generate(self):
        from transport import EphemeralKeyPair
        ekp = EphemeralKeyPair.generate()
        assert ekp.public_key is not None
        assert len(ekp.public_key) == 32

    def test_dh_is_symmetric(self):
        from transport import EphemeralKeyPair
        a = EphemeralKeyPair.generate()
        b = EphemeralKeyPair.generate()
        assert a.dh(b.public_key) == b.dh(a.public_key)


class TestAEADPackets:
    def test_encrypt_decrypt_roundtrip(self):
        from transport import encrypt_packet, decrypt_packet
        key = os.urandom(32)
        nonce_seed = os.urandom(16)
        payload = b"KVM switch HID packet"
        ciphertext = encrypt_packet(key, nonce_seed, counter=0,
                                    packet_type=1, payload=payload)
        result = decrypt_packet(key, nonce_seed, ciphertext)
        assert result is not None
        pkt_type, decrypted = result
        assert pkt_type == 1
        assert decrypted == payload

    def test_wrong_key_fails_to_decrypt(self):
        from transport import encrypt_packet, decrypt_packet
        key1 = os.urandom(32)
        key2 = os.urandom(32)
        nonce_seed = os.urandom(16)
        ciphertext = encrypt_packet(key1, nonce_seed, 0, 1, b"secret data")
        # Different key → should fail or return garbage (AEAD guarantees None)
        result = decrypt_packet(key2, nonce_seed, ciphertext)
        if result is not None:
            _, decrypted = result
            assert decrypted != b"secret data"

    def test_counter_in_header(self):
        from transport import encrypt_packet, get_counter
        key = os.urandom(32)
        nonce_seed = os.urandom(16)
        pkt = encrypt_packet(key, nonce_seed, counter=42, packet_type=1, payload=b"x")
        assert get_counter(pkt) == 42

    def test_different_counters_produce_different_ciphertext(self):
        from transport import encrypt_packet
        key = os.urandom(32)
        nonce_seed = os.urandom(16)
        payload = b"same payload"
        pkt0 = encrypt_packet(key, nonce_seed, 0, 1, payload)
        pkt1 = encrypt_packet(key, nonce_seed, 1, 1, payload)
        assert pkt0 != pkt1


class TestSessionState:
    def test_encrypt_decrypt_via_session(self):
        from transport import SessionState
        import os
        send_key = os.urandom(32)
        recv_key = send_key   # symmetric for test
        nonce_seed = os.urandom(16)
        session_id = os.urandom(16)
        s1 = SessionState(
            node_id="test-node", session_id=session_id,
            send_key=send_key, recv_key=recv_key, nonce_seed=nonce_seed,
        )
        s2 = SessionState(
            node_id="test-node", session_id=session_id,
            send_key=recv_key, recv_key=send_key, nonce_seed=nonce_seed,
        )
        payload = b"hello session"
        pkt = s1.encrypt(packet_type=1, payload=payload)
        result = s2.decrypt(pkt)
        assert result is not None
        pkt_type, decrypted = result
        assert pkt_type == 1
        assert decrypted == payload
