# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Unit tests for vaultwarden.py — config, lifecycle, status, backup paths."""
import sys
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

sys.path.insert(0, str(Path(__file__).parent.parent.parent.parent / "controller"))
pytestmark = pytest.mark.unit


# ── Config ──────────────────────────────────────────────────────────────────

class TestVaultwardenConfig:
    def test_defaults(self):
        from vaultwarden import VaultwardenConfig
        cfg = VaultwardenConfig()
        assert not cfg.enabled
        assert cfg.port == 8222
        assert cfg.signup_disabled is True
        assert cfg.oidc_enabled is False

    def test_from_env_disabled_by_default(self, monkeypatch):
        from vaultwarden import VaultwardenConfig
        monkeypatch.delenv("OZMA_VAULTWARDEN", raising=False)
        cfg = VaultwardenConfig.from_env()
        assert not cfg.enabled

    def test_from_env_enabled(self, monkeypatch):
        from vaultwarden import VaultwardenConfig
        monkeypatch.setenv("OZMA_VAULTWARDEN", "1")
        monkeypatch.setenv("OZMA_VAULTWARDEN_PORT", "9222")
        monkeypatch.setenv("OZMA_VAULTWARDEN_ADMIN_TOKEN", "mytoken")
        cfg = VaultwardenConfig.from_env()
        assert cfg.enabled
        assert cfg.port == 9222
        assert cfg.admin_token == "mytoken"

    def test_from_env_oidc(self, monkeypatch):
        from vaultwarden import VaultwardenConfig
        monkeypatch.setenv("OZMA_VAULTWARDEN", "1")
        monkeypatch.setenv("OZMA_VAULTWARDEN_OIDC", "1")
        monkeypatch.setenv("OZMA_VAULTWARDEN_OIDC_CLIENT_ID", "vw-client")
        monkeypatch.setenv("OZMA_VAULTWARDEN_OIDC_SECRET", "secret")
        monkeypatch.setenv("OZMA_VAULTWARDEN_OIDC_ISSUER", "https://ctrl.example.com/auth")
        cfg = VaultwardenConfig.from_env()
        assert cfg.oidc_enabled
        assert cfg.oidc_client_id == "vw-client"
        assert cfg.oidc_issuer_url == "https://ctrl.example.com/auth"


# ── Manager: disabled / no Docker ───────────────────────────────────────────

class TestVaultwardenManagerDisabled:
    @pytest.mark.asyncio
    async def test_start_disabled_is_noop(self, tmp_path):
        from vaultwarden import VaultwardenManager, VaultwardenConfig
        mgr = VaultwardenManager(VaultwardenConfig(enabled=False), tmp_path)
        await mgr.start()   # must not raise
        assert not mgr.get_status()["running"]

    @pytest.mark.asyncio
    async def test_stop_when_never_started(self, tmp_path):
        from vaultwarden import VaultwardenManager, VaultwardenConfig
        mgr = VaultwardenManager(VaultwardenConfig(enabled=False), tmp_path)
        await mgr.stop()    # must not raise

    @pytest.mark.asyncio
    async def test_start_skips_when_docker_absent(self, tmp_path):
        from vaultwarden import VaultwardenManager, VaultwardenConfig
        cfg = VaultwardenConfig(enabled=True, port=8222)
        mgr = VaultwardenManager(cfg, tmp_path)

        # _docker_available returns False
        mgr._docker_available = AsyncMock(return_value=False)
        await mgr.start()
        assert not mgr.get_status()["running"]


# ── Admin token ──────────────────────────────────────────────────────────────

class TestAdminToken:
    @pytest.mark.asyncio
    async def test_uses_config_token_if_set(self, tmp_path):
        from vaultwarden import VaultwardenManager, VaultwardenConfig
        cfg = VaultwardenConfig(admin_token="preset-token")
        mgr = VaultwardenManager(cfg, tmp_path)
        token = await mgr._ensure_admin_token()
        assert token == "preset-token"

    @pytest.mark.asyncio
    async def test_generates_token_when_absent(self, tmp_path):
        from vaultwarden import VaultwardenManager, VaultwardenConfig
        mgr = VaultwardenManager(VaultwardenConfig(), tmp_path)
        token = await mgr._ensure_admin_token()
        assert len(token) >= 32
        # Persisted to file
        assert mgr._token_file.exists()
        assert mgr._token_file.read_text().strip() == token

    @pytest.mark.asyncio
    async def test_reuses_persisted_token(self, tmp_path):
        from vaultwarden import VaultwardenManager, VaultwardenConfig
        mgr = VaultwardenManager(VaultwardenConfig(), tmp_path)
        first = await mgr._ensure_admin_token()
        second = await mgr._ensure_admin_token()
        assert first == second

    @pytest.mark.asyncio
    async def test_token_file_permissions(self, tmp_path):
        from vaultwarden import VaultwardenManager, VaultwardenConfig
        mgr = VaultwardenManager(VaultwardenConfig(), tmp_path)
        await mgr._ensure_admin_token()
        mode = mgr._token_file.stat().st_mode & 0o777
        assert mode == 0o600


# ── Status ───────────────────────────────────────────────────────────────────

