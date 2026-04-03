# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Cloud backup — Microsoft 365 and Google Workspace backup via vendor delta APIs.

Architecture
============

                    ┌─────────────────────────────────┐
                    │         CloudBackupManager        │
                    │                                   │
  schedule/API ────►│  BackupQueue (priority queue)     │
                    │         │                         │
                    │  WorkerPool (N concurrent)        │
                    │    Worker ──► RateLimiter ──►     │
                    │    Worker      (per provider,     │
                    │    Worker       per tenant)       │
                    │         │                         │
                    │  CredentialStore (encrypted)      │
                    │  DeltaStateStore (checkpoints)    │
                    └───────────┬─────────────────────-─┘
                                │
              ┌─────────────────┴──────────────────┐
              │                                     │
    M365BackupAgent                    GoogleWorkspaceBackupAgent
    (Graph API delta)                  (Admin SDK + Drive changes)
    • Mailboxes (.eml)                 • Gmail (RFC2822)
    • OneDrive (files)                 • Drive (files + exports)
    • SharePoint (files)               • Shared Drives
    • Teams chat (future)              • Calendar/Contacts (future)

Key design decisions
====================

Queue
  asyncio.PriorityQueue[(priority, seq, BackupJob)].  Priority 0 = highest.
  Scheduled jobs: priority 1.  Retries: priority 2 + attempt count.
  Each job carries a delta token checkpoint so restarts resume mid-user.

Worker pool
  Global semaphore caps total concurrent workers (default: 4).
  Per-provider sub-semaphore caps concurrent workers per provider (default: 2).
  This prevents one provider monopolising all workers.

Rate limiter
  Token-bucket per (provider, tenant_id).  Replenishes at configured rate.
  On HTTP 429: reads Retry-After header, pauses that (provider, tenant) bucket
  for the specified duration — other tenants / providers continue unaffected.
  Separate read/write buckets so restore traffic never blocks backup.

Credential store
  AES-256-GCM encrypted JSON, key derived from mesh CA private key via HKDF.
  Falls back to plaintext with 0o600 permissions if nacl unavailable.
  Background task refreshes OAuth tokens 10 min before expiry.
  Alerts (via state.events) if a credential will expire within 7 days and
  cannot be auto-refreshed (e.g. service account key rotation required).

Delta state
  Persisted to cloud_backup_state.json after every successful job.
  Keys: (source_type, tenant_id, user_id) → {delta_token, history_id, ...}
  On restart, workers pick up from the last checkpoint.

Persistence
  cloud_backup_config.json   — enabled sources, schedules, retention policy
  cloud_backup_state.json    — delta tokens, last-run timestamps, job stats
  cloud_backup_credentials.json — encrypted credentials (mode 0o600)
