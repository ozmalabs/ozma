# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Unit tests for cloud_backup.py — queue, rate limiter, credentials, delta state, scheduler."""
import asyncio
import json
import sys
import time
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

sys.path.insert(0, str(Path(__file__).parent.parent.parent.parent / "controller"))
pytestmark = pytest.mark.unit


# ── Rate limiter ──────────────────────────────────────────────────────────────

class TestRateLimiter:
    @pytest.mark.asyncio
    async def test_first_acquire_immediate(self):
        from cloud_backup import RateLimiter
        rl = RateLimiter(rate=10.0)
        t0 = time.monotonic()
        await rl.acquire()
        assert time.monotonic() - t0 < 0.1

    @pytest.mark.asyncio
    async def test_pause_blocks(self):
        from cloud_backup import RateLimiter
        rl = RateLimiter(rate=100.0)
        rl.pause(0.15)
        t0 = time.monotonic()
        await rl.acquire()
        elapsed = time.monotonic() - t0
        assert elapsed >= 0.10   # paused for ~150ms

    def test_pause_sets_tokens_zero(self):
        from cloud_backup import RateLimiter
        rl = RateLimiter(rate=10.0)
        rl.pause(5.0)
        assert rl._tokens == 0.0
        assert rl._paused_until > time.monotonic()


class TestRateLimiterRegistry:
    def test_returns_same_instance(self):
        from cloud_backup import RateLimiterRegistry, Provider
        reg = RateLimiterRegistry()
        a = reg.get(Provider.M365, "tenant-1", "read")
        b = reg.get(Provider.M365, "tenant-1", "read")
        assert a is b

    def test_different_tenant_different_instance(self):
        from cloud_backup import RateLimiterRegistry, Provider
        reg = RateLimiterRegistry()
        a = reg.get(Provider.M365, "tenant-1", "read")
        b = reg.get(Provider.M365, "tenant-2", "read")
        assert a is not b

    def test_different_mode_different_instance(self):
        from cloud_backup import RateLimiterRegistry, Provider
        reg = RateLimiterRegistry()
        r = reg.get(Provider.M365, "t", "read")
        w = reg.get(Provider.M365, "t", "write")
        assert r is not w

    def test_pause_propagates(self):
        from cloud_backup import RateLimiterRegistry, Provider
        reg = RateLimiterRegistry()
        reg.get(Provider.M365, "t", "read")  # create
        reg.pause(Provider.M365, "t", "read", 5.0)
        rl = reg.get(Provider.M365, "t", "read")
        assert rl._paused_until > time.monotonic()


# ── Credential store ──────────────────────────────────────────────────────────

class TestCredentialStore:
    def test_store_and_retrieve(self, tmp_path):
        from cloud_backup import CredentialStore, CredentialRecord, Provider
        store = CredentialStore(tmp_path / "creds.bin")
        rec = CredentialRecord(
            id="src-1",
            provider=Provider.M365,
            tenant_id="tenant-abc",
            client_id="client-xyz",
            client_secret="super-secret",
        )
        store.store(rec)
        retrieved = store.get("src-1")
        assert retrieved is not None
        assert retrieved.client_secret == "super-secret"
        assert retrieved.tenant_id == "tenant-abc"

    def test_persist_and_reload(self, tmp_path):
        from cloud_backup import CredentialStore, CredentialRecord, Provider
        path = tmp_path / "creds.bin"
        fixed_key = b"test-mesh-key-32-bytes-xxxxxxxxx"
        store1 = CredentialStore(path, mesh_key_bytes=fixed_key)
        rec = CredentialRecord(id="src-2", provider=Provider.GOOGLE,
                                admin_email="admin@example.com")
        store1.store(rec)

        store2 = CredentialStore(path, mesh_key_bytes=fixed_key)
        reloaded = store2.get("src-2")
        assert reloaded is not None
        assert reloaded.admin_email == "admin@example.com"

    def test_file_permissions(self, tmp_path):
        from cloud_backup import CredentialStore, CredentialRecord, Provider
        path = tmp_path / "creds.bin"
        store = CredentialStore(path)
        rec = CredentialRecord(id="src-3", provider=Provider.M365)
        store.store(rec)
        mode = path.stat().st_mode & 0o777
        assert mode == 0o600

    def test_delete_removes_record(self, tmp_path):
        from cloud_backup import CredentialStore, CredentialRecord, Provider
        store = CredentialStore(tmp_path / "creds.bin")
        rec = CredentialRecord(id="src-4", provider=Provider.M365)
        store.store(rec)
        store.delete("src-4")
        assert store.get("src-4") is None

    def test_get_missing_returns_none(self, tmp_path):
        from cloud_backup import CredentialStore
        store = CredentialStore(tmp_path / "creds.bin")
        assert store.get("nonexistent") is None

    def test_update_token(self, tmp_path):
        from cloud_backup import CredentialStore, CredentialRecord, Provider
        store = CredentialStore(tmp_path / "creds.bin")
        rec = CredentialRecord(id="src-5", provider=Provider.M365)
        store.store(rec)
        expires = time.time() + 3600
        store.update_token("src-5", "new-access-token", expires)
        updated = store.get("src-5")
        assert updated.access_token == "new-access-token"
        assert updated.token_expires_at == expires

    def test_needs_refresh_when_token_missing(self, tmp_path):
        from cloud_backup import CredentialRecord, Provider
        rec = CredentialRecord(id="x", provider=Provider.M365)
        assert rec.needs_refresh()

    def test_needs_refresh_when_near_expiry(self, tmp_path):
        from cloud_backup import CredentialRecord, Provider
        rec = CredentialRecord(id="x", provider=Provider.M365,
                                access_token="tok",
                                token_expires_at=time.time() + 300)  # 5 min — within margin
        assert rec.needs_refresh()

    def test_does_not_need_refresh_when_fresh(self, tmp_path):
        from cloud_backup import CredentialRecord, Provider
        rec = CredentialRecord(id="x", provider=Provider.M365,
                                access_token="tok",
                                token_expires_at=time.time() + 3600)
        assert not rec.needs_refresh()


# ── Delta state store ─────────────────────────────────────────────────────────