class TestVaultwardenStatus:
    def test_get_status_structure(self, tmp_path):
        from vaultwarden import VaultwardenManager, VaultwardenConfig
        mgr = VaultwardenManager(VaultwardenConfig(), tmp_path)
        status = mgr.get_status()
        for key in ("running", "container_id", "port", "admin_panel_url",
                    "vault_url", "oidc_enabled", "last_healthy", "error"):
            assert key in status

    def test_initial_status_not_running(self, tmp_path):
        from vaultwarden import VaultwardenManager, VaultwardenConfig
        mgr = VaultwardenManager(VaultwardenConfig(), tmp_path)
        assert not mgr.get_status()["running"]
        assert mgr.get_status()["error"] == ""


# ── Backup paths ─────────────────────────────────────────────────────────────

class TestBackupPaths:
    def test_default_data_dir(self, tmp_path):
        from vaultwarden import VaultwardenManager, VaultwardenConfig
        mgr = VaultwardenManager(VaultwardenConfig(), tmp_path)
        paths = mgr.backup_paths()
        assert any("db.sqlite3" in p for p in paths)
        assert any("attachments" in p for p in paths)
        assert any("rsa_key.pem" in p for p in paths)
        assert any("sends" in p for p in paths)

    def test_custom_data_dir(self, tmp_path):
        from vaultwarden import VaultwardenManager, VaultwardenConfig
        data = str(tmp_path / "mydata")
        mgr = VaultwardenManager(VaultwardenConfig(data_dir=data), tmp_path)
        paths = mgr.backup_paths()
        assert all(p.startswith(data) for p in paths)

    def test_all_critical_files_present(self, tmp_path):
        from vaultwarden import VaultwardenManager, VaultwardenConfig
        mgr = VaultwardenManager(VaultwardenConfig(), tmp_path)
        paths = mgr.backup_paths()
        names = [Path(p).name for p in paths]
        for required in ("db.sqlite3", "rsa_key.pem", "rsa_key.pub.pem"):
            assert required in names, f"Missing required backup path: {required}"


# ── OIDC configuration ───────────────────────────────────────────────────────

class TestOidcConfig:
    def test_configure_oidc_updates_state(self, tmp_path):
        from vaultwarden import VaultwardenManager, VaultwardenConfig
        mgr = VaultwardenManager(VaultwardenConfig(), tmp_path)
        mgr.configure_oidc(
            client_id="vw-client",
            client_secret="vw-secret",
            issuer_url="https://ctrl.example.com/auth",
        )
        assert mgr._cfg.oidc_enabled
        assert mgr._cfg.oidc_client_id == "vw-client"
        assert mgr._cfg.oidc_client_secret == "vw-secret"
        assert mgr._cfg.oidc_issuer_url == "https://ctrl.example.com/auth"
        assert mgr.get_status()["oidc_enabled"]


# ── Config integration ───────────────────────────────────────────────────────

class TestConfigIntegration:
    def test_config_has_vaultwarden_fields(self):
        from config import Config
        cfg = Config()
        assert hasattr(cfg, "vaultwarden_enabled")
        assert hasattr(cfg, "vaultwarden_port")
        assert hasattr(cfg, "vaultwarden_data_dir")
        assert hasattr(cfg, "vaultwarden_admin_token")
        assert cfg.vaultwarden_enabled is False
        assert cfg.vaultwarden_port == 8222

    def test_config_from_env_vaultwarden(self, monkeypatch):
        from config import Config
        monkeypatch.setenv("OZMA_VAULTWARDEN", "1")
        monkeypatch.setenv("OZMA_VAULTWARDEN_PORT", "9333")
        cfg = Config.from_env()
        assert cfg.vaultwarden_enabled
        assert cfg.vaultwarden_port == 9333


# ── State integration ────────────────────────────────────────────────────────

class TestStateIntegration:
    def test_state_has_vaultwarden_manager_slot(self):
        from state import AppState
        s = AppState()
        assert hasattr(s, "vaultwarden_manager")
        assert s.vaultwarden_manager is None

    def test_state_accepts_manager(self, tmp_path):
        from state import AppState
        from vaultwarden import VaultwardenManager, VaultwardenConfig
        s = AppState()
        mgr = VaultwardenManager(VaultwardenConfig(), tmp_path)
        s.vaultwarden_manager = mgr
        assert s.vaultwarden_manager is mgr


# ── Container detection ──────────────────────────────────────────────────────

class TestContainerDetection:
    @pytest.mark.asyncio
    async def test_container_not_running_returns_empty(self, tmp_path):
        from vaultwarden import VaultwardenManager, VaultwardenConfig

        async def _fake_run(*args, **kw):
            proc = MagicMock()
            proc.returncode = 1
            proc.communicate = AsyncMock(return_value=(b"", b""))
            return proc

        mgr = VaultwardenManager(VaultwardenConfig(), tmp_path)
        with patch("asyncio.create_subprocess_exec", side_effect=_fake_run):
            result = await mgr._container_running()
        assert result == ""

    @pytest.mark.asyncio
    async def test_container_running_returns_id(self, tmp_path):
        from vaultwarden import VaultwardenManager, VaultwardenConfig

        fake_id = "a" * 64

        async def _fake_run(*args, **kw):
            proc = MagicMock()
            proc.returncode = 0
            proc.communicate = AsyncMock(return_value=(
                f"true|{fake_id}".encode(), b""
            ))
            return proc

        mgr = VaultwardenManager(VaultwardenConfig(), tmp_path)
        with patch("asyncio.create_subprocess_exec", side_effect=_fake_run):
            result = await mgr._container_running()
        assert result == fake_id[:12]