"""

from __future__ import annotations

import asyncio
import base64
import hashlib
import hmac
import itertools
import json
import logging
import os
import time
import uuid
from dataclasses import dataclass, field
from enum import Enum
from pathlib import Path
from typing import Any, Callable

log = logging.getLogger("ozma.cloud_backup")

# ── Constants ─────────────────────────────────────────────────────────────────

_DEFAULT_WORKERS        = 4     # total concurrent backup workers
_MAX_WORKERS_PER_PROV   = 2     # max workers per provider (M365 or Google)
_MAX_JOB_ATTEMPTS       = 5     # dead-letter after this many failures
_RETRY_BASE_DELAY       = 60.0  # base retry delay in seconds (doubles each attempt)
_TOKEN_REFRESH_MARGIN   = 600   # refresh OAuth tokens 10 min before expiry
_CRED_EXPIRY_WARN_DAYS  = 7
_HEALTH_INTERVAL        = 60.0
_SCHEDULE_INTERVAL      = 300.0  # check for due scheduled jobs every 5 min
_RESULT_POLL_INTERVAL   = 30.0   # poll ConnectResultChannel every 30s

# Graph API rate limits (conservative — actual limits are tenant-dependent)
_M365_READ_RATE   = 20.0   # requests/second across all workers for this tenant
_M365_WRITE_RATE  = 5.0
# Google API limits
_GOOGLE_READ_RATE = 10.0   # units/second per user; we use per-tenant global limit
_GOOGLE_WRITE_RATE = 2.0


# ── Enums ─────────────────────────────────────────────────────────────────────

class Provider(str, Enum):
    M365   = "m365"
    GOOGLE = "google"
    RCLONE = "rclone"


class JobType(str, Enum):
    M365_MAILBOX    = "m365_mailbox"
    M365_ONEDRIVE   = "m365_onedrive"
    M365_SHAREPOINT = "m365_sharepoint"
    GOOGLE_GMAIL    = "google_gmail"
    GOOGLE_DRIVE    = "google_drive"
    GOOGLE_SHARED_DRIVES = "google_shared_drives"
    RCLONE_SYNC     = "rclone_sync"


class JobStatus(str, Enum):
    QUEUED      = "queued"
    RUNNING     = "running"
    DONE        = "done"
    FAILED      = "failed"
    DEAD_LETTER = "dead_letter"


# ── Data model ────────────────────────────────────────────────────────────────

@dataclass
class BackupJob:
    id: str = field(default_factory=lambda: str(uuid.uuid4()))
    job_type: JobType = JobType.M365_MAILBOX
    provider: Provider = Provider.M365
    tenant_id: str = ""          # credential set identifier
    user_id: str = ""            # user UPN / email
    priority: int = 1            # 0 = highest, higher = lower priority
    created_at: float = field(default_factory=time.time)
    attempts: int = 0
    next_retry_at: float = 0.0
    status: JobStatus = JobStatus.QUEUED
    last_error: str = ""
    items_backed_up: int = 0
    bytes_backed_up: int = 0
    started_at: float = 0.0
    finished_at: float = 0.0
    meta: dict[str, Any] = field(default_factory=dict)  # provider-specific params

    def retry_delay(self) -> float:
        """Exponential backoff: 60s, 120s, 240s, 480s, 960s."""
        return _RETRY_BASE_DELAY * (2 ** min(self.attempts, 4))

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id,
            "job_type": self.job_type,
            "provider": self.provider,
            "tenant_id": self.tenant_id,
            "user_id": self.user_id,
            "priority": self.priority,
            "created_at": self.created_at,
            "attempts": self.attempts,
            "status": self.status,
            "last_error": self.last_error,
            "items_backed_up": self.items_backed_up,
            "bytes_backed_up": self.bytes_backed_up,
            "started_at": self.started_at,
            "finished_at": self.finished_at,
            "meta": self.meta,
        }

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> "BackupJob":
        job = cls(
            id=d.get("id", str(uuid.uuid4())),
            job_type=JobType(d["job_type"]),
            provider=Provider(d["provider"]),
            tenant_id=d.get("tenant_id", ""),
            user_id=d.get("user_id", ""),
            priority=d.get("priority", 1),
            created_at=d.get("created_at", time.time()),
            attempts=d.get("attempts", 0),
            status=JobStatus(d.get("status", JobStatus.QUEUED)),
            last_error=d.get("last_error", ""),
            items_backed_up=d.get("items_backed_up", 0),
            bytes_backed_up=d.get("bytes_backed_up", 0),
            started_at=d.get("started_at", 0.0),
            finished_at=d.get("finished_at", 0.0),
            meta=d.get("meta", {}),
        )
        return job


@dataclass
class BackupSource:
    """Configuration for one cloud tenant to back up."""
    id: str = field(default_factory=lambda: str(uuid.uuid4()))
    name: str = ""               # display name, e.g. "Acme Corp M365"
    provider: Provider = Provider.M365
    tenant_id: str = ""          # Azure tenant ID or Google customer ID
    enabled: bool = True
    backup_mail: bool = True
    backup_files: bool = True
    backup_sharepoint: bool = False   # M365 only; potentially large
    schedule_cron: str = "0 2 * * *"  # daily at 02:00
    retention_days: int = 90
    last_run_at: float = 0.0
    last_run_status: str = ""
    # rclone-specific (only used when provider == Provider.RCLONE)
    rclone_remote: str = ""          # remote name in rclone.conf
    rclone_source_path: str = "/"    # path on the remote to back up
    rclone_flags: list[str] = field(default_factory=list)  # extra rclone flags
    tpslimit: float = 0.0            # max transactions/second (0 = unlimited)
    bwlimit: str = ""                # bandwidth limit, e.g. "10M" or "1G"

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id,
            "name": self.name,
            "provider": self.provider,
            "tenant_id": self.tenant_id,
            "enabled": self.enabled,
            "backup_mail": self.backup_mail,
            "backup_files": self.backup_files,
            "backup_sharepoint": self.backup_sharepoint,
            "schedule_cron": self.schedule_cron,
            "retention_days": self.retention_days,
            "last_run_at": self.last_run_at,
            "last_run_status": self.last_run_status,
            "rclone_remote": self.rclone_remote,
            "rclone_source_path": self.rclone_source_path,
            "rclone_flags": self.rclone_flags,
            "tpslimit": self.tpslimit,
            "bwlimit": self.bwlimit,
        }

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> "BackupSource":
        src = cls()
        for k, v in d.items():
            if hasattr(src, k):
                setattr(src, k, v)
        return src


# ── Rate limiter ──────────────────────────────────────────────────────────────

class RateLimiter:
    """
    Token-bucket rate limiter per (provider, tenant_id, read|write).

    On HTTP 429 with Retry-After: call pause(seconds) to block that bucket
    until the retry window passes.  Other (provider, tenant) pairs continue
    unaffected.
    """

    def __init__(self, rate: float) -> None:
        self._rate = rate          # tokens/second
        self._tokens = rate        # start full
        self._last_fill = time.monotonic()
        self._lock = asyncio.Lock()
        self._paused_until: float = 0.0

    async def acquire(self) -> None:
        """Block until a token is available."""
        while True:
            async with self._lock:
                now = time.monotonic()
                # Honour explicit pause (Retry-After)
                if now < self._paused_until:
                    wait = self._paused_until - now
                else:
                    # Refill tokens
                    elapsed = now - self._last_fill
                    self._tokens = min(self._rate, self._tokens + elapsed * self._rate)
                    self._last_fill = now
                    if self._tokens >= 1.0:
                        self._tokens -= 1.0
                        return
                    wait = (1.0 - self._tokens) / self._rate

            await asyncio.sleep(wait)

    def pause(self, seconds: float) -> None:
        """Signal a 429 — pause this bucket for `seconds`."""
        self._paused_until = time.monotonic() + seconds
        self._tokens = 0.0
        log.warning("Rate limiter paused for %.1fs (Retry-After)", seconds)


class RateLimiterRegistry:
    """Per-(provider, tenant_id, mode) rate limiter pool."""

    _RATES: dict[tuple[Provider, str], tuple[float, float]] = {
        # (read_rate, write_rate)
        (Provider.M365, "read"):   (_M365_READ_RATE, _M365_READ_RATE),
        (Provider.GOOGLE, "read"): (_GOOGLE_READ_RATE, _GOOGLE_READ_RATE),
    }

    def __init__(self) -> None:
        self._limiters: dict[tuple, RateLimiter] = {}

    def get(self, provider: Provider, tenant_id: str, mode: str = "read") -> RateLimiter:
        key = (provider, tenant_id, mode)
        if key not in self._limiters:
            rate = _M365_READ_RATE if provider == Provider.M365 else _GOOGLE_READ_RATE
            if mode == "write":
                rate = _M365_WRITE_RATE if provider == Provider.M365 else _GOOGLE_WRITE_RATE
            self._limiters[key] = RateLimiter(rate)
        return self._limiters[key]

    def pause(self, provider: Provider, tenant_id: str,
              mode: str, retry_after: float) -> None:
        self.get(provider, tenant_id, mode).pause(retry_after)


# ── Credential store ──────────────────────────────────────────────────────────

@dataclass
class CredentialRecord:
    id: str                          # matches BackupSource.id
    provider: Provider = Provider.M365
    # M365 app credentials
    tenant_id: str = ""
    client_id: str = ""
    client_secret: str = ""          # stored encrypted
    # Google service account
    service_account_json: str = ""   # JSON key, stored encrypted
    admin_email: str = ""            # for domain-wide delegation
    # Cached OAuth token (refreshed automatically)
    access_token: str = ""
    token_expires_at: float = 0.0
    refresh_token: str = ""
    # Lifecycle
    created_at: float = field(default_factory=time.time)
    last_rotated_at: float = 0.0
    expires_at: float = 0.0          # 0 = no expiry (service accounts)

    def needs_refresh(self) -> bool:
        if not self.access_token:
            return True
        return time.time() >= (self.token_expires_at - _TOKEN_REFRESH_MARGIN)

    def to_dict(self) -> dict[str, Any]:
        """Serialise — sensitive fields included (caller must encrypt)."""
        return {
            "id": self.id,
            "provider": self.provider,
            "tenant_id": self.tenant_id,
            "client_id": self.client_id,
            "client_secret": self.client_secret,
            "service_account_json": self.service_account_json,
            "admin_email": self.admin_email,
            "access_token": self.access_token,
            "token_expires_at": self.token_expires_at,
            "refresh_token": self.refresh_token,
            "created_at": self.created_at,
            "last_rotated_at": self.last_rotated_at,
            "expires_at": self.expires_at,
        }

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> "CredentialRecord":
        rec = cls(id=d.get("id", ""))
        for k, v in d.items():
            if hasattr(rec, k):
                setattr(rec, k, v)
        return rec


def _derive_key(mesh_key_bytes: bytes | None) -> bytes:
    """Derive a 32-byte AES key from the mesh CA private key via HKDF-SHA256."""
    ikm = mesh_key_bytes or os.urandom(32)
    # HKDF extract + expand (manual, no external dep)
    salt = b"ozma-cloud-backup-creds-v1"
    prk = hmac.new(salt, ikm, hashlib.sha256).digest()
    info = b"aes256gcm encryption key"
    okm = hmac.new(prk, info + b"\x01", hashlib.sha256).digest()
    return okm


def _encrypt_json(data: dict, key: bytes) -> bytes:
    """AES-256-GCM encrypt JSON.  Returns nonce(12) + tag(16) + ciphertext."""
    try:
        from cryptography.hazmat.primitives.ciphers.aead import AESGCM
        nonce = os.urandom(12)
        ct = AESGCM(key).encrypt(nonce, json.dumps(data).encode(), None)
        return nonce + ct
    except ImportError:
        # Fallback: base64-encoded plaintext (still mode 0o600)
        log.warning("cryptography package unavailable — credentials stored unencrypted")
        return b"PLAIN:" + json.dumps(data).encode()


def _decrypt_json(data: bytes, key: bytes) -> dict:
    if data.startswith(b"PLAIN:"):
        return json.loads(data[6:])
    try:
        from cryptography.hazmat.primitives.ciphers.aead import AESGCM
        nonce, ct = data[:12], data[12:]
        return json.loads(AESGCM(key).decrypt(nonce, ct, None))
    except Exception as e:
        raise ValueError(f"Failed to decrypt credentials: {e}") from e


class CredentialStore:
    """
    Encrypted credential storage for cloud backup sources.

    Credentials are AES-256-GCM encrypted using a key derived from the
    controller's mesh CA private key.  The credential file is created with
    mode 0o600 and is never logged.
    """

    def __init__(self, path: Path, mesh_key_bytes: bytes | None = None) -> None:
        self._path = path
        self._key = _derive_key(mesh_key_bytes)
        self._records: dict[str, CredentialRecord] = {}
        self._load()

    def _load(self) -> None:
        if not self._path.exists():
            return
        try:
            raw = self._path.read_bytes()
            data = _decrypt_json(raw, self._key)
            for d in data.get("credentials", []):
                rec = CredentialRecord.from_dict(d)
                self._records[rec.id] = rec
        except Exception as e:
            log.error("Failed to load cloud backup credentials: %s", e)

    def _save(self) -> None:
        data = {"credentials": [r.to_dict() for r in self._records.values()]}
        encrypted = _encrypt_json(data, self._key)
        # Write to temp then rename for atomicity
        tmp = self._path.with_suffix(".tmp")
        tmp.touch(mode=0o600)
        tmp.write_bytes(encrypted)
        tmp.rename(self._path)
        self._path.chmod(0o600)

    def store(self, rec: CredentialRecord) -> None:
        self._records[rec.id] = rec
        self._save()

    def get(self, source_id: str) -> CredentialRecord | None:
        return self._records.get(source_id)

    def delete(self, source_id: str) -> None:
        self._records.pop(source_id, None)
        self._save()

    def all_records(self) -> list[CredentialRecord]:
        return list(self._records.values())

    def update_token(self, source_id: str, token: str, expires_at: float) -> None:
        rec = self._records.get(source_id)
        if rec:
            rec.access_token = token
            rec.token_expires_at = expires_at
            self._save()


# ── Delta state store ─────────────────────────────────────────────────────────

class DeltaStateStore:
    """
    Persists delta tokens and checkpoints so backup jobs resume after restart.

    Key: (source_id, job_type, user_id)
    Value: dict with delta_token, history_id, page_token, last_synced_at, etc.
    """

    def __init__(self, path: Path) -> None:
        self._path = path
        self._state: dict[str, Any] = {}
        self._load()

    def _load(self) -> None:
        if self._path.exists():
            try:
                self._state = json.loads(self._path.read_text())
            except Exception as e:
                log.warning("Failed to load delta state: %s", e)

    def _save(self) -> None:
        tmp = self._path.with_suffix(".tmp")
        tmp.write_text(json.dumps(self._state, indent=2))
        tmp.rename(self._path)

    def _key(self, source_id: str, job_type: str, user_id: str) -> str:
        return f"{source_id}:{job_type}:{user_id}"

    def get(self, source_id: str, job_type: str, user_id: str) -> dict[str, Any]:
        return self._state.get(self._key(source_id, job_type, user_id), {})

    def set(self, source_id: str, job_type: str, user_id: str,
            checkpoint: dict[str, Any]) -> None:
        self._state[self._key(source_id, job_type, user_id)] = {
            **checkpoint,
            "last_synced_at": time.time(),
        }
        self._save()

    def clear(self, source_id: str) -> None:
        keys = [k for k in self._state if k.startswith(f"{source_id}:")]
        for k in keys:
            del self._state[k]
        self._save()


# ── HTTP helpers ──────────────────────────────────────────────────────────────

class ThrottledHTTP:
    """
    Thin async HTTP client wrapper that enforces rate limits and handles 429.

    Uses urllib (no extra deps) with run_in_executor for async compat.
    Callers should prefer httpx/aiohttp if available — this is the fallback.
    """

    def __init__(self, limiter: RateLimiter) -> None:
        self._limiter = limiter
        self._loop: asyncio.AbstractEventLoop | None = None

    async def get(self, url: str, headers: dict[str, str],
                  timeout: float = 30.0) -> tuple[int, dict, bytes]:
        """GET with rate limiting.  Returns (status, headers, body)."""
        await self._limiter.acquire()
        return await self._do_request("GET", url, headers, None, timeout)

    async def _do_request(self, method: str, url: str, headers: dict[str, str],
                          body: bytes | None, timeout: float) -> tuple[int, dict, bytes]:
        import urllib.request
        import urllib.error
        loop = asyncio.get_event_loop()

        def _sync():
            req = urllib.request.Request(url, data=body, headers=headers, method=method)
            try:
                with urllib.request.urlopen(req, timeout=timeout) as r:
                    return r.status, dict(r.headers), r.read()
            except urllib.error.HTTPError as e:
                retry_after = float(e.headers.get("Retry-After", "60"))
                if e.code == 429:
                    self._limiter.pause(retry_after)
                return e.code, dict(e.headers), e.read()

        return await loop.run_in_executor(None, _sync)


# ── M365 backup agent ─────────────────────────────────────────────────────────

class M365BackupAgent:
    """
    Backs up Microsoft 365 mailboxes and OneDrive using Graph API delta queries.

    Delta tokens allow incremental backup — only items changed since the last
    run are fetched.  Full backup is triggered on first run or when the delta
    token expires (Graph returns 410 Gone).
    """

    GRAPH_BASE = "https://graph.microsoft.com/v1.0"
    TOKEN_URL  = "https://login.microsoftonline.com/{tenant}/oauth2/v2.0/token"
    SCOPES     = "https://graph.microsoft.com/.default"

    def __init__(self, cred_store: CredentialStore,
                 delta_store: DeltaStateStore,
                 rate_registry: RateLimiterRegistry,
                 backup_dir: Path) -> None:
        self._creds = cred_store
        self._delta = delta_store
        self._rates = rate_registry
        self._backup_dir = backup_dir

    async def refresh_token(self, source_id: str) -> str | None:
        """Refresh OAuth token using client credentials flow. Returns access token."""
        rec = self._creds.get(source_id)
        if not rec:
            return None
        limiter = self._rates.get(Provider.M365, rec.tenant_id, "read")
        http = ThrottledHTTP(limiter)
        url = self.TOKEN_URL.format(tenant=rec.tenant_id)
        body = (
            f"grant_type=client_credentials"
            f"&client_id={rec.client_id}"
            f"&client_secret={rec.client_secret}"
            f"&scope={self.SCOPES}"
        ).encode()
        status, _, resp_body = await http._do_request(
            "POST", url,
            {"Content-Type": "application/x-www-form-urlencoded"},
            body, 30.0,
        )
        if status != 200:
            log.error("M365 token refresh failed (source=%s status=%d)", source_id, status)
            return None
        data = json.loads(resp_body)
        token = data["access_token"]
        expires_at = time.time() + int(data.get("expires_in", 3600))
        self._creds.update_token(source_id, token, expires_at)
        log.debug("M365 token refreshed for source=%s expires_in=%ds",
                  source_id, data.get("expires_in", 3600))
        return token

    async def _auth_headers(self, source_id: str) -> dict[str, str] | None:
        rec = self._creds.get(source_id)
        if not rec:
            return None
        if rec.needs_refresh():
            token = await self.refresh_token(source_id)
        else:
            token = rec.access_token
        if not token:
            return None
        return {"Authorization": f"Bearer {token}", "Accept": "application/json"}

    async def list_users(self, source_id: str) -> list[dict[str, Any]]:
        """Return all licensed mailbox users in the tenant."""
        rec = self._creds.get(source_id)
        if not rec:
            return []
        headers = await self._auth_headers(source_id)
        if not headers:
            return []
        limiter = self._rates.get(Provider.M365, rec.tenant_id, "read")
        http = ThrottledHTTP(limiter)
        users: list[dict] = []
        url: str | None = (
            f"{self.GRAPH_BASE}/users"
            "?$select=id,userPrincipalName,displayName,assignedLicenses"
            "&$filter=assignedLicenses/$count ne 0&$count=true"
        )
        headers_with_count = {**headers, "ConsistencyLevel": "eventual"}
        while url:
            status, _, body = await http.get(url, headers_with_count)
            if status == 429:
                # Rate limiter already paused; caller will re-queue this job
                raise RateLimitError("429 listing M365 users")
            if status != 200:
                log.error("Failed to list M365 users: status=%d", status)
                break
            data = json.loads(body)
            users.extend(data.get("value", []))
            url = data.get("@odata.nextLink")
        return users

    async def backup_mailbox(self, source_id: str, user_id: str,
                              user_upn: str) -> tuple[int, int]:
        """
        Back up one user's mailbox using delta query.
        Returns (items_backed_up, bytes_backed_up).
        """
        rec = self._creds.get(source_id)
        if not rec:
            return 0, 0
        headers = await self._auth_headers(source_id)
        if not headers:
            return 0, 0

        checkpoint = self._delta.get(source_id, JobType.M365_MAILBOX, user_id)
        delta_link = checkpoint.get("delta_link", "")

        limiter = self._rates.get(Provider.M365, rec.tenant_id, "read")
        http = ThrottledHTTP(limiter)

        if delta_link:
            url: str | None = delta_link
        else:
            # First run — full sync
            url = (
                f"{self.GRAPH_BASE}/users/{user_id}/messages/delta"
                "?$select=id,subject,receivedDateTime,from,toRecipients,hasAttachments"
                "&$top=50"
            )

        out_dir = self._backup_dir / source_id / "mailboxes" / user_upn
        out_dir.mkdir(parents=True, exist_ok=True)

        items = bytes_out = 0
        new_delta_link = ""

        while url:
            status, resp_headers, body = await http.get(url, headers)

            if status == 410:
                # Delta token expired — fall back to full sync
                log.info("M365 delta token expired for %s — starting full sync", user_upn)
                self._delta.set(source_id, JobType.M365_MAILBOX, user_id, {})
                url = (
                    f"{self.GRAPH_BASE}/users/{user_id}/messages/delta"
                    "?$select=id,subject,receivedDateTime,from,toRecipients"
                    "&$top=50"
                )
                continue

            if status == 429:
                raise RateLimitError(f"429 backing up mailbox {user_upn}")

            if status != 200:
                log.error("Graph mailbox error user=%s status=%d", user_upn, status)
                break

            data = json.loads(body)
            for msg in data.get("value", []):
                msg_id = msg["id"]
                # Fetch full RFC2822 message for archival
                mime_status, _, mime_body = await http.get(
                    f"{self.GRAPH_BASE}/users/{user_id}/messages/{msg_id}/$value",
                    {**headers, "Accept": "text/plain"},
                )
                if mime_status == 200:
                    msg_file = out_dir / f"{msg_id}.eml"
                    msg_file.write_bytes(mime_body)
                    meta_file = out_dir / f"{msg_id}.json"
                    meta_file.write_text(json.dumps(msg))
                    items += 1
                    bytes_out += len(mime_body)

            url = data.get("@odata.nextLink")
            if not url:
                new_delta_link = data.get("@odata.deltaLink", "")

        if new_delta_link:
            self._delta.set(source_id, JobType.M365_MAILBOX, user_id,
                            {"delta_link": new_delta_link})
            log.info("Mailbox %s: %d items, %d bytes", user_upn, items, bytes_out)

        return items, bytes_out

    async def backup_onedrive(self, source_id: str, user_id: str,
                               user_upn: str) -> tuple[int, int]:
        """Back up one user's OneDrive using drive delta."""
        rec = self._creds.get(source_id)
        if not rec:
            return 0, 0
        headers = await self._auth_headers(source_id)
        if not headers:
            return 0, 0

        checkpoint = self._delta.get(source_id, JobType.M365_ONEDRIVE, user_id)
        delta_link = checkpoint.get("delta_link", "")

        limiter = self._rates.get(Provider.M365, rec.tenant_id, "read")
        http = ThrottledHTTP(limiter)
        url: str | None = delta_link or (
            f"{self.GRAPH_BASE}/users/{user_id}/drive/root/delta"
            "?$select=id,name,size,file,folder,deleted,parentReference,lastModifiedDateTime"
            "&$top=100"
        )

        out_dir = self._backup_dir / source_id / "onedrive" / user_upn
        out_dir.mkdir(parents=True, exist_ok=True)

        items = bytes_out = 0
        new_delta_link = ""

        while url:
            status, _, body = await http.get(url, headers)

            if status == 410:
                log.info("OneDrive delta expired for %s — full sync", user_upn)
                self._delta.set(source_id, JobType.M365_ONEDRIVE, user_id, {})
                url = (
                    f"{self.GRAPH_BASE}/users/{user_id}/drive/root/delta"
                    "?$select=id,name,size,file,folder,deleted,parentReference"
                    "&$top=100"
                )
                continue

            if status == 429:
                raise RateLimitError(f"429 backing up OneDrive {user_upn}")
            if status != 200:
                log.error("Graph OneDrive error user=%s status=%d", user_upn, status)
                break

            data = json.loads(body)
            for item in data.get("value", []):
                if item.get("deleted"):
                    # Record deletion in manifest but don't download
                    manifest = out_dir / f"{item['id']}.deleted.json"
                    manifest.write_text(json.dumps(item))
                    continue
                if not item.get("file"):
                    continue  # folder — no content to download

                item_id = item["id"]
                dl_status, _, dl_body = await http.get(
                    f"{self.GRAPH_BASE}/users/{user_id}/drive/items/{item_id}/content",
                    headers,
                )
                if dl_status in (200, 302):
                    file_path = out_dir / f"{item_id}_{item.get('name', item_id)}"
                    file_path.write_bytes(dl_body)
                    meta_path = out_dir / f"{item_id}.json"
                    meta_path.write_text(json.dumps(item))
                    items += 1
                    bytes_out += len(dl_body)

            url = data.get("@odata.nextLink")
            if not url:
                new_delta_link = data.get("@odata.deltaLink", "")

        if new_delta_link:
            self._delta.set(source_id, JobType.M365_ONEDRIVE, user_id,
                            {"delta_link": new_delta_link})
            log.info("OneDrive %s: %d items, %d bytes", user_upn, items, bytes_out)

        return items, bytes_out


