#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Unit tests for KeyStore — master key lifecycle, backup methods,
subkey derivation, BYOK injection, rate limiting, and persistence.

Argon2 is mocked for speed (correctness of argon2-cffi is out-of-scope).
BIP39 tests are skipped if the mnemonic package is not installed.
"""

import asyncio
import base64
import json
import os
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

sys.path.insert(0, str(Path(__file__).parent.parent.parent.parent / "controller"))

from key_store import (
    KeyStore, BackupMethod, KeyState, KeyLockedError, KeyNotInitialisedError,
    UnlockRateLimitedError, SecureBytes, bip39_encode, bip39_decode,
    _encrypt, _decrypt, _make_verify_blob, _verify_master_key, _hkdf,
    _b64, _unb64, _HAS_BIP39,
)
from cryptography.exceptions import InvalidTag


# ── Helpers ───────────────────────────────────────────────────────────────────

def run(coro):
    loop = asyncio.new_event_loop()
    try:
        return loop.run_until_complete(coro)
    finally:
        loop.close()


def _store(tmp: Path, controller_id: str = "test-ctrl") -> KeyStore:
    return KeyStore(data_dir=tmp, controller_id=controller_id)


def _fast_store(tmp: Path) -> KeyStore:
    """Store with patched Argon2 for speed."""
    ks = _store(tmp)
    return ks


def _fake_argon2(password, salt, t, m, p):
    """Fast deterministic KEK for tests — DO NOT use in production."""
    import hashlib
    return hashlib.sha256(password.encode() + salt).digest()


# ── SecureBytes ───────────────────────────────────────────────────────────────

class TestSecureBytes(unittest.TestCase):

    def test_read(self):
        sb = SecureBytes(b"hello world secret")
        self.assertEqual(sb.read(), b"hello world secret")

    def test_zeroed_after_close(self):
        sb = SecureBytes(b"\xff" * 32)
        self.assertTrue(bool(sb))
        sb.close()
        self.assertFalse(bool(sb))

    def test_double_close_safe(self):
        sb = SecureBytes(b"key")
        sb.close()
        sb.close()  # Should not raise

    def test_read_after_close_raises(self):
        sb = SecureBytes(b"key")
        sb.close()
        with self.assertRaises(RuntimeError):
            sb.read()

    def test_del_safe(self):
        sb = SecureBytes(b"key material")
        del sb  # Should not raise


# ── Crypto primitives ─────────────────────────────────────────────────────────

class TestCryptoPrimitives(unittest.TestCase):

    def test_encrypt_decrypt_roundtrip(self):
        key = os.urandom(32)
        plaintext = b"super secret data"
        nonce, ct = _encrypt(plaintext, key)
        result = _decrypt(nonce, ct, key)
        self.assertEqual(result, plaintext)

    def test_wrong_key_raises_invalid_tag(self):
        key = os.urandom(32)
        nonce, ct = _encrypt(b"data", key)
        with self.assertRaises(InvalidTag):
            _decrypt(nonce, ct, os.urandom(32))

    def test_nonces_are_unique(self):
        key = os.urandom(32)
        nonces = {_encrypt(b"data", key)[0] for _ in range(50)}
        self.assertEqual(len(nonces), 50)

    def test_verify_blob_correct_key(self):
        master = os.urandom(32)
        salt = os.urandom(32)
        v_nonce, v_ct = _make_verify_blob(master, salt)
        self.assertTrue(_verify_master_key(master, salt, v_nonce, v_ct))

    def test_verify_blob_wrong_key(self):
        master = os.urandom(32)
        salt = os.urandom(32)
        v_nonce, v_ct = _make_verify_blob(master, salt)
        self.assertFalse(_verify_master_key(os.urandom(32), salt, v_nonce, v_ct))

    def test_hkdf_versioned_info(self):
        ikm = os.urandom(32)
        salt = os.urandom(32)
        k1 = _hkdf(ikm, salt, "ozma:v1:mesh_ca")
        k2 = _hkdf(ikm, salt, "ozma:v1:footage")
        self.assertNotEqual(k1, k2)

    def test_hkdf_salt_changes_output(self):
        ikm = os.urandom(32)
        k1 = _hkdf(ikm, os.urandom(32), "ozma:v1:mesh_ca")
        k2 = _hkdf(ikm, os.urandom(32), "ozma:v1:mesh_ca")
        self.assertNotEqual(k1, k2)

    def test_hkdf_deterministic(self):
        ikm = b"a" * 32
        salt = b"b" * 32
        k1 = _hkdf(ikm, salt, "ozma:v1:mesh_ca")
        k2 = _hkdf(ikm, salt, "ozma:v1:mesh_ca")
        self.assertEqual(k1, k2)


# ── BIP39 ─────────────────────────────────────────────────────────────────────

@unittest.skipUnless(_HAS_BIP39, "mnemonic package not installed")
class TestBIP39(unittest.TestCase):

    def test_encode_decode_roundtrip(self):
        key = os.urandom(32)
        words = bip39_encode(key)
        self.assertEqual(len(words), 24)
        recovered = bip39_decode(words)
        self.assertEqual(recovered, key)

    def test_wrong_words_raises(self):
        words = bip39_encode(os.urandom(32))
        words[0] = "zzzzzzzzz"  # not a valid BIP39 word
        with self.assertRaises(ValueError):
            bip39_decode(words)

    def test_tampered_checksum_raises(self):
        """Changing the last word (which carries checksum bits) should fail."""
        key = os.urandom(32)
        words = bip39_encode(key)
        # Replace last word with a different valid word from a different key
        other_words = bip39_encode(os.urandom(32))
        words[-1] = other_words[-1]
        with self.assertRaises(ValueError):
            bip39_decode(words)

    def test_all_keys_produce_24_words(self):
        for _ in range(10):
            self.assertEqual(len(bip39_encode(os.urandom(32))), 24)


# ── KeyStore: uninitialised ────────────────────────────────────────────────────

class TestKeyStoreUninitialised(unittest.TestCase):

    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp = Path(self._tmp.name)

    def tearDown(self):
        self._tmp.cleanup()

    def test_state_is_uninitialised(self):
        ks = _store(self.tmp)
        self.assertEqual(ks.get_status()["state"], "uninitialised")

    def test_derive_subkey_raises(self):
        ks = _store(self.tmp)
        with self.assertRaises(KeyNotInitialisedError):
            ks.derive_subkey("mesh_ca")

    def test_unlock_password_raises_not_initialised(self):
        ks = _store(self.tmp)
        with self.assertRaises(KeyNotInitialisedError):
            run(ks.unlock_password("password"))


# ── KeyStore: password method ─────────────────────────────────────────────────

@patch("key_store._derive_kek_password", side_effect=_fake_argon2)
@patch("key_store._HAS_ARGON2", True)
@patch("key_store._calibrate_argon2", new_callable=AsyncMock, return_value=65536)
class TestKeyStorePassword(unittest.TestCase):

    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp = Path(self._tmp.name)

    def tearDown(self):
        self._tmp.cleanup()

    def test_init_creates_blob(self, mock_cal, mock_argon2):
        ks = _store(self.tmp)
        run(ks.init_password("correct-horse"))
        self.assertTrue((self.tmp / "key.json").exists())

    def test_init_unlocks_immediately(self, mock_cal, mock_argon2):
        ks = _store(self.tmp)
        run(ks.init_password("correct-horse"))
        self.assertEqual(ks.get_status()["state"], "unlocked")

    def test_unlock_correct_password(self, mock_cal, mock_argon2):
        ks = _store(self.tmp)
        run(ks.init_password("correct-horse"))
        ks.lock()
        self.assertEqual(ks.get_status()["state"], "locked")
        ok = run(ks.unlock_password("correct-horse"))
        self.assertTrue(ok)
        self.assertEqual(ks.get_status()["state"], "unlocked")

    def test_unlock_wrong_password(self, mock_cal, mock_argon2):
        ks = _store(self.tmp)
        run(ks.init_password("correct-horse"))
        ks.lock()
        ok = run(ks.unlock_password("wrong-password"))
        self.assertFalse(ok)
        self.assertEqual(ks.get_status()["state"], "locked")

    def test_derive_subkey_unlocked(self, mock_cal, mock_argon2):
        ks = _store(self.tmp)
        run(ks.init_password("correct-horse"))
        key = ks.derive_subkey("mesh_ca")
        self.assertEqual(len(key), 32)

    def test_derive_subkey_locked_raises(self, mock_cal, mock_argon2):
        ks = _store(self.tmp)
        run(ks.init_password("correct-horse"))
        ks.lock()
        with self.assertRaises(KeyLockedError):
            ks.derive_subkey("mesh_ca")

    def test_subkeys_differ_by_purpose(self, mock_cal, mock_argon2):
        ks = _store(self.tmp)
        run(ks.init_password("correct-horse"))
        k1 = ks.derive_subkey("mesh_ca")
        k2 = ks.derive_subkey("footage")
        self.assertNotEqual(k1, k2)

    def test_subkeys_stable_across_unlocks(self, mock_cal, mock_argon2):
        ks = _store(self.tmp)
        run(ks.init_password("correct-horse"))
        k1 = ks.derive_subkey("mesh_ca")
        ks.lock()
        run(ks.unlock_password("correct-horse"))
        k2 = ks.derive_subkey("mesh_ca")
        self.assertEqual(k1, k2)

    def test_blob_has_controller_id(self, mock_cal, mock_argon2):
        ks = _store(self.tmp, controller_id="ctrl-abc")
        run(ks.init_password("correct-horse"))
        blob = json.loads((self.tmp / "key.json").read_text())
        self.assertEqual(blob["controller_id"], "ctrl-abc")

    def test_blob_has_hkdf_salt(self, mock_cal, mock_argon2):
        ks = _store(self.tmp)
        run(ks.init_password("correct-horse"))
        blob = json.loads((self.tmp / "key.json").read_text())
        self.assertIn("hkdf_salt", blob)
        self.assertEqual(len(_unb64(blob["hkdf_salt"])), 32)

    def test_argon2_params_in_blob(self, mock_cal, mock_argon2):
        ks = _store(self.tmp)
        run(ks.init_password("correct-horse"))
        blob = json.loads((self.tmp / "key.json").read_text())
        self.assertIn("argon2_t", blob)
        self.assertIn("argon2_m", blob)
        self.assertIn("argon2_p", blob)

    def test_blob_written_atomically(self, mock_cal, mock_argon2):
        """No partial write — .tmp should not remain after init."""
        ks = _store(self.tmp)
        run(ks.init_password("correct-horse"))
        self.assertFalse((self.tmp / "key.tmp").exists())

    def test_persist_and_reload(self, mock_cal, mock_argon2):
        ks = _store(self.tmp)
        run(ks.init_password("correct-horse"))
        k1 = ks.derive_subkey("mesh_ca")

        # Reload
        ks2 = _store(self.tmp)
        ok = run(ks2.unlock_password("correct-horse"))
        self.assertTrue(ok)
        k2 = ks2.derive_subkey("mesh_ca")
        self.assertEqual(k1, k2)


# ── KeyStore: export method ────────────────────────────────────────────────────

class TestKeyStoreExport(unittest.TestCase):

    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp = Path(self._tmp.name)

    def tearDown(self):
        self._tmp.cleanup()

    def test_init_returns_words(self):
        ks = _store(self.tmp)
        words = run(ks.init_export())
        # BIP39 returns 24 words if available, else 1 hex token
        self.assertIsInstance(words, list)
        self.assertGreater(len(words), 0)

    def test_init_unlocks_immediately(self):
        ks = _store(self.tmp)
        run(ks.init_export())
        self.assertEqual(ks.get_status()["state"], "unlocked")

    def test_unlock_correct_words(self):
        ks = _store(self.tmp)
        words = run(ks.init_export())
        ks.lock()
        ok = run(ks.unlock_export(words))
        self.assertTrue(ok)

    def test_unlock_wrong_words_returns_false(self):
        ks = _store(self.tmp)
        run(ks.init_export())
        ks.lock()
        # Generate words for a different key
        wrong_words = bip39_encode(os.urandom(32)) if _HAS_BIP39 else [os.urandom(32).hex()]
        ok = run(ks.unlock_export(wrong_words))
        self.assertFalse(ok)

    def test_subkeys_stable_across_unlock(self):
        ks = _store(self.tmp)
        words = run(ks.init_export())
        k1 = ks.derive_subkey("footage")
        ks.lock()
        run(ks.unlock_export(words))
        k2 = ks.derive_subkey("footage")
        self.assertEqual(k1, k2)

    def test_export_words_matches_init_words(self):
        ks = _store(self.tmp)
        init_words = run(ks.init_export())
        export_words = ks.export_words()
        self.assertEqual(init_words, export_words)

    def test_export_words_locked_raises(self):
        ks = _store(self.tmp)
        run(ks.init_export())
        ks.lock()
        with self.assertRaises(KeyLockedError):
            ks.export_words()


# ── KeyStore: none / ephemeral ────────────────────────────────────────────────

class TestKeyStoreNone(unittest.TestCase):

    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp = Path(self._tmp.name)

    def tearDown(self):
        self._tmp.cleanup()

    def test_init_requires_confirm(self):
        ks = _store(self.tmp)
        with self.assertRaises(ValueError):
            run(ks.init_none(confirm=False))

    def test_init_with_confirm_unlocks(self):
        ks = _store(self.tmp)
        run(ks.init_none(confirm=True))
        self.assertEqual(ks.get_status()["state"], "unlocked")

    def test_start_auto_unlocks(self):
        """On start(), ephemeral method auto-generates a key."""
        ks = _store(self.tmp)
        run(ks.init_none(confirm=True))
        ks.lock()
        run(ks.start())
        self.assertEqual(ks.get_status()["state"], "unlocked")


# ── KeyStore: rate limiting ────────────────────────────────────────────────────

@patch("key_store._derive_kek_password", side_effect=_fake_argon2)
@patch("key_store._HAS_ARGON2", True)
@patch("key_store._calibrate_argon2", new_callable=AsyncMock, return_value=65536)
class TestRateLimiting(unittest.TestCase):

    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp = Path(self._tmp.name)

    def tearDown(self):
        self._tmp.cleanup()

    def test_5_failures_triggers_lockout(self, mock_cal, mock_argon2):
        ks = _store(self.tmp)
        run(ks.init_password("real-password"))
        ks.lock()
        for _ in range(5):
            run(ks.unlock_password("wrong"))
        self.assertTrue(ks.get_status()["rate_limited"])

    def test_rate_limited_raises_on_next_attempt(self, mock_cal, mock_argon2):
        ks = _store(self.tmp)
        run(ks.init_password("real-password"))
        ks.lock()
        ks._failed_attempts = 5
        ks._lockout_until = time.monotonic() + 300
        with self.assertRaises(UnlockRateLimitedError):
            run(ks.unlock_password("any"))

    def test_correct_password_resets_failures(self, mock_cal, mock_argon2):
        ks = _store(self.tmp)
        run(ks.init_password("real-password"))
        ks.lock()
        run(ks.unlock_password("wrong"))
        run(ks.unlock_password("wrong"))
        run(ks.unlock_password("real-password"))
        self.assertEqual(ks.get_status()["failed_attempts"], 0)
        self.assertFalse(ks.get_status()["rate_limited"])


# ── KeyStore: BYOK injection ──────────────────────────────────────────────────

class TestByokInjection(unittest.TestCase):

    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp = Path(self._tmp.name)

    def tearDown(self):
        self._tmp.cleanup()

    def test_inject_unlocks(self):
        ks = _store(self.tmp)
        key = os.urandom(32)
        run(ks.inject(key, ttl=3600, actor="alice", source_ip="10.0.0.1"))
        self.assertEqual(ks.get_status()["state"], "unlocked")

    def test_inject_derive_subkey(self):
        ks = _store(self.tmp)
        key = os.urandom(32)
        run(ks.inject(key, ttl=3600))
        sk = ks.derive_subkey("mesh_ca")
        self.assertEqual(len(sk), 32)

    def test_inject_subkeys_stable(self):
        ks = _store(self.tmp)
        key = os.urandom(32)
        run(ks.inject(key, ttl=3600))
        k1 = ks.derive_subkey("mesh_ca")
        k2 = ks.derive_subkey("mesh_ca")
        self.assertEqual(k1, k2)

    def test_inject_wrong_size_raises(self):
        ks = _store(self.tmp)
        with self.assertRaises(ValueError):
            run(ks.inject(os.urandom(16)))  # 16 bytes, not 32

    def test_evict_clears_key(self):
        ks = _store(self.tmp)
        run(ks.inject(os.urandom(32), ttl=3600))
        ks.evict_injected()
        self.assertIsNone(ks.get_status()["injected_session"])

    def test_evict_locks(self):
        ks = _store(self.tmp)
        run(ks.inject(os.urandom(32), ttl=3600))
        ks.evict_injected()
        with self.assertRaises(KeyLockedError):
            ks.derive_subkey("mesh_ca")

    def test_expired_session_raises_on_derive(self):
        ks = _store(self.tmp)
        run(ks.inject(os.urandom(32), ttl=1))
        # Manually expire
        ks._injected.expires_at = time.monotonic() - 1
        with self.assertRaises(KeyLockedError):
            ks.derive_subkey("mesh_ca")

    def test_session_info_in_status(self):
        ks = _store(self.tmp)
        run(ks.inject(os.urandom(32), ttl=3600, actor="alice", source_ip="1.2.3.4"))
        status = ks.get_status()
        self.assertIsNotNone(status["injected_session"])
        self.assertEqual(status["injected_session"]["actor"], "alice")

    def test_ttl_capped_at_max(self):
        ks = _store(self.tmp)
        session = run(ks.inject(os.urandom(32), ttl=999999))
        self.assertLessEqual(session.ttl, KeyStore.BYOK_MAX_TTL)

    def test_inject_replaces_previous_session(self):
        ks = _store(self.tmp)
        key1 = os.urandom(32)
        key2 = os.urandom(32)
        run(ks.inject(key1, ttl=3600))
        k1 = ks.derive_subkey("footage")
        run(ks.inject(key2, ttl=3600))
        k2 = ks.derive_subkey("footage")
        self.assertNotEqual(k1, k2)


# ── KeyStore: change_method ───────────────────────────────────────────────────

@patch("key_store._derive_kek_password", side_effect=_fake_argon2)
@patch("key_store._HAS_ARGON2", True)
@patch("key_store._calibrate_argon2", new_callable=AsyncMock, return_value=65536)
class TestChangeMethod(unittest.TestCase):

    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp = Path(self._tmp.name)

    def tearDown(self):
        self._tmp.cleanup()

    def test_password_to_export(self, mock_cal, mock_argon2):
        ks = _store(self.tmp)
        run(ks.init_password("original"))
        k_before = ks.derive_subkey("footage")

        words = run(ks.change_method(BackupMethod.EXPORT))
        self.assertIsNotNone(words)
        k_after = ks.derive_subkey("footage")
        # Master key unchanged — subkey must be identical
        self.assertEqual(k_before, k_after)

    def test_export_to_password(self, mock_cal, mock_argon2):
        ks = _store(self.tmp)
        run(ks.init_export())
        k_before = ks.derive_subkey("config")

        run(ks.change_method(BackupMethod.PASSWORD, password="new-pass"))
        k_after = ks.derive_subkey("config")
        self.assertEqual(k_before, k_after)

    def test_change_locked_raises(self, mock_cal, mock_argon2):
        ks = _store(self.tmp)
        run(ks.init_password("pass"))
        ks.lock()
        with self.assertRaises(KeyLockedError):
            run(ks.change_method(BackupMethod.EXPORT))

    def test_change_to_none_requires_confirm(self, mock_cal, mock_argon2):
        ks = _store(self.tmp)
        run(ks.init_password("pass"))
        with self.assertRaises(ValueError):
            run(ks.change_method(BackupMethod.NONE, confirm_none=False))

    def test_new_method_persisted(self, mock_cal, mock_argon2):
        ks = _store(self.tmp)
        run(ks.init_password("pass"))
        run(ks.change_method(BackupMethod.EXPORT))
        blob = json.loads((self.tmp / "key.json").read_text())
        self.assertEqual(blob["method"], "export")

    def test_atomic_write_no_tmp_file(self, mock_cal, mock_argon2):
        ks = _store(self.tmp)
        run(ks.init_password("pass"))
        run(ks.change_method(BackupMethod.EXPORT))
        self.assertFalse((self.tmp / "key.tmp").exists())

    def test_unlock_with_new_method_after_change(self, mock_cal, mock_argon2):
        ks = _store(self.tmp)
        run(ks.init_password("original"))
        words = run(ks.change_method(BackupMethod.EXPORT))
        ks.lock()

        # Reload from disk
        ks2 = _store(self.tmp)
        ok = run(ks2.unlock_export(words))
        self.assertTrue(ok)


# ── KeyStore: status ──────────────────────────────────────────────────────────

class TestKeyStoreStatus(unittest.TestCase):

    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp = Path(self._tmp.name)

    def tearDown(self):
        self._tmp.cleanup()

    def test_status_keys(self):
        ks = _store(self.tmp)
        status = ks.get_status()
        for key in ("state", "method", "has_blob", "failed_attempts", "rate_limited"):
            self.assertIn(key, status)

    def test_no_blob_initially(self):
        ks = _store(self.tmp)
        self.assertFalse(ks.get_status()["has_blob"])

    def test_has_blob_after_init(self):
        ks = _store(self.tmp)
        run(ks.init_export())
        self.assertTrue(ks.get_status()["has_blob"])


# ── Connect backup integration ────────────────────────────────────────────────

@patch("key_store._derive_kek_password", side_effect=_fake_argon2)
@patch("key_store._HAS_ARGON2", True)
@patch("key_store._calibrate_argon2", new_callable=AsyncMock, return_value=65536)
class TestConnectBackup(unittest.TestCase):

    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp = Path(self._tmp.name)

    def tearDown(self):
        self._tmp.cleanup()

    def test_backup_pushes_blob(self, mock_cal, mock_argon2):
        ks = _store(self.tmp)
        run(ks.init_password("test123"))

        connect = AsyncMock()
        run(ks.backup_to_connect(connect))

        connect.store_key_blob.assert_called_once()
        call_kwargs = connect.store_key_blob.call_args
        assert call_kwargs.kwargs["controller_id"] == "test-ctrl"
        blob_bytes = call_kwargs.kwargs["blob"]
        blob = json.loads(blob_bytes)
        assert blob["method"] == "password"

    def test_backup_not_available_for_none_method(self, mock_cal, mock_argon2):
        ks = _store(self.tmp)
        run(ks.init_none(confirm=True))
        self.assertFalse(ks.connect_backup_enabled())

    def test_connect_backup_enabled_password(self, mock_cal, mock_argon2):
        ks = _store(self.tmp)
        run(ks.init_password("pw"))
        self.assertTrue(ks.connect_backup_enabled())

    def test_connect_backup_enabled_export(self, mock_cal, mock_argon2):
        ks = _store(self.tmp)
        run(ks.init_export())
        self.assertTrue(ks.connect_backup_enabled())

    def test_connect_backup_disabled_none(self, mock_cal, mock_argon2):
        ks = _store(self.tmp)
        run(ks.init_none(confirm=True))
        self.assertFalse(ks.connect_backup_enabled())

    def test_connect_backup_disabled_uninitialised(self, mock_cal, mock_argon2):
        ks = _store(self.tmp)
        self.assertFalse(ks.connect_backup_enabled())

    def test_backup_without_connect_raises(self, mock_cal, mock_argon2):
        ks = _store(self.tmp)
        run(ks.init_password("pw"))
        with self.assertRaises(RuntimeError):
            run(ks.backup_to_connect(None))

    def test_restore_writes_blob(self, mock_cal, mock_argon2):
        """Restore from Connect writes the blob to disk and enters LOCKED state."""
        ks1 = _store(self.tmp)
        run(ks1.init_password("mypassword"))
        original_blob = json.loads((self.tmp / "key.json").read_text())

        # Remove local blob to simulate hardware loss
        (self.tmp / "key.json").unlink()

        # Fresh store — no blob
        ks2 = _store(self.tmp)
        self.assertEqual(ks2._state, KeyState.UNINITIALISED)

        connect = AsyncMock()
        connect.fetch_key_blob.return_value = json.dumps(original_blob).encode()

        run(ks2.restore_from_connect(connect))

        # After restore: should be LOCKED (blob present, but not yet unlocked)
        self.assertEqual(ks2._state, KeyState.LOCKED)
        self.assertEqual(ks2._method, BackupMethod.PASSWORD)
        self.assertTrue((self.tmp / "key.json").exists())

    def test_restore_without_connect_raises(self, mock_cal, mock_argon2):
        ks = _store(self.tmp)
        with self.assertRaises(RuntimeError):
            run(ks.restore_from_connect(None))

    def test_restore_controller_id_mismatch_raises(self, mock_cal, mock_argon2):
        """Blob from a different controller must be rejected."""
        ks1 = _store(self.tmp)
        run(ks1.init_password("pw"))
        original_blob = json.loads((self.tmp / "key.json").read_text())
        (self.tmp / "key.json").unlink()

        # Simulate blob from a different controller
        original_blob["controller_id"] = "different-controller"

        ks2 = _store(self.tmp)
        connect = AsyncMock()
        connect.fetch_key_blob.return_value = json.dumps(original_blob).encode()

        with self.assertRaises(RuntimeError, msg="controller_id mismatch"):
            run(ks2.restore_from_connect(connect))

    def test_restore_no_blob_raises(self, mock_cal, mock_argon2):
        ks = _store(self.tmp)
        connect = AsyncMock()
        connect.fetch_key_blob.return_value = None
        with self.assertRaises(RuntimeError):
            run(ks.restore_from_connect(connect))

    def test_restore_then_unlock(self, mock_cal, mock_argon2):
        """Full DR cycle: backup → local loss → restore → unlock → derive."""
        ks1 = _store(self.tmp)
        run(ks1.init_password("drpassword"))
        subkey1 = ks1.derive_subkey("footage")
        blob_bytes = json.dumps(json.loads((self.tmp / "key.json").read_text())).encode()

        # Simulate disaster: lose local blob
        (self.tmp / "key.json").unlink()

        # Restore
        ks2 = _store(self.tmp)
        connect = AsyncMock()
        connect.fetch_key_blob.return_value = blob_bytes
        run(ks2.restore_from_connect(connect))

        # Unlock with original password
        ok = run(ks2.unlock_password("drpassword"))
        self.assertTrue(ok)

        # Subkey must match original
        subkey2 = ks2.derive_subkey("footage")
        self.assertEqual(subkey1, subkey2)


if __name__ == "__main__":
    unittest.main()