class TestDeltaStateStore:
    def test_get_missing_returns_empty(self, tmp_path):
        from cloud_backup import DeltaStateStore
        store = DeltaStateStore(tmp_path / "state.json")
        assert store.get("src", "m365_mailbox", "user@ex.com") == {}

    def test_set_and_get(self, tmp_path):
        from cloud_backup import DeltaStateStore
        store = DeltaStateStore(tmp_path / "state.json")
        store.set("src", "m365_mailbox", "user@ex.com",
                  {"delta_link": "https://graph.microsoft.com/v1.0/..."})
        result = store.get("src", "m365_mailbox", "user@ex.com")
        assert result["delta_link"].startswith("https://")

    def test_persist_and_reload(self, tmp_path):
        from cloud_backup import DeltaStateStore
        path = tmp_path / "state.json"
        store1 = DeltaStateStore(path)
        store1.set("src", "google_gmail", "a@ex.com", {"history_id": "12345"})

        store2 = DeltaStateStore(path)
        result = store2.get("src", "google_gmail", "a@ex.com")
        assert result["history_id"] == "12345"

    def test_set_adds_last_synced_at(self, tmp_path):
        from cloud_backup import DeltaStateStore
        store = DeltaStateStore(tmp_path / "state.json")
        store.set("src", "m365_mailbox", "u", {"delta_link": "x"})
        result = store.get("src", "m365_mailbox", "u")
        assert "last_synced_at" in result

    def test_clear_removes_source_keys(self, tmp_path):
        from cloud_backup import DeltaStateStore
        store = DeltaStateStore(tmp_path / "state.json")
        store.set("src-a", "m365_mailbox", "u1", {"delta_link": "x"})
        store.set("src-a", "m365_onedrive", "u1", {"delta_link": "y"})
        store.set("src-b", "m365_mailbox", "u2", {"delta_link": "z"})
        store.clear("src-a")
        assert store.get("src-a", "m365_mailbox", "u1") == {}
        assert store.get("src-a", "m365_onedrive", "u1") == {}
        # Other source unaffected
        assert store.get("src-b", "m365_mailbox", "u2")["delta_link"] == "z"


# ── BackupJob ─────────────────────────────────────────────────────────────────

class TestBackupJob:
    def test_retry_delay_doubles(self):
        from cloud_backup import BackupJob
        job = BackupJob()
        delays = []
        for i in range(5):
            job.attempts = i
            delays.append(job.retry_delay())
        assert delays[1] == delays[0] * 2
        assert delays[2] == delays[1] * 2

    def test_retry_delay_caps_at_attempt_4(self):
        from cloud_backup import BackupJob
        job = BackupJob()
        job.attempts = 4
        d4 = job.retry_delay()
        job.attempts = 10
        d10 = job.retry_delay()
        assert d4 == d10   # capped

    def test_to_dict_has_required_fields(self):
        from cloud_backup import BackupJob, JobType, Provider
        job = BackupJob(job_type=JobType.M365_MAILBOX, provider=Provider.M365,
                        tenant_id="t", user_id="u@ex.com")
        d = job.to_dict()
        for key in ("id", "job_type", "provider", "tenant_id", "user_id",
                    "status", "attempts", "items_backed_up", "bytes_backed_up"):
            assert key in d


# ── BackupSource ──────────────────────────────────────────────────────────────

class TestBackupSource:
    def test_roundtrip(self):
        from cloud_backup import BackupSource, Provider
        src = BackupSource(
            name="Acme M365",
            provider=Provider.M365,
            tenant_id="tenant-123",
            backup_mail=True,
            backup_files=False,
            retention_days=365,
        )
        d = src.to_dict()
        src2 = BackupSource.from_dict(d)
        assert src2.name == "Acme M365"
        assert src2.retention_days == 365
        assert not src2.backup_files


# ── Manager: source CRUD and persistence ─────────────────────────────────────

class TestCloudBackupManagerSources:
    @pytest.fixture
    def mgr(self, tmp_path):
        from cloud_backup import CloudBackupManager
        return CloudBackupManager(tmp_path)

    def test_add_and_list(self, mgr):
        from cloud_backup import BackupSource, Provider
        src = BackupSource(name="Test", provider=Provider.M365, tenant_id="t")
        mgr.add_source(src)
        assert src.id in {s.id for s in mgr.list_sources()}

    def test_get_source(self, mgr):
        from cloud_backup import BackupSource, Provider
        src = BackupSource(name="X", provider=Provider.M365, tenant_id="t")
        mgr.add_source(src)
        assert mgr.get_source(src.id) is src

    def test_remove_source(self, mgr):
        from cloud_backup import BackupSource, Provider
        src = BackupSource(name="Y", provider=Provider.M365, tenant_id="t")
        mgr.add_source(src)
        assert mgr.remove_source(src.id)
        assert mgr.get_source(src.id) is None

    def test_remove_nonexistent_returns_false(self, mgr):
        assert not mgr.remove_source("no-such-id")

    def test_config_persisted(self, tmp_path):
        from cloud_backup import CloudBackupManager, BackupSource, Provider
        mgr1 = CloudBackupManager(tmp_path)
        src = BackupSource(name="Persist", provider=Provider.GOOGLE, tenant_id="goog")
        mgr1.add_source(src)

        mgr2 = CloudBackupManager(tmp_path)
        reloaded = mgr2.get_source(src.id)
        assert reloaded is not None
        assert reloaded.name == "Persist"

    def test_status_has_required_keys(self, mgr):
        status = mgr.get_status()
        assert "sources" in status
        assert "queue_depth" in status
        assert "recent_jobs" in status


# ── Manager: start/stop ───────────────────────────────────────────────────────

class TestCloudBackupManagerLifecycle:
    @pytest.mark.asyncio
    async def test_start_stop(self, tmp_path):
        from cloud_backup import CloudBackupManager
        mgr = CloudBackupManager(tmp_path)
        await mgr.start()
        assert len(mgr._tasks) > 0
        await mgr.stop()
        assert all(t.done() for t in mgr._tasks)

    @pytest.mark.asyncio
    async def test_queue_depth_initially_zero(self, tmp_path):
        from cloud_backup import CloudBackupManager
        mgr = CloudBackupManager(tmp_path)
        await mgr.start()
        assert mgr.get_status()["queue_depth"] == 0
        await mgr.stop()


# ── Worker pool ───────────────────────────────────────────────────────────────

