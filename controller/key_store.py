# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Controller master key store.

A single 32-byte master key protects all zero-knowledge data:
  - Mesh CA keypair (WireGuard peering, node certificates)
  - Camera footage encryption
  - Config / scenario backups
  - Room correction profiles

The key is memory-only after unlock.  At rest it is protected by one of:

  password  — Argon2id → KEK → ChaCha20-Poly1305(master_key)
  export    — BIP39 24-word mnemonic encodes the master key directly.
              No separate password.  The words ARE the key.
              Store like cash.
  none      — Ephemeral: new key generated each start.  All encrypted data
              is lost on restart.  Requires explicit confirmation.

Key hierarchy
─────────────
  master_key (32 bytes, os.urandom)
      │
      ├─ HKDF(hkdf_salt, "ozma:v1:mesh_ca")   → mesh CA key
      ├─ HKDF(hkdf_salt, "ozma:v1:footage")   → camera footage key
      ├─ HKDF(hkdf_salt, "ozma:v1:config")    → config backup key
      ├─ HKDF(hkdf_salt, "ozma:v1:verify")    → key verification AEAD
      └─ HKDF(hkdf_salt, "ozma:v1:<purpose>") → any future purpose

hkdf_salt is 32 bytes, stored in plaintext in the blob (not secret).
It makes all subkeys installation-unique even if master keys collide.

Blob format (key.json on disk)
───────────────────────────────
{
  "version":       1,
  "method":        "password" | "export" | "none",
  "controller_id": "<id>",
  "hkdf_salt":     "<base64 32 bytes>",
  "verify_nonce":  "<base64 12 bytes>",
  "verify_ct":     "<base64 48 bytes>",   // ChaCha20-Poly1305 auth blob
  // password method only:
  "argon2_t":      3,
  "argon2_m":      131072,
  "argon2_p":      4,
  "argon2_salt":   "<base64 16 bytes>",
  "key_nonce":     "<base64 12 bytes>",
  "key_ct":        "<base64 48 bytes>",   // ChaCha20-Poly1305(KEK, master_key)
}