# ── Google Workspace backup agent ─────────────────────────────────────────────

class GoogleWorkspaceBackupAgent:
    """
    Backs up Gmail and Drive using Admin SDK + Google Drive API delta (historyId / changes).

    Uses a service account with domain-wide delegation.  The service account
    key JSON is encrypted in the credential store.
    """

    GMAIL_BASE  = "https://gmail.googleapis.com/gmail/v1/users"
    DRIVE_BASE  = "https://www.googleapis.com/drive/v3"
    TOKEN_URL   = "https://oauth2.googleapis.com/token"

    def __init__(self, cred_store: CredentialStore,
                 delta_store: DeltaStateStore,
                 rate_registry: RateLimiterRegistry,
                 backup_dir: Path) -> None:
        self._creds = cred_store
        self._delta = delta_store
        self._rates = rate_registry
        self._backup_dir = backup_dir

    async def _get_token_for_user(self, source_id: str, user_email: str) -> str | None:
        """
        Obtain an access token impersonating user_email via service account
        domain-wide delegation.  Issues a signed JWT assertion.
        """
        rec = self._creds.get(source_id)
        if not rec or not rec.service_account_json:
            return None

        try:
            sa = json.loads(rec.service_account_json)
        except Exception:
            log.error("Invalid service account JSON for source=%s", source_id)
            return None

        # Build JWT assertion for DWD
        try:
            import time as _time
            now = int(_time.time())
            header = base64.urlsafe_b64encode(
                json.dumps({"alg": "RS256", "typ": "JWT"}).encode()
            ).rstrip(b"=").decode()

            claims = {
                "iss": sa["client_email"],
                "scope": (
                    "https://www.googleapis.com/auth/gmail.readonly "
                    "https://www.googleapis.com/auth/drive.readonly "
                    "https://www.googleapis.com/auth/admin.directory.user.readonly"
                ),
                "aud": self.TOKEN_URL,
                "exp": now + 3600,
                "iat": now,
                "sub": user_email,
            }
            payload = base64.urlsafe_b64encode(
                json.dumps(claims).encode()
            ).rstrip(b"=").decode()

            from cryptography.hazmat.primitives.serialization import load_pem_private_key
            from cryptography.hazmat.primitives.asymmetric.padding import PKCS1v15
            from cryptography.hazmat.primitives.hashes import SHA256
            key = load_pem_private_key(sa["private_key"].encode(), password=None)
            sig_input = f"{header}.{payload}".encode()
            signature = base64.urlsafe_b64encode(
                key.sign(sig_input, PKCS1v15(), SHA256())
            ).rstrip(b"=").decode()

            jwt_assertion = f"{header}.{payload}.{signature}"

            limiter = self._rates.get(Provider.GOOGLE, rec.tenant_id, "read")
            http = ThrottledHTTP(limiter)
            body = (
                f"grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer"
                f"&assertion={jwt_assertion}"
            ).encode()
            status, _, resp = await http._do_request(
                "POST", self.TOKEN_URL,
                {"Content-Type": "application/x-www-form-urlencoded"},
                body, 30.0,
            )
            if status != 200:
                log.error("Google token fetch failed status=%d user=%s", status, user_email)
                return None
            return json.loads(resp).get("access_token")

        except ImportError:
            log.warning("cryptography package required for Google Workspace backup JWT signing")
            return None
        except Exception as e:
            log.error("Google JWT error: %s", e)
            return None

    async def list_users(self, source_id: str) -> list[str]:
        """Return all user email addresses in the Google Workspace domain."""
        rec = self._creds.get(source_id)
        if not rec:
            return []
        token = await self._get_token_for_user(source_id, rec.admin_email)
        if not token:
            return []
        limiter = self._rates.get(Provider.GOOGLE, rec.tenant_id, "read")
        http = ThrottledHTTP(limiter)
        headers = {"Authorization": f"Bearer {token}"}
        users: list[str] = []
        url: str | None = (
            "https://admin.googleapis.com/admin/directory/v1/users"
            f"?customer=my_customer&maxResults=500"
        )
        while url:
            status, _, body = await http.get(url, headers)
            if status == 429:
                raise RateLimitError("429 listing Google users")
            if status != 200:
                log.error("Failed to list Google users: status=%d", status)
                break
            data = json.loads(body)
            users.extend(u["primaryEmail"] for u in data.get("users", []))
            url = data.get("nextPageToken") and (
                f"https://admin.googleapis.com/admin/directory/v1/users"
                f"?customer=my_customer&maxResults=500"
                f"&pageToken={data['nextPageToken']}"
            )
        return users

    async def backup_gmail(self, source_id: str,
                            user_email: str) -> tuple[int, int]:
        """Back up Gmail using historyId delta (only new messages since last run)."""
        rec = self._creds.get(source_id)
        if not rec:
            return 0, 0
        token = await self._get_token_for_user(source_id, user_email)
        if not token:
            return 0, 0

        checkpoint = self._delta.get(source_id, JobType.GOOGLE_GMAIL, user_email)
        history_id = checkpoint.get("history_id", "")

        limiter = self._rates.get(Provider.GOOGLE, rec.tenant_id, "read")
        http = ThrottledHTTP(limiter)
        headers = {"Authorization": f"Bearer {token}"}

        out_dir = self._backup_dir / source_id / "gmail" / user_email
        out_dir.mkdir(parents=True, exist_ok=True)

        items = bytes_out = 0
        new_history_id = history_id

        if history_id:
            # Incremental: fetch changes since last historyId
            url: str | None = (
                f"{self.GMAIL_BASE}/{user_email}/history"
                f"?startHistoryId={history_id}&historyTypes=messageAdded&maxResults=500"
            )
            msg_ids: list[str] = []
            while url:
                status, _, body = await http.get(url, headers)
                if status == 404:
                    # historyId too old — full sync needed
                    history_id = ""
                    msg_ids = []
                    break
                if status == 429:
                    raise RateLimitError(f"429 Gmail history {user_email}")
                if status != 200:
                    log.error("Gmail history error user=%s status=%d", user_email, status)
                    break
                data = json.loads(body)
                new_history_id = data.get("historyId", new_history_id)
                for record in data.get("history", []):
                    msg_ids.extend(
                        m["message"]["id"]
                        for m in record.get("messagesAdded", [])
                    )
                page_token = data.get("nextPageToken")
                url = (
                    f"{self.GMAIL_BASE}/{user_email}/history"
                    f"?startHistoryId={history_id}&historyTypes=messageAdded"
                    f"&maxResults=500&pageToken={page_token}"
                    if page_token else None
                )
        else:
            msg_ids = []

        if not history_id:
            # Full sync: list all message IDs
            list_url: str | None = (
                f"{self.GMAIL_BASE}/{user_email}/messages?maxResults=500"
            )
            while list_url:
                status, _, body = await http.get(list_url, headers)
                if status == 429:
                    raise RateLimitError(f"429 Gmail list {user_email}")
                if status != 200:
                    break
                data = json.loads(body)
                msg_ids.extend(m["id"] for m in data.get("messages", []))
                new_history_id = data.get("historyId", new_history_id)
                page_token = data.get("nextPageToken")
                list_url = (
                    f"{self.GMAIL_BASE}/{user_email}/messages"
                    f"?maxResults=500&pageToken={page_token}"
                    if page_token else None
                )

        # Download each message as RFC2822
        for msg_id in msg_ids:
            status, _, body = await http.get(
                f"{self.GMAIL_BASE}/{user_email}/messages/{msg_id}"
                "?format=raw",
                headers,
            )
            if status == 200:
                data = json.loads(body)
                raw = base64.urlsafe_b64decode(data.get("raw", "") + "==")
                if raw:
                    (out_dir / f"{msg_id}.eml").write_bytes(raw)
                    items += 1
                    bytes_out += len(raw)

        if new_history_id:
            self._delta.set(source_id, JobType.GOOGLE_GMAIL, user_email,
                            {"history_id": new_history_id})
            log.info("Gmail %s: %d items, %d bytes", user_email, items, bytes_out)

        return items, bytes_out

    async def backup_drive(self, source_id: str,
                            user_email: str) -> tuple[int, int]:
        """Back up Google Drive using the changes API (page token = delta)."""
        rec = self._creds.get(source_id)
        if not rec:
            return 0, 0
        token = await self._get_token_for_user(source_id, user_email)
        if not token:
            return 0, 0

        checkpoint = self._delta.get(source_id, JobType.GOOGLE_DRIVE, user_email)
        page_token = checkpoint.get("page_token", "")

        limiter = self._rates.get(Provider.GOOGLE, rec.tenant_id, "read")
        http = ThrottledHTTP(limiter)
        headers = {"Authorization": f"Bearer {token}"}

        out_dir = self._backup_dir / source_id / "drive" / user_email
        out_dir.mkdir(parents=True, exist_ok=True)

        items = bytes_out = 0
        new_page_token = page_token

        if not page_token:
            # Get the start page token for incremental tracking
            status, _, body = await http.get(
                f"{self.DRIVE_BASE}/changes/startPageToken", headers
            )
            if status == 200:
                page_token = json.loads(body).get("startPageToken", "")

        url: str | None = (
            f"{self.DRIVE_BASE}/changes"
            f"?pageToken={page_token}&spaces=drive&includeRemoved=true"
            f"&fields=nextPageToken,newStartPageToken,changes(fileId,removed,file(id,name,size,mimeType,parents,modifiedTime))"
            if page_token else None
        )

        while url:
            status, _, body = await http.get(url, headers)
            if status == 429:
                raise RateLimitError(f"429 Drive changes {user_email}")
            if status != 200:
                log.error("Drive changes error user=%s status=%d", user_email, status)
                break

            data = json.loads(body)
            new_page_token = data.get("newStartPageToken", new_page_token)

            for change in data.get("changes", []):
                if change.get("removed"):
                    f_info = {"fileId": change["fileId"], "removed": True}
                    (out_dir / f"{change['fileId']}.removed.json").write_text(
                        json.dumps(f_info)
                    )
                    continue

                f = change.get("file", {})
                if not f:
                    continue
                file_id = f["id"]
                mime = f.get("mimeType", "")

                # Export Google Workspace formats
                export_mime = {
                    "application/vnd.google-apps.document":
                        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
                    "application/vnd.google-apps.spreadsheet":
                        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                    "application/vnd.google-apps.presentation":
                        "application/vnd.openxmlformats-officedocument.presentationml.presentation",
                }.get(mime)

                if export_mime:
                    dl_url = (
                        f"{self.DRIVE_BASE}/files/{file_id}/export"
                        f"?mimeType={export_mime}"
                    )
                elif "vnd.google-apps" not in mime:
                    dl_url = f"{self.DRIVE_BASE}/files/{file_id}?alt=media"
                else:
                    # Other Google-only format — skip binary download, store metadata
                    (out_dir / f"{file_id}.json").write_text(json.dumps(f))
                    continue

                dl_status, _, dl_body = await http.get(dl_url, headers)
                if dl_status == 200:
                    fname = f"{file_id}_{f.get('name', file_id)}"
                    (out_dir / fname).write_bytes(dl_body)
                    (out_dir / f"{file_id}.json").write_text(json.dumps(f))
                    items += 1
                    bytes_out += len(dl_body)

            next_pt = data.get("nextPageToken")
            url = (
                f"{self.DRIVE_BASE}/changes"
                f"?pageToken={next_pt}&spaces=drive&includeRemoved=true"
                f"&fields=nextPageToken,newStartPageToken,changes(fileId,removed,file(id,name,size,mimeType,parents,modifiedTime))"
                if next_pt else None
            )

        if new_page_token:
            self._delta.set(source_id, JobType.GOOGLE_DRIVE, user_email,
                            {"page_token": new_page_token})
            log.info("Drive %s: %d items, %d bytes", user_email, items, bytes_out)

        return items, bytes_out


