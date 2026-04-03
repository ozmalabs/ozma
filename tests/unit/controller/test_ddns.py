# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Unit tests for DDNSManager, DDNSRecord, and DDNSConfig."""

import asyncio
import json
import sys
import time
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

sys.path.insert(0, str(Path(__file__).parents[3] / "controller"))

from ddns import DDNSConfig, DDNSManager, DDNSRecord


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _manager(tmp_path: Path) -> DDNSManager:
    return DDNSManager(state_path=tmp_path / "ddns_state.json")


def _sample_record(**kwargs) -> dict:
    defaults = dict(
        name="Home DDNS",
        provider="cloudflare",
        credentials={"zone_id": "abc123", "api_token": "tok456"},
        hostnames=["home.example.com"],
    )
    defaults.update(kwargs)
    return defaults


# ---------------------------------------------------------------------------
# TestDDNSRecord
# ---------------------------------------------------------------------------

class TestDDNSRecord:
    def test_to_dict_has_all_keys(self):
        record = DDNSRecord(
            id="abc123",
            name="Home",
            provider="cloudflare",
            credentials={"api_token": "tok"},
            hostnames=["home.example.com"],
        )
        d = record.to_dict()
        for key in ("id", "name", "provider", "credentials", "hostnames",
                    "ipv4", "ipv6", "last_ip", "last_ipv6", "last_updated",
                    "last_error", "enabled"):
            assert key in d

    def test_roundtrip(self):
        record = DDNSRecord(
            id="abc123",
            name="Home",
            provider="duckdns",
            credentials={"token": "mytoken"},
            hostnames=["myhome.duckdns.org"],
            ipv4=True,
            ipv6=False,
            last_ip="1.2.3.4",
            last_updated=1700000000.0,
            enabled=True,
        )
        restored = DDNSRecord.from_dict(record.to_dict())
        assert restored.id == record.id
        assert restored.name == record.name
        assert restored.provider == record.provider
        assert restored.credentials == record.credentials
        assert restored.hostnames == record.hostnames
        assert restored.last_ip == "1.2.3.4"
        assert restored.last_updated == 1700000000.0

    def test_defaults(self):
        record = DDNSRecord.from_dict({
            "id": "x",
            "name": "X",
            "provider": "noip",
            "credentials": {},
            "hostnames": ["x.ddns.net"],
        })
        assert record.ipv4 is True
        assert record.ipv6 is False
        assert record.last_ip is None
        assert record.last_ipv6 is None
        assert record.last_updated is None
        assert record.last_error is None
        assert record.enabled is True

    def test_enabled_false_preserved(self):
        record = DDNSRecord.from_dict({
            "id": "y",
            "name": "Y",
            "provider": "noip",
            "credentials": {},
            "hostnames": [],
            "enabled": False,
        })
        assert record.enabled is False


# ---------------------------------------------------------------------------
# TestDDNSConfig
# ---------------------------------------------------------------------------

class TestDDNSConfig:
    def test_defaults(self):
        cfg = DDNSConfig()
        assert cfg.enabled is False
        assert cfg.check_interval_seconds == 300
        assert isinstance(cfg.ip_providers, list)
        assert len(cfg.ip_providers) > 0
        assert isinstance(cfg.ipv6_providers, list)

    def test_ip_providers_list_has_items(self):
        cfg = DDNSConfig()
        assert "ipify" in cfg.ip_providers[0] or "icanhazip" in cfg.ip_providers[0] or "my-ip" in cfg.ip_providers[0]

    def test_roundtrip(self):
        cfg = DDNSConfig(
            enabled=True,
            check_interval_seconds=600,
            ip_providers=["https://api.ipify.org"],
            ipv6_providers=["https://api6.ipify.org"],
        )
        restored = DDNSConfig.from_dict(cfg.to_dict())
        assert restored.enabled is True
        assert restored.check_interval_seconds == 600
        assert restored.ip_providers == ["https://api.ipify.org"]
        assert restored.ipv6_providers == ["https://api6.ipify.org"]

    def test_from_dict_defaults_for_missing(self):
        cfg = DDNSConfig.from_dict({})
        assert cfg.enabled is False
        assert cfg.check_interval_seconds == 300


# ---------------------------------------------------------------------------
# TestDDNSManagerCRUD
# ---------------------------------------------------------------------------

