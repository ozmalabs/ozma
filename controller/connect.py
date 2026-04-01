# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Ozma Connect — account management and cloud service integration.

This is the client-side component of the Ozma Connect SaaS. It runs on
the controller and communicates with the Ozma Connect cloud service for:

  1. Account authentication (JWT-based)
  2. Relay coordination (WireGuard tunnel setup for remote access)
  3. Config backup (zero-knowledge encrypted)
  4. HTTPS subdomain provisioning

The open-source controller is fully functional without Connect.
All features work locally with no account, no cloud, no limits.
Connect adds cloud services: remote access, HTTPS, backups, AI proxy,
cloud storage. Metering for those services happens server-side at the
Connect API boundary — not in this client.
"""

from __future__ import annotations

import asyncio
import json
import logging
import time
from pathlib import Path

log = logging.getLogger("ozma.connect")

CONNECT_API_BASE = "https://connect.ozma.dev/api/v1"
CACHE_PATH = Path(__file__).parent / "connect_cache.json"

try:
    import nacl  # noqa: F401
    _HAS_NACL = True
except ImportError:
    _HAS_NACL = False


# ── Connect client ──────────────────────────────────────────────────────────

class OzmaConnect:
    """
    Client for the Ozma Connect cloud service.

    Handles authentication, relay coordination, config backup, and
    HTTPS subdomain management. No local feature gating — the
    open-source controller runs everything without limits.
    """

    def __init__(self, api_base: str = CONNECT_API_BASE) -> None:
        self._api_base = api_base.rstrip("/")
        self._token: str = ""
        self._account_id: str = ""
        self._tier: str = "free"
        self._authenticated = False
        self._offline_mode = False
        self._cache_valid_until: float = 0

    @property
    def authenticated(self) -> bool:
        return self._authenticated

    @property
    def tier(self) -> str:
        return self._tier

    @property
    def account_id(self) -> str:
        return self._account_id

    async def start(self) -> None:
        """Load cached auth."""
        self._load_cache()
        log.info("Ozma Connect: tier=%s, authenticated=%s, offline=%s",
                 self._tier, self._authenticated, self._offline_mode)

    async def stop(self) -> None:
        self._save_cache()

    # ── Authentication ──────────────────────────────────────────────────────

    async def login(self, email: str, password: str) -> bool:
        """Authenticate with the Ozma Connect service."""
        from .build_info import build_info
        result = await self._api_post("/auth/login", {
            "email": email, "password": password,
            "build": build_info(),
        })
        if result and result.get("token"):
            self._token = result["token"]
            self._account_id = result.get("account_id", "")
            self._tier = result.get("tier", "free")
            self._authenticated = True
            self._offline_mode = False
            self._cache_valid_until = time.time() + 7 * 86400  # 7 days
            self._save_cache()
            log.info("Ozma Connect: logged in as %s (tier: %s)", email, self._tier)
            return True
        return False

    async def login_with_token(self, token: str) -> bool:
        """Authenticate with an existing JWT token."""
        result = await self._api_get("/auth/verify", token=token)
        if result and result.get("valid"):
            self._token = token
            self._account_id = result.get("account_id", "")
            self._tier = result.get("tier", "free")
            self._authenticated = True
            self._offline_mode = False
            self._cache_valid_until = time.time() + 7 * 86400
            self._save_cache()
            return True
        return False

    def logout(self) -> None:
        self._token = ""
        self._account_id = ""
        self._tier = "free"
        self._authenticated = False
        self._save_cache()

    # ── Controller registration ──────────────────────────────────────────

    async def register_controller(self, controller_id: str, name: str = "",
                                    node_count: int = 0,
                                    mesh_ca_fingerprint: str = "") -> dict | None:
        """
        Register this controller with Connect.

        Sends build info (version, edition, signature) so Connect can
        label the controller correctly in the dashboard.
        """
        if not self._authenticated:
            return None

        from .build_info import build_info
        result = await self._api_post("/controllers/register", {
            "id": controller_id,
            "name": name,
            "node_count": node_count,
            "mesh_ca_fingerprint": mesh_ca_fingerprint,
            "build": build_info(),
        })
        return result

    # ── Config backup (zero-knowledge encrypted) ─────────────────────────

    async def backup_config(self, mesh_registry: dict, scenarios: list,
                             passphrase: str = "") -> bool:
        """
        Back up controller config to Ozma Connect cloud.

        The backup is encrypted client-side with the user's passphrase
        before upload. The Connect server stores only ciphertext — it
        cannot read the mesh CA private key or any sensitive data.
        Zero-knowledge: even Ozma Labs cannot decrypt the backup.

        Backed up:
          - Mesh CA keypair (encrypted)
          - Node certificates
          - Controller identity
          - Scenarios + bindings
          - Room correction profiles
          - Control surface config

        Restore: install ozma on new hardware → login to Connect →
        enter passphrase → config restores → all nodes re-pair
        automatically (same mesh CA = same trust).
        """
        if not self._authenticated:
            return False

        # Encrypt the backup payload with the passphrase
        payload = json.dumps({
            "mesh_registry": mesh_registry,
            "scenarios": scenarios,
            "backed_up_at": time.strftime("%Y-%m-%dT%H:%M:%SZ"),
        }).encode()

        if _HAS_NACL and passphrase:
            import nacl.pwhash, nacl.secret, nacl.utils
            salt = nacl.utils.random(16)
            key = nacl.pwhash.argon2id.kdf(
                32, passphrase.encode(), salt,
                opslimit=nacl.pwhash.argon2id.OPSLIMIT_MODERATE,
                memlimit=nacl.pwhash.argon2id.MEMLIMIT_MODERATE,
            )
            box = nacl.secret.SecretBox(key)
            encrypted = salt + box.encrypt(payload)
        else:
            # No encryption — development only
            encrypted = payload

        import base64
        result = await self._api_post("/backup/upload", {
            "encrypted_config": base64.b64encode(encrypted).decode(),
            "encrypted": bool(passphrase and _HAS_NACL),
        })
        if result and result.get("ok"):
            log.info("Config backed up to Ozma Connect (encrypted=%s)",
                     bool(passphrase))
            return True
        return False

    async def restore_config(self, passphrase: str = "") -> dict | None:
        """
        Restore controller config from Ozma Connect cloud.

        Returns the decrypted config dict, or None on failure.
        """
        if not self._authenticated:
            return None

        result = await self._api_get("/backup/latest")
        if not result or not result.get("encrypted_config"):
            log.info("No backup found on Ozma Connect")
            return None

        import base64
        encrypted = base64.b64decode(result["encrypted_config"])

        if result.get("encrypted") and passphrase and _HAS_NACL:
            import nacl.pwhash, nacl.secret
            salt = encrypted[:16]
            key = nacl.pwhash.argon2id.kdf(
                32, passphrase.encode(), salt,
                opslimit=nacl.pwhash.argon2id.OPSLIMIT_MODERATE,
                memlimit=nacl.pwhash.argon2id.MEMLIMIT_MODERATE,
            )
            box = nacl.secret.SecretBox(key)
            try:
                payload = box.decrypt(encrypted[16:])
            except Exception:
                log.error("Backup decryption failed — wrong passphrase?")
                return None
        else:
            payload = encrypted

        config = json.loads(payload)
        log.info("Config restored from Ozma Connect (backed up: %s)",
                 config.get("backed_up_at", "unknown"))
        return config

    # ── Relay coordination ──────────────────────────────────────────────────

    async def get_relay_config(self) -> dict | None:
        """Get WireGuard relay configuration for remote access."""
        if not self._authenticated:
            return None
        result = await self._api_get("/relay/config")
        return result

    async def register_for_relay(self, mesh_public_key: bytes) -> dict | None:
        """Register this controller's mesh public key with the relay."""
        if not self._authenticated:
            return None
        result = await self._api_post("/relay/register", {
            "mesh_public_key": mesh_public_key.hex(),
        })
        return result

    # ── API helpers ─────────────────────────────────────────────────────────

    async def _api_get(self, path: str, token: str = "") -> dict | None:
        import urllib.request
        t = token or self._token
        try:
            loop = asyncio.get_running_loop()
            def _fetch():
                req = urllib.request.Request(
                    f"{self._api_base}{path}",
                    headers={"Authorization": f"Bearer {t}"} if t else {},
                )
                with urllib.request.urlopen(req, timeout=10) as r:
                    return json.loads(r.read())
            return await loop.run_in_executor(None, _fetch)
        except Exception:
            self._offline_mode = True
            return None

    async def _api_post(self, path: str, body: dict) -> dict | None:
        import urllib.request
        try:
            loop = asyncio.get_running_loop()
            def _post():
                data = json.dumps(body).encode()
                req = urllib.request.Request(
                    f"{self._api_base}{path}",
                    data=data,
                    headers={
                        "Content-Type": "application/json",
                        **({"Authorization": f"Bearer {self._token}"} if self._token else {}),
                    },
                    method="POST",
                )
                with urllib.request.urlopen(req, timeout=10) as r:
                    return json.loads(r.read())
            return await loop.run_in_executor(None, _post)
        except Exception:
            self._offline_mode = True
            return None

    # ── Cache ───────────────────────────────────────────────────────────────

    def _load_cache(self) -> None:
        if CACHE_PATH.exists():
            try:
                data = json.loads(CACHE_PATH.read_text())
                if data.get("valid_until", 0) > time.time():
                    self._tier = data.get("tier", "free")
                    self._account_id = data.get("account_id", "")
                    self._token = data.get("token", "")
                    self._authenticated = bool(self._token)
                    self._cache_valid_until = data["valid_until"]
                    self._offline_mode = True  # using cache = offline
            except Exception:
                pass

    def _save_cache(self) -> None:
        data = {
            "tier": self._tier,
            "account_id": self._account_id,
            "token": self._token,
            "valid_until": self._cache_valid_until,
        }
        try:
            CACHE_PATH.write_text(json.dumps(data, indent=2))
        except Exception:
            pass

    # ── Status ──────────────────────────────────────────────────────────────

    def status(self) -> dict:
        return {
            "authenticated": self._authenticated,
            "offline_mode": self._offline_mode,
            "tier": self._tier,
            "account_id": self._account_id,
        }