# ── Backup queue abstraction ──────────────────────────────────────────────────
#
# OSS mode: LocalBackupQueue — wraps asyncio.PriorityQueue, workers run in-process.
#
# Connect mode: ConnectBackupQueue (Connect-side, not in this repo) — pushes jobs to
#   an external Redis/SQS queue.  Spot-instance worker containers dequeue jobs,
#   fetch credentials + delta state from Connect, write backup data to object storage,
#   then commit the new delta checkpoint.  The controller is scheduler-only; it never
#   touches the actual backup data.  Stateless workers can be killed mid-job safely
#   because delta tokens are not advanced until after the successful write.
#
# This interface is the seam.  CloudBackupManager uses LocalBackupQueue in OSS mode.
# When Connect is available, the manager can swap in ConnectBackupQueue at startup
# (injected via constructor) without changing any scheduling or retry logic.

class BackupQueue:
    """
    Abstract interface for a backup job queue.

    LocalBackupQueue (this module) — in-process asyncio queue.
    ConnectBackupQueue (connect.py, future) — external managed queue.
    """

    async def put(self, job: "BackupJob", priority: int = 1) -> None:
        raise NotImplementedError

    async def get(self) -> "BackupJob":
        raise NotImplementedError

    def task_done(self) -> None:
        pass

    def qsize(self) -> int:
        return 0

    def is_external(self) -> bool:
        """True if job execution happens outside this process (spot containers)."""
        return False