class TestBackupWorkerPool:
    @pytest.mark.asyncio
    async def test_job_dispatched_and_done(self, tmp_path):
        from cloud_backup import (
            BackupWorkerPool, BackupJob, JobType, Provider, JobStatus
        )

        queue = asyncio.PriorityQueue()
        m365 = MagicMock()
        m365.backup_mailbox = AsyncMock(return_value=(5, 1024))
        google = MagicMock()

        pool = BackupWorkerPool(queue, m365, google, max_workers=1, max_per_provider=1)
        completed = []
        pool._on_job_done = lambda j: completed.append(j)

        await pool.start(n_workers=1)

        job = BackupJob(
            job_type=JobType.M365_MAILBOX,
            provider=Provider.M365,
            tenant_id="src-1",
            user_id="user@ex.com",
        )
        await queue.put((0, 0, job))
        await asyncio.sleep(0.2)
        await pool.stop()

        assert any(j.status == JobStatus.DONE for j in completed)
        done_job = next(j for j in completed if j.status == JobStatus.DONE)
        assert done_job.items_backed_up == 5
        assert done_job.bytes_backed_up == 1024

    @pytest.mark.asyncio
    async def test_rate_limit_error_requeues(self, tmp_path):
        from cloud_backup import (
            BackupWorkerPool, BackupJob, JobType, Provider,
            RateLimitError, JobStatus
        )

        queue = asyncio.PriorityQueue()
        m365 = MagicMock()
        call_count = 0

        async def _failing(*args, **kwargs):
            nonlocal call_count
            call_count += 1
            if call_count == 1:
                raise RateLimitError("throttled")
            return (1, 100)

        m365.backup_mailbox = _failing
        google = MagicMock()

        pool = BackupWorkerPool(queue, m365, google, max_workers=1, max_per_provider=1)
        completed = []
        pool._on_job_done = lambda j: completed.append(j)

        # Override retry delay to be instant
        with patch("cloud_backup._RETRY_BASE_DELAY", 0.01):
            await pool.start(n_workers=1)
            job = BackupJob(
                job_type=JobType.M365_MAILBOX,
                provider=Provider.M365,
                tenant_id="src-1",
                user_id="u@ex.com",
            )
            await queue.put((0, 0, job))
            await asyncio.sleep(0.5)
            await pool.stop()

        # Should have been retried and eventually succeeded
        done = [j for j in completed if j.status == JobStatus.DONE]
        assert done

    @pytest.mark.asyncio
    async def test_dead_letter_after_max_attempts(self, tmp_path):
        from cloud_backup import (
            BackupWorkerPool, BackupJob, JobType, Provider, JobStatus
        )

        queue = asyncio.PriorityQueue()
        m365 = MagicMock()
        m365.backup_mailbox = AsyncMock(side_effect=Exception("permanent failure"))
        google = MagicMock()

        pool = BackupWorkerPool(queue, m365, google, max_workers=1, max_per_provider=1)
        completed = []
        pool._on_job_done = lambda j: completed.append(j)

        with patch("cloud_backup._RETRY_BASE_DELAY", 0.001):
            with patch("cloud_backup._MAX_JOB_ATTEMPTS", 2):
                await pool.start(n_workers=1)
                job = BackupJob(
                    job_type=JobType.M365_MAILBOX,
                    provider=Provider.M365,
                    tenant_id="src-1",
                    user_id="u@ex.com",
                )
                await queue.put((0, 0, job))
                await asyncio.sleep(0.5)
                await pool.stop()

        dead = [j for j in completed if j.status == JobStatus.DEAD_LETTER]
        assert dead


# ── Cron scheduler ────────────────────────────────────────────────────────────

class TestCronScheduler:
    def test_due_when_past_time_and_never_run(self):
        from cloud_backup import _cron_is_due
        import datetime
        # "0 0 * * *" = midnight; if we haven't run today it's due
        assert _cron_is_due("0 0 * * *", 0.0)

    def test_not_due_if_run_today(self):
        from cloud_backup import _cron_is_due
        # last run = just now → not due again today
        assert not _cron_is_due("0 0 * * *", time.time())

    def test_invalid_cron_returns_false(self):
        from cloud_backup import _cron_is_due
        assert not _cron_is_due("not a cron", 0.0)
        assert not _cron_is_due("", 0.0)


# ── Key derivation ────────────────────────────────────────────────────────────

class TestKeyDerivation:
    def test_deterministic(self):
        from cloud_backup import _derive_key
        key_bytes = b"test-mesh-key-32-bytes-long!!!!!"
        k1 = _derive_key(key_bytes)
        k2 = _derive_key(key_bytes)
        assert k1 == k2
        assert len(k1) == 32

    def test_different_inputs_different_keys(self):
        from cloud_backup import _derive_key
        k1 = _derive_key(b"key-a")
        k2 = _derive_key(b"key-b")
        assert k1 != k2

    def test_none_input_returns_32_bytes(self):
        from cloud_backup import _derive_key
        # None → random IKM → random key (non-deterministic but correct length)
        k = _derive_key(None)
        assert len(k) == 32


# ── Encrypt/decrypt roundtrip ─────────────────────────────────────────────────

class TestEncryption:
    def test_encrypt_decrypt_roundtrip(self):
        from cloud_backup import _encrypt_json, _decrypt_json, _derive_key
        key = _derive_key(b"test-key")
        data = {"client_secret": "s3cr3t", "tenant_id": "abc123"}
        encrypted = _encrypt_json(data, key)
        decrypted = _decrypt_json(encrypted, key)
        assert decrypted["client_secret"] == "s3cr3t"
        assert decrypted["tenant_id"] == "abc123"

    def test_wrong_key_raises(self):
        from cloud_backup import _encrypt_json, _decrypt_json, _derive_key
        key1 = _derive_key(b"key1")
        key2 = _derive_key(b"key2")
        encrypted = _encrypt_json({"x": 1}, key1)
        if not encrypted.startswith(b"PLAIN:"):   # only if cryptography is available
            with pytest.raises(Exception):
                _decrypt_json(encrypted, key2)


# ── rclone agent ──────────────────────────────────────────────────────────────