Security notes
──────────────
- Rate-limiting: 5 failed unlock attempts → 5-minute lockout (per process)
- BYOK (cloud controller Pro): key injected via API, memory-only, TTL-evicted
- Locked state: derive_subkey() raises KeyLockedError; callers must handle
- No timing oracle: AEAD tag verification is constant-time inside OpenSSL
- Core dumps disabled at startup; mlock attempted (not fatal if denied)
- Blob writes are atomic (write to .tmp, fsync, rename)
"""

from __future__ import annotations

import asyncio
import base64
import json
import logging
import mmap
import os
import struct
import time
from dataclasses import dataclass, field
from enum import Enum
from pathlib import Path
from typing import Any

from cryptography.hazmat.primitives.ciphers.aead import ChaCha20Poly1305
from cryptography.hazmat.primitives.kdf.hkdf import HKDF
from cryptography.hazmat.primitives import hashes
from cryptography.exceptions import InvalidTag

log = logging.getLogger("ozma.key_store")

DATA_DIR = Path(__file__).parent / "key_data"

# ── Optional deps ─────────────────────────────────────────────────────────────

try:
    from argon2.low_level import hash_secret_raw, Type as _Argon2Type
    _HAS_ARGON2 = True
except ImportError:
    _HAS_ARGON2 = False
    log.warning("argon2-cffi not installed — password backup method unavailable")

try:
    from mnemonic import Mnemonic as _Mnemonic
    _mnemo = _Mnemonic("english")
    _HAS_BIP39 = True
except ImportError:
    _HAS_BIP39 = False
    log.warning("mnemonic not installed — BIP39 export will use hex fallback")


# ── BIP39 helpers ─────────────────────────────────────────────────────────────

def bip39_encode(key: bytes) -> list[str]:
    """Encode 32-byte key as 24 BIP39 words (with checksum)."""
    if len(key) != 32:
        raise ValueError("BIP39 encode requires exactly 32 bytes")
    if _HAS_BIP39:
        return _mnemo.to_mnemonic(key).split()
    # Hex fallback (not user-friendly, but functional)
    return [key.hex()]


def bip39_decode(words: list[str]) -> bytes:
    """Decode BIP39 words to 32-byte key; raises ValueError on bad checksum."""
    if _HAS_BIP39:
        phrase = " ".join(words)
        if not _mnemo.check(phrase):
            raise ValueError("Invalid mnemonic — checksum failed or unknown words")
        key = _mnemo.to_entropy(phrase)
        if len(key) != 32:
            raise ValueError(f"Mnemonic decoded to {len(key)} bytes, expected 32")
        return key
    # Hex fallback
    if len(words) != 1:
        raise ValueError("Expected hex fallback (single token)")
    return bytes.fromhex(words[0])


# ── Memory security ───────────────────────────────────────────────────────────

class SecureBytes:
    """
    mmap-backed buffer for key material.

    - mlock() attempted to prevent swapping (requires CAP_IPC_LOCK or ulimit)
    - Zeroed on __del__ and explicit close()
    - Not immune to /proc/self/mem or ptrace by root — mitigate at OS level
    """

    def __init__(self, data: bytes) -> None:
        self._len = len(data)
        self._closed = False
        try:
            self._buf = mmap.mmap(-1, max(self._len, 1))
        except OSError:
            # Fallback: plain bytearray (no mlock, but still zeroed on close)
            self._buf = None
            self._fallback = bytearray(data)
            return
        self._buf.write(data)
        self._buf.seek(0)
        # Attempt mlock — not fatal if denied
        try:
            import ctypes
            import ctypes.util
            libc = ctypes.CDLL(ctypes.util.find_library("c"), use_errno=True)
            addr = ctypes.addressof(ctypes.c_char.from_buffer(self._buf))
            libc.mlock(ctypes.c_void_p(addr), ctypes.c_size_t(self._len))
        except Exception:
            pass  # mlock is best-effort

    def read(self) -> bytes:
        if self._closed:
            raise RuntimeError("SecureBytes already closed")
        if self._buf is None:
            return bytes(self._fallback)
        self._buf.seek(0)
        return self._buf.read(self._len)

    def close(self) -> None:
        if self._closed:
            return
        self._closed = True
        if self._buf is None:
            for i in range(len(self._fallback)):
                self._fallback[i] = 0
            return
        try:
            self._buf.seek(0)
            self._buf.write(b"\x00" * self._len)
            self._buf.close()
        except Exception:
            pass

    def __del__(self) -> None:
        try:
            self.close()
        except Exception:
            pass

    def __bool__(self) -> bool:
        return not self._closed


# ── Enums and exceptions ──────────────────────────────────────────────────────

class BackupMethod(str, Enum):
    PASSWORD = "password"
    EXPORT   = "export"   # BIP39 words encode the key directly
    NONE     = "none"     # ephemeral — key lost on restart


class KeyState(str, Enum):
    UNINITIALISED = "uninitialised"
    LOCKED        = "locked"
    UNLOCKED      = "unlocked"


class KeyLockedError(Exception):
    """Raised by derive_subkey() when the master key is not in memory."""


class KeyNotInitialisedError(Exception):
    """Raised when the key store has no blob yet."""


class UnlockRateLimitedError(Exception):
    """Too many failed attempts — caller must wait before retrying."""
    def __init__(self, retry_after: float) -> None:
        self.retry_after = retry_after
        super().__init__(f"Rate limited — retry after {int(retry_after - time.monotonic())}s")


# ── BYOK session ──────────────────────────────────────────────────────────────

@dataclass
class InjectedSession:
    key:        SecureBytes
    expires_at: float          # monotonic
    actor:      str
    source_ip:  str
    ttl:        int

    @property
    def expired(self) -> bool:
        return time.monotonic() > self.expires_at

    def to_dict(self) -> dict[str, Any]:
        remaining = max(0, int(self.expires_at - time.monotonic()))
        return {
            "actor": self.actor,
            "source_ip": self.source_ip,
            "ttl": self.ttl,
            "seconds_remaining": remaining,
        }


# ── KeyStore ──────────────────────────────────────────────────────────────────

_VERIFY_PLAINTEXT = b"ozma-verify-v1"
_SUBKEY_VERSION   = "v1"


class KeyStore:
    """
    Controller master key store.  Single instance per controller process.
    Thread-safe for asyncio (no locking needed — single-threaded event loop).
    """

    MAX_ATTEMPTS     = 5
    LOCKOUT_SECONDS  = 300
    BYOK_DEFAULT_TTL = 3600
    BYOK_MAX_TTL     = 86400

    # Argon2id defaults — may be calibrated down at init on slow hardware
    ARGON2_T = 3
    ARGON2_M = 131072   # 128 MiB
    ARGON2_P = 4

    def __init__(self, data_dir: Path = DATA_DIR,
                 controller_id: str = "") -> None:
        self._dir = data_dir
        self._dir.mkdir(parents=True, exist_ok=True)
        self._blob_path = self._dir / "key.json"
        self._controller_id = controller_id

        self._master_key:   SecureBytes | None  = None
        self._blob:         dict | None         = None
        self._state:        KeyState            = KeyState.UNINITIALISED
        self._method:       BackupMethod        = BackupMethod.NONE
        self._hkdf_salt:    bytes               = b""
        self._injected:     InjectedSession | None = None

        # Unlock rate limiting
        self._failed_attempts: int   = 0
        self._lockout_until:   float = 0.0   # monotonic

        self._evict_task: asyncio.Task | None = None

        # Disable core dumps — best-effort
        try:
            import resource
            resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
        except Exception:
            pass

        self._load()

    # ── Lifecycle ─────────────────────────────────────────────────────────────

    async def start(self) -> None:
        # Auto-unlock ephemeral key
        if self._method == BackupMethod.NONE and self._state == KeyState.LOCKED:
            self._generate_ephemeral()
        log.info("Key store started: state=%s method=%s",
                 self._state.value, self._method.value)

    async def stop(self) -> None:
        if self._evict_task:
            self._evict_task.cancel()
        self.lock()

    # ── Initialisation ────────────────────────────────────────────────────────

    async def init_password(self, password: str) -> None:
        """
        First-time init with password backup.
        Calibrates Argon2 parameters for this hardware, then generates and
        wraps the master key.  Key is unlocked immediately after init.
        """
        if not _HAS_ARGON2:
            raise RuntimeError("argon2-cffi required for password method — pip install argon2-cffi")
        if not password:
            raise ValueError("Password must not be empty")

        master_key = os.urandom(32)
        hkdf_salt  = os.urandom(32)
        m          = await _calibrate_argon2()

        argon2_salt = os.urandom(16)
        kek = _derive_kek_password(password, argon2_salt,
                                    t=self.ARGON2_T, m=m, p=self.ARGON2_P)
        key_nonce, key_ct = _encrypt(master_key, kek)
        v_nonce, v_ct = _make_verify_blob(master_key, hkdf_salt)

        blob = {
            "version":       1,
            "method":        BackupMethod.PASSWORD.value,
            "controller_id": self._controller_id,
            "hkdf_salt":     _b64(hkdf_salt),
            "verify_nonce":  _b64(v_nonce),
            "verify_ct":     _b64(v_ct),
            "argon2_t":      self.ARGON2_T,
            "argon2_m":      m,
            "argon2_p":      self.ARGON2_P,
            "argon2_salt":   _b64(argon2_salt),
            "key_nonce":     _b64(key_nonce),
            "key_ct":        _b64(key_ct),
        }
        self._commit(blob, master_key, hkdf_salt, BackupMethod.PASSWORD)
        log.info("Key store initialised: method=password argon2_m=%d", m)

    async def init_export(self) -> list[str]:
        """
        First-time init with BIP39 export backup.
        Returns 24 words that ARE the master key.  Display to user once.
        Key is unlocked immediately after init.
        """
        master_key = os.urandom(32)
        hkdf_salt  = os.urandom(32)
        v_nonce, v_ct = _make_verify_blob(master_key, hkdf_salt)

        blob = {
            "version":       1,
            "method":        BackupMethod.EXPORT.value,
            "controller_id": self._controller_id,
            "hkdf_salt":     _b64(hkdf_salt),
            "verify_nonce":  _b64(v_nonce),
            "verify_ct":     _b64(v_ct),
        }
        self._commit(blob, master_key, hkdf_salt, BackupMethod.EXPORT)
        words = bip39_encode(master_key)
        log.info("Key store initialised: method=export")
        return words

    async def init_none(self, confirm: bool = False) -> None:
        """
        Ephemeral mode — key is lost on restart.  Requires explicit confirm=True.
        """
        if not confirm:
            raise ValueError("Pass confirm=True to acknowledge key loss on restart")
        master_key = os.urandom(32)
        hkdf_salt  = os.urandom(32)

        blob = {
            "version":       1,
            "method":        BackupMethod.NONE.value,
            "controller_id": self._controller_id,
            "hkdf_salt":     _b64(hkdf_salt),
        }
        self._commit(blob, master_key, hkdf_salt, BackupMethod.NONE)
        log.info("Key store initialised: method=none (ephemeral)")

    # ── Unlock ────────────────────────────────────────────────────────────────

    async def unlock_password(self, password: str) -> bool:
        """Unlock with password.  Returns True on success, False on wrong password."""
        self._check_rate_limit()
        self._require_blob()   # raises KeyNotInitialisedError if no blob
        if self._state == KeyState.UNLOCKED:
            return True
        if self._method != BackupMethod.PASSWORD:
            raise ValueError(f"Key method is {self._method.value}, not password")
        if not _HAS_ARGON2:
            raise RuntimeError("argon2-cffi not installed")

        blob = self._require_blob()
        argon2_salt = _unb64(blob["argon2_salt"])
        kek = _derive_kek_password(
            password, argon2_salt,
            t=blob["argon2_t"], m=blob["argon2_m"], p=blob["argon2_p"],
        )
        try:
            candidate = _decrypt(_unb64(blob["key_nonce"]), _unb64(blob["key_ct"]), kek)
        except InvalidTag:
            self._record_failure()
            return False

        if not _verify_master_key(candidate, _unb64(blob["hkdf_salt"]),
                                   _unb64(blob["verify_nonce"]),
                                   _unb64(blob["verify_ct"])):
            self._record_failure()
            return False

        self._set_unlocked(candidate, _unb64(blob["hkdf_salt"]))
        self._failed_attempts = 0
        log.info("Key store unlocked (password)")
        return True

    async def unlock_export(self, words: list[str]) -> bool:
        """Unlock with BIP39 words.  Returns True on success."""
        self._check_rate_limit()
        if self._state == KeyState.UNLOCKED:
            return True
        if self._method != BackupMethod.EXPORT:
            raise ValueError(f"Key method is {self._method.value}, not export")

        blob = self._require_blob()
        try:
            candidate = bip39_decode(words)
        except ValueError:
            self._record_failure()
            return False

        if not _verify_master_key(candidate, _unb64(blob["hkdf_salt"]),
                                   _unb64(blob["verify_nonce"]),
                                   _unb64(blob["verify_ct"])):
            self._record_failure()
            return False

        self._set_unlocked(candidate, _unb64(blob["hkdf_salt"]))
        self._failed_attempts = 0
        log.info("Key store unlocked (export words)")
        return True

    def lock(self) -> None:
        """Evict master key from memory.  Clears injected session too."""
        if self._master_key:
            self._master_key.close()
            self._master_key = None
        if self._injected:
            self._injected.key.close()
            self._injected = None
        if self._method != BackupMethod.NONE:
            self._state = KeyState.LOCKED
        log.info("Key store locked")

    # ── BYOK (cloud controller Pro) ───────────────────────────────────────────

    async def inject(self, key_bytes: bytes, ttl: int = BYOK_DEFAULT_TTL,
                     actor: str = "", source_ip: str = "") -> InjectedSession:
        """
        Inject a master key for a bounded session (BYOK cloud controller).

        The key is held in memory only — never persisted.  It is evicted when:
          - TTL expires (background task)
          - lock() is called
          - stop() is called
        """
        if len(key_bytes) != 32:
            raise ValueError("Injected key must be exactly 32 bytes")
        ttl = min(max(ttl, 60), self.BYOK_MAX_TTL)

        # Evict any previous injected session
        if self._injected:
            self._injected.key.close()

        session = InjectedSession(
            key=SecureBytes(key_bytes),
            expires_at=time.monotonic() + ttl,
            actor=actor,
            source_ip=source_ip,
            ttl=ttl,
        )
        self._injected = session

        # Derive hkdf_salt from the injected key (deterministic, no stored blob needed)
        self._hkdf_salt = _hkdf(key_bytes, b"", "ozma:v1:hkdf_salt_derive")
        self._state = KeyState.UNLOCKED

        # Schedule eviction (guard against closed loop in tests)
        if self._evict_task:
            try:
                self._evict_task.cancel()
            except RuntimeError:
                pass
        try:
            self._evict_task = asyncio.create_task(
                self._evict_after(ttl), name="key-byok-evict"
            )
        except RuntimeError:
            self._evict_task = None  # no running loop (e.g. tests)
        log.info("Key injected by %s from %s TTL=%ds", actor, source_ip, ttl)
        return session

    def evict_injected(self) -> None:
        """Explicitly evict BYOK injected session."""
        if self._injected:
            self._injected.key.close()
            self._injected = None
            if self._method == BackupMethod.NONE:
                self._state = KeyState.LOCKED
            elif self._master_key:
                pass  # permanent key still loaded
            else:
                self._state = KeyState.LOCKED
            log.info("Injected key session evicted")

    # ── Subkey derivation ─────────────────────────────────────────────────────

    def derive_subkey(self, purpose: str) -> bytes:
        """
        Derive a purpose-specific 32-byte subkey from the master key.

        Raises KeyLockedError if the master key is not in memory.
        Raises KeyNotInitialisedError if the store has never been initialised.

        Callers should derive on demand and discard the result; do not store
        derived subkeys in long-lived variables.
        """
        if self._state == KeyState.UNINITIALISED:
            raise KeyNotInitialisedError("Key store not initialised")

        # Injected session takes precedence and checks TTL
        if self._injected:
            if self._injected.expired:
                self.evict_injected()
                raise KeyLockedError("Injected key session expired")
            master = self._injected.key.read()
            salt = self._hkdf_salt
            return _hkdf(master, salt, f"ozma:{_SUBKEY_VERSION}:{purpose}")

        if self._state != KeyState.UNLOCKED or self._master_key is None:
            raise KeyLockedError(
                f"Key store is {self._state.value} — unlock before accessing encrypted data"
            )
        master = self._master_key.read()
        return _hkdf(master, self._hkdf_salt, f"ozma:{_SUBKEY_VERSION}:{purpose}")

    # ── Method change ─────────────────────────────────────────────────────────

    async def change_method(self, new_method: BackupMethod,
                             password: str = "",
                             confirm_none: bool = False) -> list[str] | None:
        """
        Change backup method.  Requires the key to be unlocked.

        Transactional: new blob written atomically; old blob preserved until
        write succeeds.  Returns BIP39 words if new_method=export, else None.

        Note: changing method does NOT rotate the master key.  The old method's
        credentials still work against the same underlying key until the caller
        explicitly rotates via rotate_master() (not yet implemented).
        """
        if self._state != KeyState.UNLOCKED:
            raise KeyLockedError("Must be unlocked to change backup method")

        # Derive current master key bytes
        master = self._current_master_bytes()
        hkdf_salt = self._hkdf_salt

        if new_method == BackupMethod.PASSWORD:
            if not password:
                raise ValueError("Password required for password method")
            if not _HAS_ARGON2:
                raise RuntimeError("argon2-cffi not installed")
            m = await _calibrate_argon2()
            argon2_salt = os.urandom(16)
            kek = _derive_kek_password(password, argon2_salt,
                                        t=self.ARGON2_T, m=m, p=self.ARGON2_P)
            key_nonce, key_ct = _encrypt(master, kek)
            v_nonce, v_ct = _make_verify_blob(master, hkdf_salt)
            new_blob = {
                "version":       1,
                "method":        BackupMethod.PASSWORD.value,
                "controller_id": self._controller_id,
                "hkdf_salt":     _b64(hkdf_salt),
                "verify_nonce":  _b64(v_nonce),
                "verify_ct":     _b64(v_ct),
                "argon2_t":      self.ARGON2_T,
                "argon2_m":      m,
                "argon2_p":      self.ARGON2_P,
                "argon2_salt":   _b64(argon2_salt),
                "key_nonce":     _b64(key_nonce),
                "key_ct":        _b64(key_ct),
            }
            self._atomic_save(new_blob)
            self._blob = new_blob
            self._method = BackupMethod.PASSWORD
            log.info("Key method changed to password")
            return None

        elif new_method == BackupMethod.EXPORT:
            v_nonce, v_ct = _make_verify_blob(master, hkdf_salt)
            new_blob = {
                "version":       1,
                "method":        BackupMethod.EXPORT.value,
                "controller_id": self._controller_id,
                "hkdf_salt":     _b64(hkdf_salt),
                "verify_nonce":  _b64(v_nonce),
                "verify_ct":     _b64(v_ct),
            }
            self._atomic_save(new_blob)
            self._blob = new_blob
            self._method = BackupMethod.EXPORT
            words = bip39_encode(master)
            log.info("Key method changed to export")
            return words

        elif new_method == BackupMethod.NONE:
            if not confirm_none:
                raise ValueError("Pass confirm_none=True to acknowledge key loss on restart")
            new_blob = {
                "version":       1,
                "method":        BackupMethod.NONE.value,
                "controller_id": self._controller_id,
                "hkdf_salt":     _b64(hkdf_salt),
            }
            self._atomic_save(new_blob)
            self._blob = new_blob
            self._method = BackupMethod.NONE
            log.info("Key method changed to none (ephemeral)")
            return None

        else:
            raise ValueError(f"Unknown backup method: {new_method}")

    # ── Export words ──────────────────────────────────────────────────────────

    def export_words(self) -> list[str]:
        """Return the BIP39 recovery words.  Requires unlocked state."""
        if self._state != KeyState.UNLOCKED:
            raise KeyLockedError("Must be unlocked to export words")
        return bip39_encode(self._current_master_bytes())

    # ── Connect cloud backup ──────────────────────────────────────────────────

    async def backup_to_connect(self, connect: Any) -> None:
        """
        Push the on-disk key blob to Ozma Connect for account-level backup.

        The blob is already public-safe: the password method stores only an
        Argon2id-protected ciphertext; the export method stores no secret at all
        (the user holds the words); the none method stores no key material.

        Connect stores the blob verbatim.  Even if Connect is compromised the
        attacker still needs the user's password or word list to recover the key.

        Raises:
            KeyNotInitialisedError  — no blob to push
            RuntimeError            — Connect client unavailable or RPC failed
        """
        blob = self._require_blob()
        if connect is None:
            raise RuntimeError("Connect client is not available")
        blob_bytes = json.dumps(blob, indent=2).encode()
        await connect.store_key_blob(
            controller_id=self._controller_id,
            blob=blob_bytes,
        )
        log.info("Key blob pushed to Connect (method=%s, size=%d bytes)",
                 self._method.value, len(blob_bytes))

    async def restore_from_connect(self, connect: Any) -> None:
        """
        Pull the key blob from Ozma Connect and write it locally.

        Used for disaster recovery when the controller's local storage has been
        lost (hardware failure, re-flash).  The blob must still be unlocked by
        the user's password or word list before any subkeys can be derived.

        After restore, call unlock_password() or unlock_export() as normal.

        Raises:
            RuntimeError  — Connect client unavailable, no blob stored, or RPC failed
        """
        if connect is None:
            raise RuntimeError("Connect client is not available")
        blob_bytes = await connect.fetch_key_blob(controller_id=self._controller_id)
        if not blob_bytes:
            raise RuntimeError("No key blob found in Connect for this controller")
        try:
            blob = json.loads(blob_bytes)
        except json.JSONDecodeError as exc:
            raise RuntimeError(f"Key blob from Connect is corrupt: {exc}") from exc
        if blob.get("controller_id") != self._controller_id:
            raise RuntimeError(
                "Key blob controller_id mismatch — this blob belongs to a different controller"
            )
        self._dir.mkdir(parents=True, exist_ok=True)
        self._atomic_save(blob)
        # Reload from freshly written file
        self._blob      = blob
        self._method    = BackupMethod(blob.get("method", "none"))
        self._hkdf_salt = _unb64(blob["hkdf_salt"]) if "hkdf_salt" in blob else b""
        self._state     = KeyState.LOCKED
        log.info("Key blob restored from Connect (method=%s) — unlock to proceed",
                 self._method.value)

    def connect_backup_enabled(self) -> bool:
        """True if the current blob supports Connect backup (not the none method)."""
        return self._blob is not None and self._method != BackupMethod.NONE

    # ── Status ────────────────────────────────────────────────────────────────

    def get_status(self) -> dict[str, Any]:
        d: dict[str, Any] = {
            "state":     self._state.value,
            "method":    self._method.value,
            "has_blob":  self._blob is not None,
            "failed_attempts": self._failed_attempts,
            "rate_limited": time.monotonic() < self._lockout_until,
        }
        if self._injected and not self._injected.expired:
            d["injected_session"] = self._injected.to_dict()
        else:
            d["injected_session"] = None
        if self._blob:
            d["controller_id"] = self._blob.get("controller_id", "")
        return d

    # ── Internal helpers ──────────────────────────────────────────────────────

    def _require_blob(self) -> dict:
        if not self._blob:
            raise KeyNotInitialisedError("No key blob found — run init first")
        return self._blob

    def _current_master_bytes(self) -> bytes:
        if self._injected and not self._injected.expired:
            return self._injected.key.read()
        if self._master_key:
            return self._master_key.read()
        raise KeyLockedError("No master key in memory")

    def _generate_ephemeral(self) -> None:
        master_key = os.urandom(32)
        self._set_unlocked(master_key, _unb64(self._blob["hkdf_salt"])
                           if self._blob else os.urandom(32))

    def _set_unlocked(self, master_key: bytes, hkdf_salt: bytes) -> None:
        if self._master_key:
            self._master_key.close()
        self._master_key = SecureBytes(master_key)
        self._hkdf_salt  = hkdf_salt
        self._state      = KeyState.UNLOCKED

    def _commit(self, blob: dict, master_key: bytes,
                hkdf_salt: bytes, method: BackupMethod) -> None:
        """Write blob to disk and set in-memory state."""
        self._atomic_save(blob)
        self._blob   = blob
        self._method = method
        self._set_unlocked(master_key, hkdf_salt)

    def _atomic_save(self, blob: dict) -> None:
        tmp = self._blob_path.with_suffix(".tmp")
        tmp.write_text(json.dumps(blob, indent=2))
        try:
            tmp.chmod(0o600)
            os.fsync(tmp.open().fileno())
        except Exception:
            pass
        tmp.rename(self._blob_path)

    def _load(self) -> None:
        if not self._blob_path.exists():
            return
        try:
            blob = json.loads(self._blob_path.read_text())
        except Exception as exc:
            log.error("Failed to load key blob: %s", exc)
            return

        self._blob      = blob
        self._method    = BackupMethod(blob.get("method", "none"))
        self._hkdf_salt = _unb64(blob["hkdf_salt"]) if "hkdf_salt" in blob else b""
        self._state     = KeyState.LOCKED
        log.debug("Key blob loaded: method=%s", self._method.value)

    def _check_rate_limit(self) -> None:
        if self._lockout_until and time.monotonic() < self._lockout_until:
            raise UnlockRateLimitedError(self._lockout_until)

    def _record_failure(self) -> None:
        self._failed_attempts += 1
        if self._failed_attempts >= self.MAX_ATTEMPTS:
            self._lockout_until = time.monotonic() + self.LOCKOUT_SECONDS
            log.warning("Key unlock rate limited after %d failures (%ds lockout)",
                        self._failed_attempts, self.LOCKOUT_SECONDS)

    async def _evict_after(self, ttl: int) -> None:
        await asyncio.sleep(ttl)
        self.evict_injected()


# ── Pure crypto helpers ───────────────────────────────────────────────────────

def _b64(data: bytes) -> str:
    return base64.b64encode(data).decode()


def _unb64(s: str) -> bytes:
    return base64.b64decode(s)


def _hkdf(ikm: bytes, salt: bytes, info: str) -> bytes:
    return HKDF(
        algorithm=hashes.SHA256(),
        length=32,
        salt=salt or None,
        info=info.encode(),
    ).derive(ikm)


def _encrypt(plaintext: bytes, key: bytes) -> tuple[bytes, bytes]:
    """Returns (nonce, ciphertext+tag)."""
    nonce = os.urandom(12)
    ct = ChaCha20Poly1305(key).encrypt(nonce, plaintext, b"")
    return nonce, ct


def _decrypt(nonce: bytes, ct_tag: bytes, key: bytes) -> bytes:
    """Raises InvalidTag on wrong key."""
    return ChaCha20Poly1305(key).decrypt(nonce, ct_tag, b"")


def _make_verify_blob(master_key: bytes, hkdf_salt: bytes) -> tuple[bytes, bytes]:
    """Return (nonce, ciphertext+tag) for the key-verification AEAD blob."""
    verify_key = _hkdf(master_key, hkdf_salt, f"ozma:{_SUBKEY_VERSION}:verify")
    return _encrypt(_VERIFY_PLAINTEXT, verify_key)


def _verify_master_key(candidate: bytes, hkdf_salt: bytes,
                        v_nonce: bytes, v_ct: bytes) -> bool:
    """Verify candidate master key against stored verification blob."""
    verify_key = _hkdf(candidate, hkdf_salt, f"ozma:{_SUBKEY_VERSION}:verify")
    try:
        plaintext = _decrypt(v_nonce, v_ct, verify_key)
        return plaintext == _VERIFY_PLAINTEXT
    except InvalidTag:
        return False


def _derive_kek_password(password: str, salt: bytes,
                          t: int, m: int, p: int) -> bytes:
    if not _HAS_ARGON2:
        raise RuntimeError("argon2-cffi not installed")
    return hash_secret_raw(
        secret=password.encode(),
        salt=salt,
        time_cost=t,
        memory_cost=m,
        parallelism=p,
        hash_len=32,
        type=_Argon2Type.ID,
    )


async def _calibrate_argon2(target_ms: int = 1500) -> int:
    """
    Return the largest Argon2id memory_cost (KiB) that completes in target_ms.
    Defaults to 131072 (128 MiB) if calibration cannot run.
    """
    if not _HAS_ARGON2:
        return 65536
    import time as _time
    test_pw = b"calibration-test"
    test_salt = os.urandom(16)
    best = 65536
    for m in [65536, 131072, 262144]:
        t0 = _time.monotonic()
        try:
            _derive_kek_password(test_pw.decode(), test_salt, t=3, m=m, p=4)
        except Exception:
            break
        elapsed_ms = (_time.monotonic() - t0) * 1000
        if elapsed_ms <= target_ms:
            best = m
        else:
            break
    log.debug("Argon2 calibration: selected m=%d KiB", best)
    return best