class LocalBackupQueue(BackupQueue):
    """
    In-process priority queue.  Workers run as asyncio tasks in the controller.
    Used in OSS mode and during development.
    """

    def __init__(self) -> None:
        self._q: asyncio.PriorityQueue = asyncio.PriorityQueue()
        self._seq = itertools.count()

    async def put(self, job: "BackupJob", priority: int = 1) -> None:
        await self._q.put((priority, next(self._seq), job))

    async def get(self) -> "BackupJob":
        _, _, job = await self._q.get()
        return job

    def task_done(self) -> None:
        self._q.task_done()

    def qsize(self) -> int:
        return self._q.qsize()


# ── Backup result channel ──────────────────────────────────────────────────────
#
# The symmetric return path for job results.  Local workers fire _on_job_done
# directly (no channel needed).  Spot containers push results to Connect, and
# the controller polls to drain them.
#
# Connect API contract (server-side, to be implemented in ozmalabs/connect):
#
#   POST /api/v1/backup/results
#       Body: { controller_id, job: <BackupJob.to_dict()> }
#       Auth: spot-worker short-lived token (injected at container start)
#       → 200 { ok: true }
#
#   GET  /api/v1/backup/results?controller_id={id}&since={ts}
#       Auth: controller token
#       → 200 { results: [<BackupJob.to_dict()>, ...] }
#       Results are acked + removed from the server-side buffer after retrieval.

class BackupResultChannel:
    """
    Abstract return path for completed job results.

    LocalResultChannel  — no-op (local workers fire _on_job_done directly).
    ConnectResultChannel — HTTP push/poll via Connect API.
    """

    async def push_result(self, job: "BackupJob") -> None:
        """Called by the worker after a job completes (success or failure)."""
        raise NotImplementedError

    async def drain(self) -> "list[BackupJob]":
        """
        Called by the controller to retrieve completed jobs since last drain.
        Returns an empty list if no results are pending.
        """
        raise NotImplementedError


class LocalResultChannel(BackupResultChannel):
    """
    No-op result channel for local (in-process) workers.

    BackupWorkerPool calls _on_job_done directly after each job — there is no
    asynchronous return path to poll.  This stub satisfies the interface so
    CloudBackupManager does not need to special-case local mode.
    """

    async def push_result(self, job: "BackupJob") -> None:
        pass  # Never called in local mode; _on_job_done fires synchronously.

    async def drain(self) -> "list[BackupJob]":
        return []  # Never called in local mode; polling loop is not started.


class ConnectResultChannel(BackupResultChannel):
    """
    HTTP-backed result channel via Ozma Connect.

    push_result() is called by spot-instance worker containers after each job.
    drain()       is called by the controller's _result_poll_loop every N seconds.

    The Connect server buffers results per controller_id and clears them after
    retrieval, so repeated drain() calls are idempotent until new results arrive.
    """

    _PUSH_PATH  = "/api/v1/backup/results"
    _DRAIN_PATH = "/api/v1/backup/results"

    def __init__(self, connect_url: str, token: str, controller_id: str) -> None:
        self._base     = connect_url.rstrip("/")
        self._token    = token
        self._ctrl_id  = controller_id
        self._last_drain_at: float = 0.0

    async def push_result(self, job: "BackupJob") -> None:
        """POST the completed job dict to Connect.  Best-effort — log on failure."""
        import urllib.request, urllib.error
        body = json.dumps({
            "controller_id": self._ctrl_id,
            "job": job.to_dict(),
        }).encode()
        loop = asyncio.get_running_loop()
        def _post():
            req = urllib.request.Request(
                f"{self._base}{self._PUSH_PATH}",
                data=body,
                headers={
                    "Content-Type": "application/json",
                    "Authorization": f"Bearer {self._token}",
                },
                method="POST",
            )
            with urllib.request.urlopen(req, timeout=10) as r:
                return r.status
        try:
            await loop.run_in_executor(None, _post)
        except Exception as e:
            log.warning("ConnectResultChannel.push_result failed: %s", e)

    async def drain(self) -> "list[BackupJob]":
        """
        GET completed jobs from Connect since the last drain.
        Returns deserialized BackupJob objects; Connect clears them after response.
        """
        import urllib.request, urllib.error
        since = self._last_drain_at
        url = (
            f"{self._base}{self._DRAIN_PATH}"
            f"?controller_id={self._ctrl_id}&since={since:.3f}"
        )
        loop = asyncio.get_running_loop()
        def _get():
            req = urllib.request.Request(
                url,
                headers={"Authorization": f"Bearer {self._token}"},
            )
            with urllib.request.urlopen(req, timeout=10) as r:
                return json.loads(r.read())
        try:
            data = await loop.run_in_executor(None, _get)
            self._last_drain_at = time.time()
            return [BackupJob.from_dict(d) for d in data.get("results", [])]
        except Exception as e:
            log.warning("ConnectResultChannel.drain failed: %s", e)
            return []


# ── Connect delta state store ──────────────────────────────────────────────────
#
# When workers run in spot containers they cannot access the controller's local
# cloud_backup_state.json.  ConnectDeltaStateStore replaces the local JSON file
# with Connect-API calls so any container can pick up where another left off.
#
# Connect API contract (server-side, to be implemented in ozmalabs/connect):
#
#   GET    /api/v1/backup/delta/{controller_id}/{key}
#       → 200 { checkpoint: {...} }  or  404
#
#   PUT    /api/v1/backup/delta/{controller_id}/{key}
#       Body: { checkpoint: {...} }
#       → 200 { ok: true }
#
#   DELETE /api/v1/backup/delta/{controller_id}?source_id={id}
#       → 200 { ok: true, deleted: N }
#
# The key format is identical to DeltaStateStore: "{source_id}:{job_type}:{user_id}".
# Spot workers use this class directly; the controller continues to use the local
# DeltaStateStore for its own status display (last_synced_at is in the result
# pushed through ConnectResultChannel, not delta state).