class TestDDNSManagerCRUD:
    def test_add_record_returns_ddns_record(self, tmp_path):
        mgr = _manager(tmp_path)
        rec = mgr.add_record(**_sample_record())
        assert isinstance(rec, DDNSRecord)
        assert rec.name == "Home DDNS"
        assert rec.provider == "cloudflare"

    def test_add_record_generates_id(self, tmp_path):
        mgr = _manager(tmp_path)
        rec = mgr.add_record(**_sample_record())
        assert len(rec.id) > 0

    def test_update_record_changes_field(self, tmp_path):
        mgr = _manager(tmp_path)
        rec = mgr.add_record(**_sample_record())
        updated = mgr.update_record(rec.id, enabled=False)
        assert updated is not None
        assert updated.enabled is False

    def test_update_record_missing_returns_none(self, tmp_path):
        mgr = _manager(tmp_path)
        result = mgr.update_record("nonexistent", enabled=False)
        assert result is None

    def test_remove_record_returns_true(self, tmp_path):
        mgr = _manager(tmp_path)
        rec = mgr.add_record(**_sample_record())
        assert mgr.remove_record(rec.id) is True
        assert mgr.get_record(rec.id) is None

    def test_remove_record_missing_returns_false(self, tmp_path):
        mgr = _manager(tmp_path)
        assert mgr.remove_record("ghost") is False

    def test_list_records_returns_list_of_dicts(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.add_record(**_sample_record(name="R1", hostnames=["r1.example.com"]))
        mgr.add_record(**_sample_record(name="R2", hostnames=["r2.example.com"]))
        records = mgr.list_records()
        assert isinstance(records, list)
        assert len(records) == 2
        for r in records:
            assert isinstance(r, dict)
            assert "id" in r

    def test_get_record_by_id(self, tmp_path):
        mgr = _manager(tmp_path)
        rec = mgr.add_record(**_sample_record())
        found = mgr.get_record(rec.id)
        assert found is not None
        assert found.name == "Home DDNS"


# ---------------------------------------------------------------------------
# TestIPDetection
# ---------------------------------------------------------------------------

def _make_aiohttp_module_mock(status: int, body: str):
    """
    Build a mock aiohttp module.

    _get_current_ip does `import aiohttp` inside the function body, so we
    patch sys.modules["aiohttp"] rather than an attribute on the ddns module.
    """
    import aiohttp as _real_aiohttp

    mock_resp = AsyncMock()
    mock_resp.status = status
    mock_resp.text = AsyncMock(return_value=body)
    mock_resp.__aenter__ = AsyncMock(return_value=mock_resp)
    mock_resp.__aexit__ = AsyncMock(return_value=False)

    mock_get_cm = MagicMock()
    mock_get_cm.__aenter__ = AsyncMock(return_value=mock_resp)
    mock_get_cm.__aexit__ = AsyncMock(return_value=False)

    mock_session = MagicMock()
    mock_session.get = MagicMock(return_value=mock_get_cm)
    mock_session.__aenter__ = AsyncMock(return_value=mock_session)
    mock_session.__aexit__ = AsyncMock(return_value=False)

    mock_aiohttp = MagicMock()
    mock_aiohttp.ClientSession = MagicMock(return_value=mock_session)
    mock_aiohttp.ClientTimeout = _real_aiohttp.ClientTimeout
    return mock_aiohttp


class TestIPDetection:
    @pytest.mark.asyncio
    async def test_get_current_ip_returns_valid_ipv4(self, tmp_path):
        import sys
        mgr = _manager(tmp_path)
        mock_aiohttp = _make_aiohttp_module_mock(200, "1.2.3.4\n")
        with patch.dict(sys.modules, {"aiohttp": mock_aiohttp}):
            result = await mgr._get_current_ip()
        assert result == "1.2.3.4"

    @pytest.mark.asyncio
    async def test_get_current_ip_returns_none_on_failure(self, tmp_path):
        import sys
        mgr = _manager(tmp_path)
        # All providers return non-IPv4 body → should return None
        mock_aiohttp = _make_aiohttp_module_mock(200, "error-response\n")
        with patch.dict(sys.modules, {"aiohttp": mock_aiohttp}):
            result = await mgr._get_current_ip()
        assert result is None

    @pytest.mark.asyncio
    async def test_get_current_ip_rejects_non_ipv4(self, tmp_path):
        import sys
        mgr = _manager(tmp_path)
        mock_aiohttp = _make_aiohttp_module_mock(200, "not-an-ip\n")
        with patch.dict(sys.modules, {"aiohttp": mock_aiohttp}):
            result = await mgr._get_current_ip()
        assert result is None


# ---------------------------------------------------------------------------
# TestUpdateRecord
# ---------------------------------------------------------------------------

class TestUpdateRecord:
    @pytest.mark.asyncio
    async def test_skip_when_ip_unchanged_in_do_all_updates(self, tmp_path):
        mgr = _manager(tmp_path)
        rec = mgr.add_record(**_sample_record())
        rec.last_ip = "5.5.5.5"  # same as what _get_current_ip will return

        update_mock = AsyncMock(return_value=True)
        with patch.object(mgr, "_update_cloudflare", update_mock):
            with patch.object(mgr, "_get_current_ip", AsyncMock(return_value="5.5.5.5")):
                await mgr._do_all_updates()

        # IP unchanged → provider should NOT have been called
        update_mock.assert_not_called()

    @pytest.mark.asyncio
    async def test_update_record_updates_last_ip_on_success(self, tmp_path):
        mgr = _manager(tmp_path)
        rec = mgr.add_record(**_sample_record())
        rec.last_ip = "1.1.1.1"

        with patch.object(mgr, "_update_cloudflare", AsyncMock(return_value=True)):
            ok = await mgr._update_record(rec, "2.2.2.2", None)

        assert ok is True
        assert rec.last_ip == "2.2.2.2"

    @pytest.mark.asyncio
    async def test_update_record_does_not_update_last_ip_on_failure(self, tmp_path):
        mgr = _manager(tmp_path)
        rec = mgr.add_record(**_sample_record())
        rec.last_ip = "1.1.1.1"

        with patch.object(mgr, "_update_cloudflare", AsyncMock(return_value=False)):
            ok = await mgr._update_record(rec, "2.2.2.2", None)

        assert ok is False
        assert rec.last_ip == "1.1.1.1"

    @pytest.mark.asyncio
    async def test_update_record_unknown_provider_returns_false(self, tmp_path):
        mgr = _manager(tmp_path)
        rec = mgr.add_record(
            name="Bad",
            provider="unknown_provider",
            credentials={},
            hostnames=["bad.example.com"],
        )
        ok = await mgr._update_record(rec, "1.2.3.4", None)
        assert ok is False
        assert rec.last_error is not None


# ---------------------------------------------------------------------------
# TestStatus
# ---------------------------------------------------------------------------

class TestStatus:
    def test_get_status_has_expected_fields(self, tmp_path):
        mgr = _manager(tmp_path)
        status = mgr.get_status()
        for key in ("enabled", "records_total", "records_enabled",
                    "current_ipv4", "current_ipv6", "records"):
            assert key in status

    def test_records_total_reflects_count(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.add_record(**_sample_record(name="R1", hostnames=["r1.example.com"]))
        mgr.add_record(**_sample_record(name="R2", hostnames=["r2.example.com"]))
        assert mgr.get_status()["records_total"] == 2

    def test_current_ipv4_initially_none(self, tmp_path):
        mgr = _manager(tmp_path)
        assert mgr.get_status()["current_ipv4"] is None

    def test_records_enabled_count(self, tmp_path):
        mgr = _manager(tmp_path)
        r1 = mgr.add_record(**_sample_record(name="On", hostnames=["on.example.com"]))
        r2 = mgr.add_record(**_sample_record(name="Off", hostnames=["off.example.com"]))
        mgr.update_record(r2.id, enabled=False)
        status = mgr.get_status()
        assert status["records_enabled"] == 1


# ---------------------------------------------------------------------------
# TestPersistence
# ---------------------------------------------------------------------------

class TestPersistence:
    def test_records_survive_reload(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.add_record(**_sample_record(name="CF", hostnames=["cf.example.com"]))
        mgr.add_record(
            name="Duck",
            provider="duckdns",
            credentials={"token": "tok"},
            hostnames=["duck.duckdns.org"],
        )

        mgr2 = _manager(tmp_path)
        records = mgr2.list_records()
        assert len(records) == 2
        names = {r["name"] for r in records}
        assert "CF" in names
        assert "Duck" in names

    def test_config_survives_reload(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr._config.enabled = True
        mgr._config.check_interval_seconds = 120
        mgr._save()

        mgr2 = _manager(tmp_path)
        assert mgr2.get_config().enabled is True
        assert mgr2.get_config().check_interval_seconds == 120

    def test_state_file_mode_600(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.add_record(**_sample_record())
        state_file = tmp_path / "ddns_state.json"
        assert state_file.exists()
        mode = oct(state_file.stat().st_mode)[-3:]
        assert mode == "600"


# ---------------------------------------------------------------------------
# TestLifecycle
# ---------------------------------------------------------------------------

class TestLifecycle:
    @pytest.mark.asyncio
    async def test_start_creates_task_when_enabled(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr._config.enabled = True
        with patch.object(mgr, "_do_all_updates", AsyncMock()):
            await mgr.start()
            assert mgr._task is not None
            assert not mgr._task.done()
            await mgr.stop()

    @pytest.mark.asyncio
    async def test_start_does_not_create_task_when_disabled(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr._config.enabled = False
        await mgr.start()
        assert mgr._task is None

    @pytest.mark.asyncio
    async def test_stop_cancels_task(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr._config.enabled = True
        with patch.object(mgr, "_do_all_updates", AsyncMock()):
            await mgr.start()
            task = mgr._task
            await mgr.stop()
        assert task.done()
        assert mgr._task is None

    @pytest.mark.asyncio
    async def test_double_start_does_not_create_second_task(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr._config.enabled = True
        with patch.object(mgr, "_do_all_updates", AsyncMock()):
            await mgr.start()
            task1 = mgr._task
            await mgr.start()  # second start should be a no-op
            assert mgr._task is task1
            await mgr.stop()