class TestRcloneBackupAgent:
    """Tests for RcloneBackupAgent — all subprocess calls are mocked."""

    def _make_agent(self, tmp_path):
        from cloud_backup import RcloneBackupAgent
        return RcloneBackupAgent(tmp_path, mesh_key_bytes=b"test-mesh-key")

    @pytest.mark.asyncio
    async def test_check_available_true(self, tmp_path):
        agent = self._make_agent(tmp_path)
        mock_proc = MagicMock()
        mock_proc.returncode = 0
        mock_proc.communicate = AsyncMock(
            return_value=(b"rclone v1.66.0\nOS/Arch: linux/amd64\n", b"")
        )
        with patch("asyncio.create_subprocess_exec", return_value=mock_proc):
            available, ver = await agent.check_available()
        assert available is True
        assert "rclone" in ver

    @pytest.mark.asyncio
    async def test_check_available_false_when_not_installed(self, tmp_path):
        agent = self._make_agent(tmp_path)
        with patch("asyncio.create_subprocess_exec", side_effect=FileNotFoundError):
            available, ver = await agent.check_available()
        assert available is False
        assert ver == ""

    @pytest.mark.asyncio
    async def test_check_available_false_on_nonzero_rc(self, tmp_path):
        agent = self._make_agent(tmp_path)
        mock_proc = MagicMock()
        mock_proc.returncode = 1
        mock_proc.communicate = AsyncMock(return_value=(b"", b"error"))
        with patch("asyncio.create_subprocess_exec", return_value=mock_proc):
            available, _ = await agent.check_available()
        assert available is False

    @pytest.mark.asyncio
    async def test_list_remotes_empty_when_no_conf(self, tmp_path):
        agent = self._make_agent(tmp_path)
        # No rclone.conf → empty list without invoking rclone
        result = await agent.list_remotes()
        assert result == []

    @pytest.mark.asyncio
    async def test_list_remotes_parses_output(self, tmp_path):
        agent = self._make_agent(tmp_path)
        agent._conf.touch()  # ensure file exists so we proceed to subprocess
        mock_proc = MagicMock()
        mock_proc.returncode = 0
        mock_proc.communicate = AsyncMock(
            return_value=(b"mys3: s3\ndropbox: dropbox\n", b"")
        )
        with patch("asyncio.create_subprocess_exec", return_value=mock_proc):
            remotes = await agent.list_remotes()
        assert len(remotes) == 2
        assert remotes[0] == {"name": "mys3", "type": "s3"}
        assert remotes[1] == {"name": "dropbox", "type": "dropbox"}

    @pytest.mark.asyncio
    async def test_list_remotes_nonzero_rc_returns_empty(self, tmp_path):
        agent = self._make_agent(tmp_path)
        agent._conf.touch()
        mock_proc = MagicMock()
        mock_proc.returncode = 1
        mock_proc.communicate = AsyncMock(return_value=(b"", b"err"))
        with patch("asyncio.create_subprocess_exec", return_value=mock_proc):
            result = await agent.list_remotes()
        assert result == []

    @pytest.mark.asyncio
    async def test_configure_remote_success(self, tmp_path):
        agent = self._make_agent(tmp_path)
        mock_proc = MagicMock()
        mock_proc.returncode = 0
        mock_proc.communicate = AsyncMock(return_value=(b"", b""))
        with patch("asyncio.create_subprocess_exec", return_value=mock_proc) as mock_exec:
            ok = await agent.configure_remote("mys3", "s3", {
                "access_key_id": "AKID",
                "secret_access_key": "secret",
            })
        assert ok is True
        cmd = mock_exec.call_args[0]
        assert "config" in cmd
        assert "create" in cmd
        assert "mys3" in cmd
        assert "s3" in cmd

    @pytest.mark.asyncio
    async def test_configure_remote_creates_conf_file(self, tmp_path):
        agent = self._make_agent(tmp_path)
        mock_proc = MagicMock()
        mock_proc.returncode = 0
        mock_proc.communicate = AsyncMock(return_value=(b"", b""))
        with patch("asyncio.create_subprocess_exec", return_value=mock_proc):
            await agent.configure_remote("r", "local", {})
        assert agent._conf.exists()

    @pytest.mark.asyncio
    async def test_configure_remote_failure(self, tmp_path):
        agent = self._make_agent(tmp_path)
        mock_proc = MagicMock()
        mock_proc.returncode = 1
        mock_proc.communicate = AsyncMock(return_value=(b"", b"config error"))
        with patch("asyncio.create_subprocess_exec", return_value=mock_proc):
            ok = await agent.configure_remote("bad", "s3", {})
        assert ok is False

    @pytest.mark.asyncio
    async def test_delete_remote_success(self, tmp_path):
        agent = self._make_agent(tmp_path)
        mock_proc = MagicMock()
        mock_proc.returncode = 0
        mock_proc.communicate = AsyncMock(return_value=(b"", b""))
        with patch("asyncio.create_subprocess_exec", return_value=mock_proc):
            ok = await agent.delete_remote("mys3")
        assert ok is True

    @pytest.mark.asyncio
    async def test_delete_remote_failure(self, tmp_path):
        agent = self._make_agent(tmp_path)
        mock_proc = MagicMock()
        mock_proc.returncode = 1
        mock_proc.communicate = AsyncMock(return_value=(b"", b"err"))
        with patch("asyncio.create_subprocess_exec", return_value=mock_proc):
            ok = await agent.delete_remote("missing")
        assert ok is False

    @pytest.mark.asyncio
    async def test_sync_success(self, tmp_path):
        agent = self._make_agent(tmp_path)
        dest = tmp_path / "backup"
        mock_proc = MagicMock()
        mock_proc.returncode = 0
        # Simulate one JSON stats line
        stats_line = json.dumps({
            "stats": {"transfers": 42, "bytes": 1024000}
        }).encode()
        mock_proc.communicate = AsyncMock(return_value=(stats_line, b""))
        with patch("asyncio.create_subprocess_exec", return_value=mock_proc):
            items, nbytes = await agent.sync(
                "src-1", "mys3:/bucket/path", dest
            )
        assert items == 42
        assert nbytes == 1024000
        assert dest.exists()

    @pytest.mark.asyncio
    async def test_sync_passes_tpslimit_and_bwlimit(self, tmp_path):
        agent = self._make_agent(tmp_path)
        dest = tmp_path / "backup"
        mock_proc = MagicMock()
        mock_proc.returncode = 0
        mock_proc.communicate = AsyncMock(return_value=(b"", b""))
        with patch("asyncio.create_subprocess_exec", return_value=mock_proc) as mock_exec:
            await agent.sync(
                "src-1", "b2:bucket", dest,
                tpslimit=5.0, bwlimit="10M",
            )
        cmd = list(mock_exec.call_args[0])
        assert "--tpslimit" in cmd
        assert "5.0" in cmd
        assert "--bwlimit" in cmd
        assert "10M" in cmd

    @pytest.mark.asyncio
    async def test_sync_skips_tpslimit_when_zero(self, tmp_path):
        agent = self._make_agent(tmp_path)
        dest = tmp_path / "backup"
        mock_proc = MagicMock()
        mock_proc.returncode = 0
        mock_proc.communicate = AsyncMock(return_value=(b"", b""))
        with patch("asyncio.create_subprocess_exec", return_value=mock_proc) as mock_exec:
            await agent.sync("src-1", "remote:/path", dest, tpslimit=0.0)
        cmd = list(mock_exec.call_args[0])
        assert "--tpslimit" not in cmd

    @pytest.mark.asyncio
    async def test_sync_raises_on_failure(self, tmp_path):
        agent = self._make_agent(tmp_path)
        dest = tmp_path / "backup"
        mock_proc = MagicMock()
        mock_proc.returncode = 1
        mock_proc.communicate = AsyncMock(return_value=(b"", b"fatal error"))
        with patch("asyncio.create_subprocess_exec", return_value=mock_proc):
            with pytest.raises(RuntimeError, match="rclone exited 1"):
                await agent.sync("src-1", "remote:/path", dest)

    @pytest.mark.asyncio
    async def test_sync_creates_backup_dir(self, tmp_path):
        agent = self._make_agent(tmp_path)
        dest = tmp_path / "deep" / "nested" / "backup"
        mock_proc = MagicMock()
        mock_proc.returncode = 0
        mock_proc.communicate = AsyncMock(return_value=(b"", b""))
        with patch("asyncio.create_subprocess_exec", return_value=mock_proc) as mock_exec:
            await agent.sync("src-1", "remote:/path", dest)
        cmd = list(mock_exec.call_args[0])
        assert "--backup-dir" in cmd
        # Both dest and backup-dir should be created
        assert dest.exists()

    @pytest.mark.asyncio
    async def test_sync_passes_extra_flags(self, tmp_path):
        agent = self._make_agent(tmp_path)
        dest = tmp_path / "backup"
        mock_proc = MagicMock()
        mock_proc.returncode = 0
        mock_proc.communicate = AsyncMock(return_value=(b"", b""))
        with patch("asyncio.create_subprocess_exec", return_value=mock_proc) as mock_exec:
            await agent.sync(
                "src-1", "remote:/path", dest,
                extra_flags=["--exclude", "*.tmp"],
            )
        cmd = list(mock_exec.call_args[0])
        assert "--exclude" in cmd
        assert "*.tmp" in cmd

    def test_conf_pass_derived_from_mesh_key(self, tmp_path):
        from cloud_backup import RcloneBackupAgent
        agent = RcloneBackupAgent(tmp_path, mesh_key_bytes=b"my-secret-mesh-key")
        assert agent._conf_pass is not None
        assert len(agent._conf_pass) == 32

    def test_conf_pass_none_without_mesh_key(self, tmp_path):
        from cloud_backup import RcloneBackupAgent
        agent = RcloneBackupAgent(tmp_path, mesh_key_bytes=None)
        assert agent._conf_pass is None

    def test_conf_pass_deterministic(self, tmp_path):
        from cloud_backup import RcloneBackupAgent
        a1 = RcloneBackupAgent(tmp_path, mesh_key_bytes=b"key")
        a2 = RcloneBackupAgent(tmp_path, mesh_key_bytes=b"key")
        assert a1._conf_pass == a2._conf_pass

    def test_conf_pass_different_for_different_keys(self, tmp_path):
        from cloud_backup import RcloneBackupAgent
        a1 = RcloneBackupAgent(tmp_path / "a", mesh_key_bytes=b"key-a")
        a2 = RcloneBackupAgent(tmp_path / "b", mesh_key_bytes=b"key-b")
        assert a1._conf_pass != a2._conf_pass