class ConnectDeltaStateStore:
    """
    Connect-backed delta checkpoint store for spot-instance workers.

    Implements the same get/set/clear interface as DeltaStateStore so the
    backup agents (M365BackupAgent, GoogleWorkspaceBackupAgent, RcloneBackupAgent)
    can be used unchanged inside spot containers — just swap the delta store.
    """

    _BASE_PATH = "/api/v1/backup/delta"

    def __init__(self, connect_url: str, token: str, controller_id: str) -> None:
        self._base    = connect_url.rstrip("/")
        self._token   = token
        self._ctrl_id = controller_id
        # In-memory write-through cache so within-job reads don't hit the network.
        self._cache: dict[str, Any] = {}

    def _key(self, source_id: str, job_type: str, user_id: str) -> str:
        return f"{source_id}:{job_type}:{user_id}"

    def get(self, source_id: str, job_type: str, user_id: str) -> dict[str, Any]:
        """
        Synchronous read — uses run_in_executor internally.
        Callers in async context should use await get_async() instead.
        Falls back to cache on network error.
        """
        import urllib.request
        key = self._key(source_id, job_type, user_id)
        if key in self._cache:
            return self._cache[key]
        url = f"{self._base}{self._BASE_PATH}/{self._ctrl_id}/{key}"
        try:
            req = urllib.request.Request(
                url,
                headers={"Authorization": f"Bearer {self._token}"},
            )
            with urllib.request.urlopen(req, timeout=5) as r:
                data = json.loads(r.read())
                result = data.get("checkpoint", {})
                self._cache[key] = result
                return result
        except Exception:
            return {}

    async def get_async(self, source_id: str, job_type: str,
                        user_id: str) -> dict[str, Any]:
        """Async variant — preferred inside async backup agents."""
        import urllib.request
        key = self._key(source_id, job_type, user_id)
        if key in self._cache:
            return self._cache[key]
        url = f"{self._base}{self._BASE_PATH}/{self._ctrl_id}/{key}"
        loop = asyncio.get_running_loop()
        def _get():
            req = urllib.request.Request(
                url, headers={"Authorization": f"Bearer {self._token}"}
            )
            try:
                with urllib.request.urlopen(req, timeout=5) as r:
                    return json.loads(r.read()).get("checkpoint", {})
            except Exception:
                return {}
        result = await loop.run_in_executor(None, _get)
        self._cache[key] = result
        return result

    def set(self, source_id: str, job_type: str, user_id: str,
            checkpoint: dict[str, Any]) -> None:
        """
        Write-through: update cache immediately, then persist to Connect.
        Called after a successful backup page — must be best-effort so a
        network hiccup does not abort the job.
        """
        import urllib.request
        key = self._key(source_id, job_type, user_id)
        full = {**checkpoint, "last_synced_at": time.time()}
        self._cache[key] = full
        body = json.dumps({"checkpoint": full}).encode()
        url = f"{self._base}{self._BASE_PATH}/{self._ctrl_id}/{key}"
        try:
            req = urllib.request.Request(
                url,
                data=body,
                headers={
                    "Content-Type": "application/json",
                    "Authorization": f"Bearer {self._token}",
                },
                method="PUT",
            )
            urllib.request.urlopen(req, timeout=5).close()
        except Exception as e:
            log.warning("ConnectDeltaStateStore.set failed (checkpoint may be lost): %s", e)

    def clear(self, source_id: str) -> None:
        """Remove all delta state for a source (triggers full re-sync on next run)."""
        import urllib.request
        keys = [k for k in self._cache if k.startswith(f"{source_id}:")]
        for k in keys:
            del self._cache[k]
        url = (
            f"{self._base}{self._BASE_PATH}/{self._ctrl_id}"
            f"?source_id={source_id}"
        )
        try:
            req = urllib.request.Request(
                url,
                headers={"Authorization": f"Bearer {self._token}"},
                method="DELETE",
            )
            urllib.request.urlopen(req, timeout=5).close()
        except Exception as e:
            log.warning("ConnectDeltaStateStore.clear failed: %s", e)


# ── rclone backup agent ───────────────────────────────────────────────────────