# ── rclone integration in BackupSource ───────────────────────────────────────

class TestBackupSourceRclone:
    def test_rclone_fields_serialise(self):
        from cloud_backup import BackupSource, Provider
        src = BackupSource(
            name="My B2 bucket",
            provider=Provider.RCLONE,
            rclone_remote="myb2",
            rclone_source_path="/photos",
            tpslimit=5.0,
            bwlimit="20M",
            rclone_flags=["--exclude", "*.tmp"],
        )
        d = src.to_dict()
        assert d["rclone_remote"] == "myb2"
        assert d["rclone_source_path"] == "/photos"
        assert d["tpslimit"] == 5.0
        assert d["bwlimit"] == "20M"
        assert d["rclone_flags"] == ["--exclude", "*.tmp"]

    def test_rclone_fields_roundtrip(self):
        from cloud_backup import BackupSource, Provider
        src = BackupSource(
            name="Dropbox",
            provider=Provider.RCLONE,
            rclone_remote="drop",
            rclone_source_path="/docs",
            tpslimit=2.5,
            bwlimit="5M",
        )
        restored = BackupSource.from_dict(src.to_dict())
        assert restored.rclone_remote == "drop"
        assert restored.rclone_source_path == "/docs"
        assert restored.tpslimit == 2.5
        assert restored.bwlimit == "5M"

    def test_rclone_flags_default_empty(self):
        from cloud_backup import BackupSource, Provider
        src = BackupSource(provider=Provider.RCLONE)
        assert src.rclone_flags == []
        assert src.tpslimit == 0.0
        assert src.bwlimit == ""


# ── Provider enum includes RCLONE ─────────────────────────────────────────────

class TestProviderEnum:
    def test_rclone_in_provider(self):
        from cloud_backup import Provider
        assert Provider.RCLONE == "rclone"
        assert Provider("rclone") == Provider.RCLONE


# ── JobType includes RCLONE_SYNC ──────────────────────────────────────────────

class TestJobTypeEnum:
    def test_rclone_sync_in_job_type(self):
        from cloud_backup import JobType
        assert JobType.RCLONE_SYNC == "rclone_sync"
        assert JobType("rclone_sync") == JobType.RCLONE_SYNC


# ── Worker pool handles rclone jobs ──────────────────────────────────────────

class TestWorkerPoolRclone:
    @pytest.mark.asyncio
    async def test_dispatch_rclone_sync(self, tmp_path):
        from cloud_backup import (
            BackupWorkerPool, BackupJob, JobType, Provider,
            RcloneBackupAgent,
        )
        queue = asyncio.PriorityQueue()
        m365 = MagicMock()
        google = MagicMock()
        rclone = MagicMock(spec=RcloneBackupAgent)
        rclone._dir = tmp_path
        rclone.sync = AsyncMock(return_value=(10, 512))

        pool = BackupWorkerPool(queue, m365, google, rclone_agent=rclone,
                                max_workers=1, max_per_provider=1)
        completed = []
        pool._on_job_done = lambda j: completed.append(j)

        await pool.start(n_workers=1)
        job = BackupJob(
            job_type=JobType.RCLONE_SYNC,
            provider=Provider.RCLONE,
            tenant_id="src-1",
            user_id="mys3:/mybucket",
            meta={"tpslimit": 3.0, "bwlimit": "10M", "flags": []},
        )
        await queue.put((0, 0, job))
        await asyncio.sleep(0.3)
        await pool.stop()

        assert len(completed) == 1
        assert completed[0].items_backed_up == 10
        assert completed[0].bytes_backed_up == 512
        rclone.sync.assert_called_once()
        call_kwargs = rclone.sync.call_args
        assert call_kwargs[1].get("tpslimit") == 3.0 or 3.0 in call_kwargs[0]

    @pytest.mark.asyncio
    async def test_dispatch_rclone_raises_when_no_agent(self, tmp_path):
        from cloud_backup import (
            BackupWorkerPool, BackupJob, JobType, Provider, JobStatus,
        )
        queue = asyncio.PriorityQueue()
        m365 = MagicMock()
        google = MagicMock()

        pool = BackupWorkerPool(queue, m365, google, rclone_agent=None,
                                max_workers=1, max_per_provider=1)
        completed = []
        pool._on_job_done = lambda j: completed.append(j)

        with patch("cloud_backup._RETRY_BASE_DELAY", 0.01):
            await pool.start(n_workers=1)
            job = BackupJob(
                job_type=JobType.RCLONE_SYNC,
                provider=Provider.RCLONE,
                tenant_id="src-1",
                user_id="r:/path",
            )
            await queue.put((0, 0, job))
            await asyncio.sleep(0.5)
            await pool.stop()

        # Job should have been attempted at least once
        assert len(completed) >= 1
        # Should be failed or queued-for-retry, not done
        assert completed[-1].status != JobStatus.DONE
        assert "rclone agent not available" in completed[-1].last_error

    @pytest.mark.asyncio
    async def test_rclone_semaphore_present(self, tmp_path):
        from cloud_backup import BackupWorkerPool, Provider
        import asyncio as _asyncio
        queue = _asyncio.PriorityQueue()
        m365 = MagicMock()
        google = MagicMock()
        pool = BackupWorkerPool(queue, m365, google, rclone_agent=None)
        assert Provider.RCLONE in pool._prov_sems