class RcloneBackupAgent:
    """
    Wraps the rclone binary to back up any of its 70+ supported providers.

    rclone handles OAuth token refresh, checksums, rate limiting, and retries
    internally.  We add:
      - subprocess management via asyncio.create_subprocess_exec
      - --config pointing to our managed rclone.conf (mode 0o600)
      - --tpslimit / --bwlimit passed from BackupSource config
      - --backup-dir for versioned backups (changed/deleted files preserved)
      - --log-format json for structured log parsing
      - Optional config encryption: RCLONE_CONFIG_PASS derived from mesh key

    rclone.conf lives at data_dir/rclone.conf and is never logged.
    """

    _BINARY = "rclone"

    def __init__(self, data_dir: Path,
                 mesh_key_bytes: bytes | None = None) -> None:
        self._dir = data_dir
        self._conf = data_dir / "rclone.conf"
        self._conf_pass: str | None = None
        if mesh_key_bytes:
            # Derive a stable config password from the mesh key.
            # rclone uses this to encrypt the rclone.conf at rest.
            import hashlib as _hl
            raw = hmac.new(b"ozma-rclone-conf-pass-v1",
                           mesh_key_bytes, _hl.sha256).hexdigest()
            self._conf_pass = raw[:32]

    def _base_cmd(self) -> list[str]:
        """Common flags prepended to every rclone invocation."""
        return [self._BINARY, "--config", str(self._conf)]

    def _base_env(self) -> dict[str, str]:
        env = {**os.environ}
        if self._conf_pass:
            env["RCLONE_CONFIG_PASS"] = self._conf_pass
        return env

    async def check_available(self) -> tuple[bool, str]:
        """Returns (available, version_string)."""
        try:
            proc = await asyncio.create_subprocess_exec(
                self._BINARY, "version", "--check",
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            stdout, _ = await proc.communicate()
            if proc.returncode == 0:
                ver = stdout.decode().splitlines()[0] if stdout else "unknown"
                return True, ver
            return False, ""
        except FileNotFoundError:
            return False, ""

    async def list_remotes(self) -> list[dict[str, str]]:
        """
        Returns list of configured remotes as [{"name": "...", "type": "..."}].
        """
        if not self._conf.exists():
            return []
        proc = await asyncio.create_subprocess_exec(
            *self._base_cmd(), "listremotes", "--long",
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
            env=self._base_env(),
        )
        stdout, _ = await proc.communicate()
        if proc.returncode != 0:
            return []
        remotes = []
        for line in stdout.decode().splitlines():
            line = line.strip()
            if ":" in line:
                parts = line.split(":", 1)
                name = parts[0].strip().rstrip(":")
                remote_type = parts[1].strip() if len(parts) > 1 else ""
                remotes.append({"name": name, "type": remote_type})
        return remotes

    async def configure_remote(self, name: str, remote_type: str,
                                params: dict[str, str]) -> bool:
        """
        Create or update a remote in rclone.conf.

        Equivalent to: rclone config create <name> <type> [key=value ...]
        For OAuth providers this is non-interactive — supply all params directly
        (e.g. client_id, client_secret, token for pre-authorised remotes).
        """
        self._conf.parent.mkdir(parents=True, exist_ok=True)
        if not self._conf.exists():
            self._conf.touch(mode=0o600)
        else:
            self._conf.chmod(0o600)

        cmd = [*self._base_cmd(), "config", "create", name, remote_type]
        for k, v in params.items():
            cmd += [k, v]

        proc = await asyncio.create_subprocess_exec(
            *cmd,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
            env=self._base_env(),
        )
        _, stderr = await proc.communicate()
        if proc.returncode != 0:
            log.error("rclone config create failed: %s", stderr.decode())
            return False
        self._conf.chmod(0o600)
        log.info("rclone remote configured: %s (type=%s)", name, remote_type)
        return True

    async def delete_remote(self, name: str) -> bool:
        """Remove a remote from rclone.conf."""
        proc = await asyncio.create_subprocess_exec(
            *self._base_cmd(), "config", "delete", name,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
            env=self._base_env(),
        )
        _, stderr = await proc.communicate()
        if proc.returncode != 0:
            log.warning("rclone config delete failed: %s", stderr.decode())
            return False
        return True

    async def sync(
        self,
        source_id: str,
        remote_path: str,
        dest_path: Path,
        tpslimit: float = 0.0,
        bwlimit: str = "",
        extra_flags: list[str] | None = None,
    ) -> tuple[int, int]:
        """
        Run rclone sync from remote_path → dest_path.

        Uses:
          --checksum         — verify by hash not just mtime/size
          --backup-dir       — versioned backup of changed/deleted files
          --retries 5        — retry on transient errors
          --retries-sleep 10s
          --log-format json  — structured log output
          --stats-one-line   — one-line progress stats
          --stats 0          — disable periodic stats (we parse final)

        Returns (items_synced, bytes_transferred) from rclone stats.
        """
        dest_path.mkdir(parents=True, exist_ok=True)
        backup_dir = dest_path.parent / f"{dest_path.name}.versions"
        backup_dir.mkdir(parents=True, exist_ok=True)

        cmd = [
            *self._base_cmd(),
            "sync",
            remote_path,
            str(dest_path),
            "--checksum",
            "--backup-dir", str(backup_dir),
            "--retries", "5",
            "--retries-sleep", "10s",
            "--log-format", "json",
            "--log-level", "INFO",
            "--stats", "0",
        ]
        if tpslimit > 0:
            cmd += ["--tpslimit", str(tpslimit)]
        if bwlimit:
            cmd += ["--bwlimit", bwlimit]
        if extra_flags:
            cmd += extra_flags

        log.info("rclone sync: %s → %s", remote_path, dest_path)

        proc = await asyncio.create_subprocess_exec(
            *cmd,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
            env=self._base_env(),
        )
        stdout, stderr = await proc.communicate()

        items = bytes_transferred = 0
        # Parse JSON log lines for stats
        for line in (stdout or b"").splitlines():
            try:
                entry = json.loads(line)
                msg = entry.get("msg", "")
                if "Transferred:" in msg:
                    # Extract bytes from log message if present
                    pass
                # rclone emits final stats as a structured entry
                stats = entry.get("stats", {})
                if stats:
                    items = int(stats.get("transfers", 0))
                    bytes_transferred = int(stats.get("bytes", 0))
            except (json.JSONDecodeError, ValueError):
                continue

        if proc.returncode not in (0, 9):  # 9 = transfer errors (partial success)
            err = stderr.decode()
            log.error("rclone sync failed (rc=%d): %s", proc.returncode, err[:500])
            raise RuntimeError(
                f"rclone exited {proc.returncode}: {err[:200]}"
            )

        log.info("rclone sync done: %s items=%d bytes=%d",
                 remote_path, items, bytes_transferred)
        return items, bytes_transferred


# ── Exceptions ────────────────────────────────────────────────────────────────

class RateLimitError(Exception):
    """Raised by agents when a 429 is encountered — triggers re-queue."""


# ── Worker pool ───────────────────────────────────────────────────────────────

class BackupWorkerPool:
    """
    Fixed-size pool of async backup workers consuming from a shared priority queue.

    Global semaphore: limits total concurrent workers.
    Per-provider semaphore: limits concurrent workers for each provider so one
    provider can't monopolise the pool.
    """

    def __init__(self,
                 queue: "BackupQueue | asyncio.PriorityQueue",
                 m365_agent: M365BackupAgent,
                 google_agent: GoogleWorkspaceBackupAgent,
                 rclone_agent: "RcloneBackupAgent | None" = None,
                 max_workers: int = _DEFAULT_WORKERS,
                 max_per_provider: int = _MAX_WORKERS_PER_PROV) -> None:
        self._queue = queue
        self._m365 = m365_agent
        self._google = google_agent
        self._rclone = rclone_agent
        self._global_sem = asyncio.Semaphore(max_workers)
        self._prov_sems: dict[Provider, asyncio.Semaphore] = {
            Provider.M365:   asyncio.Semaphore(max_per_provider),
            Provider.GOOGLE: asyncio.Semaphore(max_per_provider),
            Provider.RCLONE: asyncio.Semaphore(max_per_provider),
        }
        self._tasks: list[asyncio.Task] = []
        self._completed: list[BackupJob] = []
        self._on_job_done: Callable[[BackupJob], None] | None = None

    async def start(self, n_workers: int | None = None) -> None:
        count = n_workers or _DEFAULT_WORKERS
        for i in range(count):
            t = asyncio.create_task(self._worker(i), name=f"cloud-backup-worker-{i}")
            self._tasks.append(t)

    async def stop(self) -> None:
        for t in self._tasks:
            t.cancel()
        await asyncio.gather(*self._tasks, return_exceptions=True)
        self._tasks.clear()

    async def _worker(self, worker_id: int) -> None:
        log.debug("Cloud backup worker %d started", worker_id)
        while True:
            try:
                if isinstance(self._queue, BackupQueue):
                    job = await self._queue.get()
                else:
                    # Raw asyncio.PriorityQueue (legacy / tests)
                    _, _, job = await self._queue.get()
                if job is None:
                    break  # poison pill
                await self._run_job(job)
                if isinstance(self._queue, BackupQueue):
                    self._queue.task_done()
                else:
                    self._queue.task_done()
            except asyncio.CancelledError:
                break
            except Exception as e:
                log.error("Worker %d unhandled error: %s", worker_id, e)

    async def _run_job(self, job: BackupJob) -> None:
        async with self._global_sem:
            async with self._prov_sems[job.provider]:
                job.status = JobStatus.RUNNING
                job.started_at = time.time()
                job.attempts += 1
                try:
                    items, nbytes = await self._dispatch(job)
                    job.items_backed_up = items
                    job.bytes_backed_up = nbytes
                    job.status = JobStatus.DONE
                    job.finished_at = time.time()
                    log.info(
                        "Job done: %s %s/%s items=%d bytes=%d",
                        job.job_type, job.tenant_id, job.user_id,
                        items, nbytes,
                    )
                except RateLimitError as e:
                    log.warning("Rate limited on %s — re-queuing: %s", job.user_id, e)
                    job.status = JobStatus.QUEUED
                    job.next_retry_at = time.time() + job.retry_delay()
                    await asyncio.sleep(job.retry_delay())
                    await self._requeue(job)
                except Exception as e:
                    job.last_error = str(e)
                    if job.attempts >= _MAX_JOB_ATTEMPTS:
                        job.status = JobStatus.DEAD_LETTER
                        log.error(
                            "Job dead-lettered after %d attempts: %s %s — %s",
                            job.attempts, job.job_type, job.user_id, e,
                        )
                    else:
                        job.status = JobStatus.QUEUED
                        delay = job.retry_delay()
                        log.warning(
                            "Job failed (attempt %d/%d), retry in %.0fs: %s",
                            job.attempts, _MAX_JOB_ATTEMPTS, delay, e,
                        )
                        await asyncio.sleep(delay)
                        await self._requeue(job)

                if self._on_job_done:
                    try:
                        self._on_job_done(job)
                    except Exception:
                        pass

    async def _dispatch(self, job: BackupJob) -> tuple[int, int]:
        source_id = job.tenant_id
        if job.job_type == JobType.M365_MAILBOX:
            return await self._m365.backup_mailbox(
                source_id, job.user_id, job.user_id
            )
        elif job.job_type == JobType.M365_ONEDRIVE:
            return await self._m365.backup_onedrive(
                source_id, job.user_id, job.user_id
            )
        elif job.job_type == JobType.GOOGLE_GMAIL:
            return await self._google.backup_gmail(source_id, job.user_id)
        elif job.job_type == JobType.GOOGLE_DRIVE:
            return await self._google.backup_drive(source_id, job.user_id)
        elif job.job_type == JobType.RCLONE_SYNC:
            if not self._rclone:
                raise RuntimeError("rclone agent not available")
            # user_id carries the rclone remote path: "<remote>:<path>"
            remote_path = job.user_id
            dest = self._rclone._dir / "data" / source_id
            tpslimit = float(job.meta.get("tpslimit", 0))
            bwlimit = job.meta.get("bwlimit", "")
            extra_flags = job.meta.get("flags", [])
            return await self._rclone.sync(
                source_id, remote_path, dest,
                tpslimit=tpslimit, bwlimit=bwlimit, extra_flags=extra_flags,
            )
        else:
            raise ValueError(f"Unknown job type: {job.job_type}")

    async def _requeue(self, job: BackupJob) -> None:
        seq = next(_seq_counter)
        await self._queue.put((job.priority + job.attempts, seq, job))


_seq_counter = itertools.count()


# ── Scheduler ─────────────────────────────────────────────────────────────────

def _cron_is_due(cron: str, last_run_at: float) -> bool:
    """
    Minimal cron check: only supports 'M H * * *' format.
    Returns True if the schedule has elapsed since last_run_at.
    """
    try:
        parts = cron.strip().split()
        if len(parts) != 5:
            return False
        import datetime
        now = datetime.datetime.now()
        minute = int(parts[0]) if parts[0] != "*" else now.minute
        hour   = int(parts[1]) if parts[1] != "*" else now.hour
        # Check if we're past the scheduled time today and haven't run today
        scheduled_today = now.replace(hour=hour, minute=minute, second=0, microsecond=0)
        last = datetime.datetime.fromtimestamp(last_run_at) if last_run_at else None
        if now >= scheduled_today:
            if last is None or last.date() < now.date():
                return True
        return False
    except Exception:
        return False


# ── Manager ───────────────────────────────────────────────────────────────────

class CloudBackupManager:
    """
    Top-level manager wiring together all cloud backup components.

    Lifecycle:
        mgr = CloudBackupManager(data_dir, mesh_key_bytes)
        await mgr.start()
        # controller runs
        await mgr.stop()
    """

    def __init__(self, data_dir: Path,
                 mesh_key_bytes: bytes | None = None,
                 queue: "BackupQueue | None" = None,
                 result_channel: "BackupResultChannel | None" = None) -> None:
        """
        Parameters
        ----------
        data_dir:
            Directory for credentials, delta state, and local backup data.
        mesh_key_bytes:
            Controller's mesh CA private key bytes — used to derive the
            credential encryption key and rclone config password.
        queue:
            Optional external BackupQueue (Connect-side).  When provided, jobs
            are dispatched to the external queue and execution happens in spot-
            instance worker containers outside this process.  LocalBackupQueue
            (in-process asyncio workers) is used when None (OSS default).
        result_channel:
            Optional BackupResultChannel (Connect-side).  Required when `queue`
            is external — this is how spot containers report completed jobs back
            to the controller.  LocalResultChannel (no-op) is used when None.
        """
        self._dir = data_dir
        self._dir.mkdir(parents=True, exist_ok=True)

        self._cred_store = CredentialStore(
            data_dir / "cloud_backup_credentials.json",
            mesh_key_bytes,
        )
        self._delta_store = DeltaStateStore(data_dir / "cloud_backup_state.json")
        self._rate_registry = RateLimiterRegistry()

        backup_data = data_dir / "data"
        backup_data.mkdir(exist_ok=True)

        self._m365   = M365BackupAgent(self._cred_store, self._delta_store,
                                       self._rate_registry, backup_data)
        self._google = GoogleWorkspaceBackupAgent(self._cred_store, self._delta_store,
                                                  self._rate_registry, backup_data)
        self._rclone = RcloneBackupAgent(data_dir, mesh_key_bytes)

        # Use the injected queue (Connect external) or default to local in-process queue.
        self._queue: BackupQueue = queue if queue is not None else LocalBackupQueue()
        self._external_queue = queue is not None  # skip in-process workers if external
        # Result channel: how spot containers report back.  LocalResultChannel is a
        # no-op; _on_job_done fires directly when local workers finish.
        self._result_channel: BackupResultChannel = (
            result_channel if result_channel is not None else LocalResultChannel()
        )
        self._pool  = BackupWorkerPool(self._queue, self._m365, self._google,
                                       rclone_agent=self._rclone)

        self._sources: dict[str, BackupSource] = {}
        self._config_path = data_dir / "cloud_backup_config.json"
        self._load_config()

        self._tasks: list[asyncio.Task] = []
        self._job_history: list[BackupJob] = []  # recent completed jobs

        self._pool._on_job_done = self._on_job_done

    # ── Lifecycle ─────────────────────────────────────────────────────────────

    async def start(self) -> None:
        if not self._external_queue:
            # OSS mode: spawn in-process workers to consume from the local queue.
            # Connect mode: skip — workers run in external spot-instance containers.
            await self._pool.start()
        else:
            # External mode: poll the result channel so spot containers can report back.
            self._tasks.append(
                asyncio.create_task(
                    self._result_poll_loop(), name="cloud-backup-result-poll"
                )
            )
        self._tasks.append(
            asyncio.create_task(self._scheduler_loop(), name="cloud-backup-scheduler")
        )
        self._tasks.append(
            asyncio.create_task(self._token_refresh_loop(), name="cloud-backup-token-refresh")
        )
        mode = "external queue (Connect)" if self._external_queue else "local workers"
        log.info("Cloud backup manager started (%d sources, %s)", len(self._sources), mode)

    async def stop(self) -> None:
        for t in self._tasks:
            t.cancel()
        await asyncio.gather(*self._tasks, return_exceptions=True)
        if not self._external_queue:
            await self._pool.stop()

    # ── Source management ─────────────────────────────────────────────────────

    def add_source(self, source: BackupSource) -> BackupSource:
        self._sources[source.id] = source
        self._save_config()
        return source

    def remove_source(self, source_id: str) -> bool:
        if source_id not in self._sources:
            return False
        del self._sources[source_id]
        self._cred_store.delete(source_id)
        self._delta_store.clear(source_id)
        self._save_config()
        return True

    def get_source(self, source_id: str) -> BackupSource | None:
        return self._sources.get(source_id)

    def list_sources(self) -> list[BackupSource]:
        return list(self._sources.values())

    def store_credentials(self, rec: CredentialRecord) -> None:
        """Store (or update) credentials for a source.  Never logged."""
        self._cred_store.store(rec)

    # ── Manual trigger ────────────────────────────────────────────────────────

    async def trigger_backup(self, source_id: str, priority: int = 0) -> list[str]:
        """
        Enqueue all backup jobs for a source immediately.
        Returns list of job IDs enqueued.
        """
        source = self._sources.get(source_id)
        if not source or not source.enabled:
            return []

        job_ids: list[str] = []

        if source.provider == Provider.M365:
            users = await self._m365.list_users(source_id)
            for user in users:
                upn = user.get("userPrincipalName", "")
                if not upn:
                    continue
                if source.backup_mail:
                    job = BackupJob(
                        job_type=JobType.M365_MAILBOX,
                        provider=Provider.M365,
                        tenant_id=source_id,
                        user_id=upn,
                        priority=priority,
                    )
                    await self._enqueue(job)
                    job_ids.append(job.id)
                if source.backup_files:
                    job = BackupJob(
                        job_type=JobType.M365_ONEDRIVE,
                        provider=Provider.M365,
                        tenant_id=source_id,
                        user_id=upn,
                        priority=priority,
                    )
                    await self._enqueue(job)
                    job_ids.append(job.id)

        elif source.provider == Provider.GOOGLE:
            users = await self._google.list_users(source_id)
            for email in users:
                if source.backup_mail:
                    job = BackupJob(
                        job_type=JobType.GOOGLE_GMAIL,
                        provider=Provider.GOOGLE,
                        tenant_id=source_id,
                        user_id=email,
                        priority=priority,
                    )
                    await self._enqueue(job)
                    job_ids.append(job.id)
                if source.backup_files:
                    job = BackupJob(
                        job_type=JobType.GOOGLE_DRIVE,
                        provider=Provider.GOOGLE,
                        tenant_id=source_id,
                        user_id=email,
                        priority=priority,
                    )
                    await self._enqueue(job)
                    job_ids.append(job.id)

        elif source.provider == Provider.RCLONE:
            if not source.rclone_remote:
                log.warning("rclone source %s has no remote configured", source_id)
                return []
            remote_path = f"{source.rclone_remote}:{source.rclone_source_path.lstrip('/')}"
            job = BackupJob(
                job_type=JobType.RCLONE_SYNC,
                provider=Provider.RCLONE,
                tenant_id=source_id,
                user_id=remote_path,
                priority=priority,
                meta={
                    "tpslimit": source.tpslimit,
                    "bwlimit": source.bwlimit,
                    "flags": source.rclone_flags,
                },
            )
            await self._enqueue(job)
            job_ids.append(job.id)

        log.info("Triggered backup for source %s: %d jobs enqueued", source_id, len(job_ids))
        source.last_run_at = time.time()
        source.last_run_status = "running"
        self._save_config()
        return job_ids

    def get_status(self) -> dict[str, Any]:
        return {
            "sources": [s.to_dict() for s in self._sources.values()],
            "queue_depth": self._queue.qsize(),
            "queue_mode": "external" if self._external_queue else "local",
            "recent_jobs": [j.to_dict() for j in self._job_history[-50:]],
        }

    # ── rclone remote management ──────────────────────────────────────────────

    async def rclone_available(self) -> tuple[bool, str]:
        """Check whether rclone binary is installed."""
        return await self._rclone.check_available()

    async def rclone_list_remotes(self) -> list[dict[str, str]]:
        """List configured rclone remotes."""
        return await self._rclone.list_remotes()

    async def rclone_configure_remote(self, name: str, remote_type: str,
                                      params: dict[str, str]) -> bool:
        """Create or update a named rclone remote."""
        return await self._rclone.configure_remote(name, remote_type, params)

    async def rclone_delete_remote(self, name: str) -> bool:
        """Remove a named rclone remote."""
        return await self._rclone.delete_remote(name)

    # ── Result channel access (for Connect wiring) ────────────────────────────

    @property
    def result_channel(self) -> BackupResultChannel:
        """Expose the result channel so connect.py can inject ConnectResultChannel."""
        return self._result_channel

    def set_result_channel(self, channel: BackupResultChannel) -> None:
        """
        Replace the result channel after construction.

        Called by connect.py once Connect credentials are available, to swap
        LocalResultChannel for ConnectResultChannel without restarting the manager.
        Safe to call while running — the polling loop checks _result_channel each
        iteration, so the new channel takes effect on the next poll.
        """
        self._result_channel = channel
        if not self._external_queue:
            return
        # Ensure the poll loop is running now that we have a real channel
        poll_running = any(
            t.get_name() == "cloud-backup-result-poll"
            for t in self._tasks
            if not t.done()
        )
        if not poll_running:
            t = asyncio.create_task(
                self._result_poll_loop(), name="cloud-backup-result-poll"
            )
            self._tasks.append(t)
            log.info("Result poll loop started (ConnectResultChannel injected)")

    # ── Internal ──────────────────────────────────────────────────────────────

    async def _enqueue(self, job: BackupJob) -> None:
        await self._queue.put(job, priority=job.priority)

    async def _scheduler_loop(self) -> None:
        while True:
            await asyncio.sleep(_SCHEDULE_INTERVAL)
            for source in list(self._sources.values()):
                if not source.enabled:
                    continue
                if _cron_is_due(source.schedule_cron, source.last_run_at):
                    log.info("Scheduled backup triggered for source: %s", source.name)
                    try:
                        await self.trigger_backup(source.id, priority=1)
                    except Exception as e:
                        log.error("Scheduled backup failed for %s: %s", source.name, e)

    async def _token_refresh_loop(self) -> None:
        """Proactively refresh OAuth tokens before they expire."""
        while True:
            await asyncio.sleep(60)
            for rec in self._cred_store.all_records():
                if rec.provider == Provider.M365 and rec.needs_refresh() and rec.client_secret:
                    try:
                        await self._m365.refresh_token(rec.id)
                    except Exception as e:
                        log.warning("Token refresh failed for %s: %s", rec.id, e)
                # Google tokens are per-user-impersonation, not cached globally —
                # they are fetched fresh per-job via service account JWT.
                # Alert if service account JSON is approaching its own expiry
                # (service account keys have configurable max age in Google Admin).
                if rec.provider == Provider.GOOGLE and rec.expires_at:
                    days_left = (rec.expires_at - time.time()) / 86400
                    if 0 < days_left < _CRED_EXPIRY_WARN_DAYS:
                        log.warning(
                            "Google service account key for source %s expires in %.1f days "
                            "— rotate in Google Admin Console",
                            rec.id, days_left,
                        )

    def _on_job_done(self, job: BackupJob) -> None:
        self._job_history.append(job)
        # Keep history bounded
        if len(self._job_history) > 500:
            self._job_history = self._job_history[-500:]
        # Update source last-run metadata from job result
        src = self._sources.get(job.tenant_id)
        if src and job.status in (JobStatus.DONE, JobStatus.DEAD_LETTER, JobStatus.FAILED):
            src.last_run_status = job.status
            if job.finished_at:
                src.last_run_at = job.finished_at
            self._save_config()

    async def _result_poll_loop(self) -> None:
        """
        Poll ConnectResultChannel for job completions from spot containers.

        Only runs in external-queue mode.  Drains the channel every
        _RESULT_POLL_INTERVAL seconds and calls _on_job_done for each result,
        keeping _job_history and source status in sync.
        """
        log.info("Result poll loop started (external queue mode)")
        while True:
            try:
                await asyncio.sleep(_RESULT_POLL_INTERVAL)
                results = await self._result_channel.drain()
                if results:
                    log.info("Result poll: received %d completed jobs", len(results))
                for job in results:
                    self._on_job_done(job)
            except asyncio.CancelledError:
                break
            except Exception as e:
                log.warning("Result poll loop error: %s", e)

    def _load_config(self) -> None:
        if not self._config_path.exists():
            return
        try:
            data = json.loads(self._config_path.read_text())
            for d in data.get("sources", []):
                src = BackupSource.from_dict(d)
                self._sources[src.id] = src
        except Exception as e:
            log.error("Failed to load cloud backup config: %s", e)

    def _save_config(self) -> None:
        data = {"sources": [s.to_dict() for s in self._sources.values()]}
        tmp = self._config_path.with_suffix(".tmp")
        tmp.write_text(json.dumps(data, indent=2))
        tmp.rename(self._config_path)