# ── BackupQueue abstraction ───────────────────────────────────────────────────

class TestLocalBackupQueue:
    @pytest.mark.asyncio
    async def test_put_and_get(self):
        from cloud_backup import LocalBackupQueue, BackupJob
        q = LocalBackupQueue()
        job = BackupJob()
        await q.put(job, priority=1)
        assert q.qsize() == 1
        result = await q.get()
        assert result is job

    @pytest.mark.asyncio
    async def test_priority_ordering(self):
        from cloud_backup import LocalBackupQueue, BackupJob
        q = LocalBackupQueue()
        low = BackupJob()
        high = BackupJob()
        await q.put(low, priority=5)
        await q.put(high, priority=0)
        first = await q.get()
        second = await q.get()
        assert first is high
        assert second is low

    def test_is_not_external(self):
        from cloud_backup import LocalBackupQueue
        q = LocalBackupQueue()
        assert q.is_external() is False

    def test_qsize(self):
        from cloud_backup import LocalBackupQueue
        q = LocalBackupQueue()
        assert q.qsize() == 0


# ── CloudBackupManager queue mode ────────────────────────────────────────────

class TestCloudBackupManagerQueueMode:
    def test_defaults_to_local_queue(self, tmp_path):
        from cloud_backup import CloudBackupManager, LocalBackupQueue
        mgr = CloudBackupManager(tmp_path)
        assert isinstance(mgr._queue, LocalBackupQueue)
        assert mgr._external_queue is False

    def test_external_queue_injected(self, tmp_path):
        from cloud_backup import CloudBackupManager, LocalBackupQueue, BackupQueue
        # Inject a custom queue subclass
        class FakeExternalQueue(BackupQueue):
            async def put(self, job, priority=1):
                pass
            async def get(self):
                pass
            def is_external(self):
                return True

        q = FakeExternalQueue()
        mgr = CloudBackupManager(tmp_path, queue=q)
        assert mgr._queue is q
        assert mgr._external_queue is True

    def test_status_shows_queue_mode(self, tmp_path):
        from cloud_backup import CloudBackupManager
        mgr = CloudBackupManager(tmp_path)
        status = mgr.get_status()
        assert status["queue_mode"] == "local"


# ── BackupJob.from_dict roundtrip ─────────────────────────────────────────────

class TestBackupJobFromDict:
    def test_roundtrip(self):
        from cloud_backup import BackupJob, JobType, Provider, JobStatus
        job = BackupJob(
            job_type=JobType.M365_MAILBOX,
            provider=Provider.M365,
            tenant_id="src-1",
            user_id="user@example.com",
            priority=2,
            attempts=3,
            status=JobStatus.DONE,
            last_error="",
            items_backed_up=42,
            bytes_backed_up=1024,
            started_at=1000.0,
            finished_at=1010.0,
            meta={"tpslimit": 5.0},
        )
        d = job.to_dict()
        restored = BackupJob.from_dict(d)
        assert restored.id == job.id
        assert restored.job_type == JobType.M365_MAILBOX
        assert restored.provider == Provider.M365
        assert restored.tenant_id == "src-1"
        assert restored.user_id == "user@example.com"
        assert restored.priority == 2
        assert restored.attempts == 3
        assert restored.status == JobStatus.DONE
        assert restored.items_backed_up == 42
        assert restored.bytes_backed_up == 1024
        assert restored.started_at == 1000.0
        assert restored.finished_at == 1010.0
        assert restored.meta == {"tpslimit": 5.0}

    def test_meta_in_to_dict(self):
        from cloud_backup import BackupJob, JobType, Provider
        job = BackupJob(
            job_type=JobType.RCLONE_SYNC,
            provider=Provider.RCLONE,
            meta={"flags": ["--exclude", "*.tmp"], "bwlimit": "10M"},
        )
        d = job.to_dict()
        assert "meta" in d
        assert d["meta"]["bwlimit"] == "10M"

    def test_from_dict_defaults(self):
        from cloud_backup import BackupJob, JobType, Provider, JobStatus
        # Minimal dict — only required fields
        d = {"job_type": "m365_mailbox", "provider": "m365"}
        job = BackupJob.from_dict(d)
        assert job.job_type == JobType.M365_MAILBOX
        assert job.provider == Provider.M365
        assert job.status == JobStatus.QUEUED
        assert job.meta == {}


# ── LocalResultChannel ────────────────────────────────────────────────────────

class TestLocalResultChannel:
    @pytest.mark.asyncio
    async def test_push_is_noop(self):
        from cloud_backup import LocalResultChannel, BackupJob
        ch = LocalResultChannel()
        job = BackupJob()
        # Should not raise and return None
        result = await ch.push_result(job)
        assert result is None

    @pytest.mark.asyncio
    async def test_drain_returns_empty(self):
        from cloud_backup import LocalResultChannel
        ch = LocalResultChannel()
        results = await ch.drain()
        assert results == []


# ── ConnectResultChannel ──────────────────────────────────────────────────────

class TestConnectResultChannel:
    def _make_channel(self):
        from cloud_backup import ConnectResultChannel
        return ConnectResultChannel(
            connect_url="https://connect.ozma.dev",
            token="test-token",
            controller_id="ctrl-abc",
        )

    @pytest.mark.asyncio
    async def test_push_result_posts_to_connect(self):
        ch = self._make_channel()
        from cloud_backup import BackupJob, JobType, Provider, JobStatus
        job = BackupJob(
            job_type=JobType.M365_MAILBOX,
            provider=Provider.M365,
            tenant_id="src-1",
            user_id="u@ex.com",
            status=JobStatus.DONE,
            items_backed_up=10,
        )

        calls = []
        def _fake_urlopen(req, timeout=None):
            calls.append({
                "url": req.full_url,
                "method": req.get_method(),
                "body": json.loads(req.data),
            })
            mock_resp = MagicMock()
            mock_resp.status = 200
            mock_resp.__enter__ = lambda s: s
            mock_resp.__exit__ = MagicMock(return_value=False)
            return mock_resp

        with patch("urllib.request.urlopen", _fake_urlopen):
            await ch.push_result(job)

        assert len(calls) == 1
        assert calls[0]["method"] == "POST"
        assert "/backup/results" in calls[0]["url"]
        assert calls[0]["body"]["controller_id"] == "ctrl-abc"
        assert calls[0]["body"]["job"]["tenant_id"] == "src-1"

    @pytest.mark.asyncio
    async def test_push_result_logs_on_failure(self):
        ch = self._make_channel()
        from cloud_backup import BackupJob
        job = BackupJob()
        import urllib.error
        with patch("urllib.request.urlopen", side_effect=Exception("network error")):
            # Should not raise — just log warning
            await ch.push_result(job)

    @pytest.mark.asyncio
    async def test_drain_deserialises_jobs(self):
        ch = self._make_channel()
        from cloud_backup import BackupJob, JobType, Provider, JobStatus
        job = BackupJob(
            job_type=JobType.GOOGLE_DRIVE,
            provider=Provider.GOOGLE,
            tenant_id="src-2",
            user_id="bob@example.com",
            status=JobStatus.DONE,
            items_backed_up=7,
            bytes_backed_up=9000,
        )
        response_body = json.dumps({"results": [job.to_dict()]}).encode()

        mock_resp = MagicMock()
        mock_resp.read.return_value = response_body
        mock_resp.__enter__ = lambda s: s
        mock_resp.__exit__ = MagicMock(return_value=False)

        with patch("urllib.request.urlopen", return_value=mock_resp):
            results = await ch.drain()

        assert len(results) == 1
        assert results[0].tenant_id == "src-2"
        assert results[0].user_id == "bob@example.com"
        assert results[0].items_backed_up == 7
        assert results[0].status == JobStatus.DONE

    @pytest.mark.asyncio
    async def test_drain_returns_empty_on_failure(self):
        ch = self._make_channel()
        with patch("urllib.request.urlopen", side_effect=Exception("offline")):
            results = await ch.drain()
        assert results == []

    @pytest.mark.asyncio
    async def test_drain_includes_since_param(self):
        ch = self._make_channel()
        ch._last_drain_at = 1234567890.0

        captured_url = []
        mock_resp = MagicMock()
        mock_resp.read.return_value = json.dumps({"results": []}).encode()
        mock_resp.__enter__ = lambda s: s
        mock_resp.__exit__ = MagicMock(return_value=False)

        def _fake_urlopen(req, timeout=None):
            captured_url.append(req.full_url)
            return mock_resp

        with patch("urllib.request.urlopen", _fake_urlopen):
            await ch.drain()

        assert "since=1234567890" in captured_url[0]

    @pytest.mark.asyncio
    async def test_drain_updates_last_drain_at(self):
        ch = self._make_channel()
        assert ch._last_drain_at == 0.0
        mock_resp = MagicMock()
        mock_resp.read.return_value = json.dumps({"results": []}).encode()
        mock_resp.__enter__ = lambda s: s
        mock_resp.__exit__ = MagicMock(return_value=False)
        with patch("urllib.request.urlopen", return_value=mock_resp):
            await ch.drain()
        assert ch._last_drain_at > 0.0


# ── ConnectDeltaStateStore ────────────────────────────────────────────────────

class TestConnectDeltaStateStore:
    def _make_store(self):
        from cloud_backup import ConnectDeltaStateStore
        return ConnectDeltaStateStore(
            connect_url="https://connect.ozma.dev",
            token="tok",
            controller_id="ctrl-1",
        )

    def test_get_cache_miss_returns_empty_on_404(self):
        store = self._make_store()
        import urllib.error, urllib.request
        with patch("urllib.request.urlopen", side_effect=Exception("404")):
            result = store.get("src", "m365_mailbox", "u@ex.com")
        assert result == {}

    def test_get_cache_hit(self):
        store = self._make_store()
        key = "src-1:m365_mailbox:u@ex.com"
        store._cache[key] = {"delta_link": "https://graph.../delta?token=abc"}
        result = store.get("src-1", "m365_mailbox", "u@ex.com")
        assert result["delta_link"].startswith("https://graph")

    def test_get_populates_cache(self):
        store = self._make_store()
        checkpoint = {"delta_link": "https://example.com/delta?t=xyz"}
        response_body = json.dumps({"checkpoint": checkpoint}).encode()
        mock_resp = MagicMock()
        mock_resp.read.return_value = response_body
        mock_resp.__enter__ = lambda s: s
        mock_resp.__exit__ = MagicMock(return_value=False)
        with patch("urllib.request.urlopen", return_value=mock_resp):
            result = store.get("src-1", "m365_mailbox", "u@ex.com")
        assert result == checkpoint
        # Second call should hit cache
        key = "src-1:m365_mailbox:u@ex.com"
        assert store._cache[key] == checkpoint

    def test_set_updates_cache_and_puts_to_connect(self):
        store = self._make_store()
        calls = []
        mock_resp = MagicMock()
        mock_resp.__enter__ = lambda s: s
        mock_resp.__exit__ = MagicMock(return_value=False)
        mock_resp.close = MagicMock()

        def _fake_urlopen(req, timeout=None):
            calls.append({"method": req.get_method(), "url": req.full_url})
            return mock_resp

        with patch("urllib.request.urlopen", _fake_urlopen):
            store.set("src-1", "m365_mailbox", "u@ex.com", {"delta_link": "tok"})

        key = "src-1:m365_mailbox:u@ex.com"
        assert store._cache[key]["delta_link"] == "tok"
        assert len(calls) == 1
        assert calls[0]["method"] == "PUT"
        assert "src-1:m365_mailbox" in calls[0]["url"]

    def test_set_does_not_raise_on_network_error(self):
        store = self._make_store()
        with patch("urllib.request.urlopen", side_effect=Exception("timeout")):
            # Should not raise
            store.set("src-1", "m365_mailbox", "u@ex.com", {"delta_link": "tok"})
        # Cache was still updated despite network failure
        key = "src-1:m365_mailbox:u@ex.com"
        assert store._cache[key]["delta_link"] == "tok"

    def test_clear_removes_from_cache(self):
        store = self._make_store()
        store._cache["src-1:m365_mailbox:u@ex.com"] = {"delta_link": "abc"}
        store._cache["src-1:m365_onedrive:u@ex.com"] = {"delta_link": "def"}
        store._cache["src-2:m365_mailbox:other@ex.com"] = {"delta_link": "xyz"}

        mock_resp = MagicMock()
        mock_resp.__enter__ = lambda s: s
        mock_resp.__exit__ = MagicMock(return_value=False)
        mock_resp.close = MagicMock()

        with patch("urllib.request.urlopen", return_value=mock_resp):
            store.clear("src-1")

        assert "src-1:m365_mailbox:u@ex.com" not in store._cache
        assert "src-1:m365_onedrive:u@ex.com" not in store._cache
        assert "src-2:m365_mailbox:other@ex.com" in store._cache

    def test_clear_does_not_raise_on_network_error(self):
        store = self._make_store()
        store._cache["src-1:m365_mailbox:u@ex.com"] = {"delta_link": "abc"}
        with patch("urllib.request.urlopen", side_effect=Exception("offline")):
            store.clear("src-1")
        # Cache was cleared even if network call failed
        assert "src-1:m365_mailbox:u@ex.com" not in store._cache

    @pytest.mark.asyncio
    async def test_get_async_returns_checkpoint(self):
        store = self._make_store()
        checkpoint = {"history_id": "999"}
        response_body = json.dumps({"checkpoint": checkpoint}).encode()
        mock_resp = MagicMock()
        mock_resp.read.return_value = response_body
        mock_resp.__enter__ = lambda s: s
        mock_resp.__exit__ = MagicMock(return_value=False)
        with patch("urllib.request.urlopen", return_value=mock_resp):
            result = await store.get_async("src-1", "google_gmail", "u@g.com")
        assert result == checkpoint

    @pytest.mark.asyncio
    async def test_get_async_uses_cache(self):
        store = self._make_store()
        key = "src-1:google_gmail:u@g.com"
        store._cache[key] = {"history_id": "cached"}
        # Should not call urlopen
        with patch("urllib.request.urlopen", side_effect=Exception("should not call")):
            result = await store.get_async("src-1", "google_gmail", "u@g.com")
        assert result["history_id"] == "cached"


# ── Result poll loop ──────────────────────────────────────────────────────────

class TestResultPollLoop:
    @pytest.mark.asyncio
    async def test_poll_loop_updates_job_history(self, tmp_path):
        from cloud_backup import (
            CloudBackupManager, BackupResultChannel, BackupJob,
            JobType, Provider, JobStatus, LocalBackupQueue,
        )
        # External queue
        queue = LocalBackupQueue()

        # Fake result channel that returns one job then nothing
        class FakeResultChannel(BackupResultChannel):
            def __init__(self):
                self.drain_count = 0
            async def push_result(self, job): pass
            async def drain(self):
                self.drain_count += 1
                if self.drain_count == 1:
                    return [BackupJob(
                        job_type=JobType.M365_MAILBOX,
                        provider=Provider.M365,
                        tenant_id="src-1",
                        user_id="u@ex.com",
                        status=JobStatus.DONE,
                        items_backed_up=99,
                        bytes_backed_up=4096,
                        finished_at=time.time(),
                    )]
                return []

        channel = FakeResultChannel()
        mgr = CloudBackupManager(tmp_path, queue=queue, result_channel=channel)

        with patch("cloud_backup._RESULT_POLL_INTERVAL", 0.05):
            await mgr.start()
            await asyncio.sleep(0.2)
            await mgr.stop()

        assert len(mgr._job_history) >= 1
        assert mgr._job_history[0].items_backed_up == 99

    @pytest.mark.asyncio
    async def test_poll_loop_updates_source_last_run(self, tmp_path):
        from cloud_backup import (
            CloudBackupManager, BackupResultChannel, BackupJob, BackupSource,
            JobType, Provider, JobStatus, LocalBackupQueue,
        )
        queue = LocalBackupQueue()

        completed_job = BackupJob(
            job_type=JobType.GOOGLE_DRIVE,
            provider=Provider.GOOGLE,
            tenant_id="src-99",
            user_id="u@g.com",
            status=JobStatus.DONE,
            items_backed_up=5,
            finished_at=time.time(),
        )

        class FakeResultChannel(BackupResultChannel):
            def __init__(self): self._called = False
            async def push_result(self, job): pass
            async def drain(self):
                if not self._called:
                    self._called = True
                    return [completed_job]
                return []

        channel = FakeResultChannel()
        mgr = CloudBackupManager(tmp_path, queue=queue, result_channel=channel)
        src = BackupSource(id="src-99", name="Google", provider=Provider.GOOGLE)
        mgr._sources["src-99"] = src

        with patch("cloud_backup._RESULT_POLL_INTERVAL", 0.05):
            await mgr.start()
            await asyncio.sleep(0.2)
            await mgr.stop()

        assert mgr._sources["src-99"].last_run_status == JobStatus.DONE

    @pytest.mark.asyncio
    async def test_poll_loop_not_started_for_local_queue(self, tmp_path):
        from cloud_backup import CloudBackupManager
        mgr = CloudBackupManager(tmp_path)  # no external queue
        await mgr.start()
        task_names = [t.get_name() for t in mgr._tasks]
        assert "cloud-backup-result-poll" not in task_names
        await mgr.stop()

    @pytest.mark.asyncio
    async def test_poll_loop_started_for_external_queue(self, tmp_path):
        from cloud_backup import (
            CloudBackupManager, BackupResultChannel, LocalBackupQueue,
        )
        queue = LocalBackupQueue()

        class FakeResultChannel(BackupResultChannel):
            async def push_result(self, job): pass
            async def drain(self): return []

        mgr = CloudBackupManager(tmp_path, queue=queue,
                                  result_channel=FakeResultChannel())
        await mgr.start()
        task_names = [t.get_name() for t in mgr._tasks]
        assert "cloud-backup-result-poll" in task_names
        await mgr.stop()

    def test_set_result_channel_replaces_channel(self, tmp_path):
        from cloud_backup import (
            CloudBackupManager, LocalResultChannel, BackupResultChannel,
        )
        mgr = CloudBackupManager(tmp_path)
        assert isinstance(mgr.result_channel, LocalResultChannel)

        class NewChannel(BackupResultChannel):
            async def push_result(self, job): pass
            async def drain(self): return []

        ch = NewChannel()
        mgr.set_result_channel(ch)
        assert mgr.result_channel is ch

    @pytest.mark.asyncio
    async def test_on_job_done_updates_source_status(self, tmp_path):
        from cloud_backup import (
            CloudBackupManager, BackupJob, BackupSource,
            JobType, Provider, JobStatus,
        )
        mgr = CloudBackupManager(tmp_path)
        src = BackupSource(id="src-1", name="Test", provider=Provider.M365)
        mgr._sources["src-1"] = src

        job = BackupJob(
            job_type=JobType.M365_MAILBOX,
            provider=Provider.M365,
            tenant_id="src-1",
            status=JobStatus.DONE,
            finished_at=5000.0,
        )
        mgr._on_job_done(job)

        assert mgr._sources["src-1"].last_run_status == JobStatus.DONE
        assert mgr._sources["src-1"].last_run_at == 5000.0
