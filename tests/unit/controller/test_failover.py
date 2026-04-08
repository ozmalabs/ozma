# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Unit tests for FailoverManager, FailoverConfig, FailoverStatus."""

import asyncio
import json
import sys
import time
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

sys.path.insert(0, str(Path(__file__).parents[3] / "controller"))

from failover import (
    FailoverConfig,
    FailoverManager,
    FailoverMode,
    FailoverStatus,
    FREE_DAYS_BY_TIER,
)


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _mock_connect(authenticated=True, tier="business") -> MagicMock:
    c = MagicMock()
    c.authenticated = authenticated
    c.tier = tier
    c.send_failover_heartbeat = AsyncMock(return_value=True)
    c.check_failover_status = AsyncMock(return_value={"state": "normal"})
    c.accept_failover = AsyncMock(return_value={"ok": True, "virtual_url": "https://v.c.ozma.dev"})
    c.decline_failover = AsyncMock(return_value={"ok": True})
    c.extend_failover = AsyncMock(return_value={"ok": True, "paid_until": time.time() + 86400 * 7, "checkout_url": "https://pay.ozma.dev/..."})
    c.push_sync_delta = AsyncMock(return_value=True)
    c.pull_sync_delta = AsyncMock(return_value={"scenarios": [], "exported_at": time.time()})
    c.signal_handoff_complete = AsyncMock(return_value=True)
    return c


def _mock_app_state() -> MagicMock:
    s = MagicMock()
    s.events = asyncio.Queue()
    s.nodes = {}
    s.active_node_id = None
    return s


def _manager(tmp_path: Path, connect=None, mode: str = "local", **kwargs) -> FailoverManager:
    c = connect or _mock_connect()
    with patch.dict("os.environ", {"OZMA_FAILOVER_MODE": mode}):
        mgr = FailoverManager(
            connect=c,
            state=_mock_app_state(),
            state_path=tmp_path / "failover_state.json",
            **kwargs,
        )
    return mgr


# ---------------------------------------------------------------------------
# TestFreeDaysByTier
# ---------------------------------------------------------------------------

class TestFreeDaysByTier:
    def test_free_tier_gets_zero_days(self):
        assert FREE_DAYS_BY_TIER["free"] == 0

    def test_business_gets_three_days(self):
        assert FREE_DAYS_BY_TIER["business"] == 3

    def test_business_pro_gets_seven_days(self):
        assert FREE_DAYS_BY_TIER["business_pro"] == 7

    def test_enterprise_gets_thirty_days(self):
        assert FREE_DAYS_BY_TIER["enterprise"] == 30


# ---------------------------------------------------------------------------
# TestFailoverConfig
# ---------------------------------------------------------------------------

class TestFailoverConfig:
    def test_defaults(self):
        cfg = FailoverConfig()
        assert cfg.grace_period_minutes == 15
        assert cfg.heartbeat_interval == 60
        assert cfg.poll_interval == 30
        assert cfg.tier == "free"
        assert cfg.free_days == 0

    def test_roundtrip(self):
        cfg = FailoverConfig(free_days=7, tier="business_pro")
        cfg2 = FailoverConfig.from_dict(cfg.to_dict())
        assert cfg2.free_days == 7
        assert cfg2.tier == "business_pro"


# ---------------------------------------------------------------------------
# TestFailoverStatus
# ---------------------------------------------------------------------------

class TestFailoverStatus:
    def test_defaults(self):
        s = FailoverStatus()
        assert s.mode == FailoverMode.LOCAL
        assert s.outage_detected_at is None
        assert s.failover_started_at is None
        assert s.offer_accepted is False

    def test_to_dict_has_expected_keys(self):
        s = FailoverStatus()
        d = s.to_dict()
        for k in ("mode", "outage_detected_at", "failover_started_at",
                  "free_days_remaining", "offer_accepted", "offer_declined",
                  "virtual_url", "backup_label", "backup_timestamp"):
            assert k in d


# ---------------------------------------------------------------------------
# TestFailoverManagerInit
# ---------------------------------------------------------------------------

class TestFailoverManagerInit:
    def test_local_mode_by_default(self, tmp_path):
        mgr = _manager(tmp_path)
        assert mgr.get_mode() == FailoverMode.LOCAL
        assert not mgr.is_virtual()

    def test_virtual_mode_from_env(self, tmp_path):
        mgr = _manager(tmp_path, mode="virtual")
        assert mgr.get_mode() == FailoverMode.VIRTUAL
        assert mgr.is_virtual()

    def test_status_has_expected_keys(self, tmp_path):
        mgr = _manager(tmp_path)
        s = mgr.get_status()
        for k in ("mode", "config", "outage_duration_seconds", "failover_duration_seconds"):
            assert k in s

    def test_no_start_when_not_authenticated(self, tmp_path):
        c = _mock_connect(authenticated=False)
        mgr = _manager(tmp_path, connect=c)

        async def _run():
            await mgr.start()
            assert mgr._heartbeat_task is None
            assert mgr._poll_task is None

        asyncio.run(_run())


# ---------------------------------------------------------------------------
# TestLifecycle
# ---------------------------------------------------------------------------

class TestLifecycle:
    @pytest.mark.asyncio
    async def test_start_creates_tasks_when_authenticated(self, tmp_path):
        mgr = _manager(tmp_path)
        await mgr.start()
        assert mgr._heartbeat_task is not None
        assert mgr._poll_task is not None
        await mgr.stop()

    @pytest.mark.asyncio
    async def test_stop_cancels_all_tasks(self, tmp_path):
        mgr = _manager(tmp_path)
        await mgr.start()
        hb_task = mgr._heartbeat_task
        poll_task = mgr._poll_task
        await mgr.stop()
        assert hb_task.done()
        assert poll_task.done()
        assert mgr._heartbeat_task is None
        assert mgr._poll_task is None

    @pytest.mark.asyncio
    async def test_config_tier_set_from_connect(self, tmp_path):
        c = _mock_connect(tier="business_pro")
        mgr = _manager(tmp_path, connect=c)
        await mgr.start()
        assert mgr._config.tier == "business_pro"
        assert mgr._config.free_days == 7
        await mgr.stop()

    @pytest.mark.asyncio
    async def test_heartbeat_sent_on_start(self, tmp_path):
        c = _mock_connect()
        mgr = _manager(tmp_path, connect=c)
        await mgr.start()
        await asyncio.sleep(0.01)
        # Heartbeat loop fires immediately then sleeps; at least one call
        await mgr.stop()
        c.send_failover_heartbeat.assert_called()


# ---------------------------------------------------------------------------
# TestOutageDetection
# ---------------------------------------------------------------------------

class TestOutageDetection:
    @pytest.mark.asyncio
    async def test_outage_detected_event_emitted(self, tmp_path):
        c = _mock_connect()
        c.check_failover_status = AsyncMock(return_value={
            "state": "outage_detected",
            "outage_detected_at": time.time() - 600,
        })
        mgr = _manager(tmp_path, connect=c)
        await mgr._check_connect_status()
        assert mgr._status.outage_detected_at is not None

    @pytest.mark.asyncio
    async def test_failover_offer_event_emitted(self, tmp_path):
        c = _mock_connect()
        c.check_failover_status = AsyncMock(return_value={
            "state": "failover_pending",
            "backup_label": "today at 14:32",
            "backup_timestamp": time.time() - 7200,
            "free_days_remaining": 3.0,
        })
        mgr = _manager(tmp_path, connect=c)
        # Simulate prior outage detection
        mgr._status.outage_detected_at = time.time() - 900
        await mgr._check_connect_status()
        # Event should have been emitted
        assert mgr._status.backup_label == "today at 14:32"
        assert mgr._status.free_days_remaining == 3.0
        event = mgr._app_state.events.get_nowait()
        assert event["type"] == "failover.offer_available"
        assert event["backup_label"] == "today at 14:32"

    @pytest.mark.asyncio
    async def test_normal_state_no_events(self, tmp_path):
        c = _mock_connect()
        c.check_failover_status = AsyncMock(return_value={"state": "normal"})
        mgr = _manager(tmp_path, connect=c)
        await mgr._check_connect_status()
        assert mgr._app_state.events.empty()


# ---------------------------------------------------------------------------
# TestUserActions
# ---------------------------------------------------------------------------

class TestUserActions:
    @pytest.mark.asyncio
    async def test_accept_virtual_controller(self, tmp_path):
        c = _mock_connect()
        mgr = _manager(tmp_path, connect=c)
        result = await mgr.accept_virtual_controller()
        assert result["ok"] is True
        assert mgr._status.offer_accepted is True
        assert mgr._status.virtual_url == "https://v.c.ozma.dev"
        assert mgr._status.failover_started_at is not None

    @pytest.mark.asyncio
    async def test_accept_fires_event(self, tmp_path):
        mgr = _manager(tmp_path)
        await mgr.accept_virtual_controller()
        event = mgr._app_state.events.get_nowait()
        assert event["type"] == "failover.accepted"
        assert event["virtual_url"] == "https://v.c.ozma.dev"

    @pytest.mark.asyncio
    async def test_accept_blocked_in_virtual_mode(self, tmp_path):
        mgr = _manager(tmp_path, mode="virtual")
        result = await mgr.accept_virtual_controller()
        assert result["ok"] is False

    @pytest.mark.asyncio
    async def test_decline_virtual_controller(self, tmp_path):
        mgr = _manager(tmp_path)
        result = await mgr.decline_virtual_controller()
        assert result["ok"] is True
        assert mgr._status.offer_declined is True

    @pytest.mark.asyncio
    async def test_decline_fires_event(self, tmp_path):
        mgr = _manager(tmp_path)
        await mgr.decline_virtual_controller()
        event = mgr._app_state.events.get_nowait()
        assert event["type"] == "failover.declined"

    @pytest.mark.asyncio
    async def test_extend_failover(self, tmp_path):
        mgr = _manager(tmp_path)
        result = await mgr.extend_failover(7)
        assert result["ok"] is True
        assert mgr._status.paid_until is not None

    @pytest.mark.asyncio
    async def test_extend_fires_event(self, tmp_path):
        mgr = _manager(tmp_path)
        await mgr.extend_failover(7)
        event = mgr._app_state.events.get_nowait()
        assert event["type"] == "failover.extended"
        assert event["days"] == 7


# ---------------------------------------------------------------------------
# TestVirtualMode
# ---------------------------------------------------------------------------

class TestVirtualMode:
    @pytest.mark.asyncio
    async def test_virtual_mode_emits_active_event_on_start(self, tmp_path):
        c = _mock_connect()
        mgr = _manager(tmp_path, connect=c, mode="virtual")
        await mgr.start()
        await asyncio.sleep(0.01)
        # Find the failover.virtual_active event
        events = []
        while not mgr._app_state.events.empty():
            events.append(mgr._app_state.events.get_nowait())
        assert any(e["type"] == "failover.virtual_active" for e in events)
        await mgr.stop()

    @pytest.mark.asyncio
    async def test_virtual_detects_local_recovery(self, tmp_path):
        c = _mock_connect()
        c.check_failover_status = AsyncMock(return_value={"state": "local_heartbeat_resumed"})
        mgr = _manager(tmp_path, connect=c, mode="virtual")

        with patch.object(mgr, "_do_handoff", AsyncMock()) as mock_handoff:
            await mgr._check_connect_status()
            # Should have fired handoff
            mock_handoff.assert_called_once()

    @pytest.mark.asyncio
    async def test_virtual_no_duplicate_handoff(self, tmp_path):
        c = _mock_connect()
        c.check_failover_status = AsyncMock(return_value={"state": "local_recovered"})
        mgr = _manager(tmp_path, connect=c, mode="virtual")

        call_count = 0

        async def _fake_handoff():
            nonlocal call_count
            call_count += 1
            await asyncio.sleep(5)  # simulate long handoff

        with patch.object(mgr, "_do_handoff", side_effect=_fake_handoff):
            # First status check starts handoff task
            await mgr._check_connect_status()
            first_task = mgr._sync_task
            # Second check should not start another
            await mgr._check_connect_status()
            assert mgr._sync_task is first_task

        if first_task:
            first_task.cancel()
            try:
                await first_task
            except (asyncio.CancelledError, Exception):
                pass


# ---------------------------------------------------------------------------
# TestStateDelta
# ---------------------------------------------------------------------------

class TestStateDelta:
    @pytest.mark.asyncio
    async def test_export_state_delta_has_timestamp(self, tmp_path):
        mgr = _manager(tmp_path)
        delta = await mgr.export_state_delta()
        assert "exported_at" in delta
        assert delta["exported_at"] <= time.time() + 1

    @pytest.mark.asyncio
    async def test_apply_state_delta_fires_event(self, tmp_path):
        mgr = _manager(tmp_path)
        delta = {"exported_at": time.time(), "scenarios": []}
        ok = await mgr.apply_state_delta(delta)
        assert ok is True
        event = mgr._app_state.events.get_nowait()
        assert event["type"] == "failover.state_applied"

    @pytest.mark.asyncio
    async def test_apply_delta_with_no_scenarios_manager(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr._scenarios = None
        delta = {"exported_at": time.time(), "scenarios": [{"id": "s1"}]}
        # Should not raise even without scenarios manager
        ok = await mgr.apply_state_delta(delta)
        assert ok is True


# ---------------------------------------------------------------------------
# TestPersistence
# ---------------------------------------------------------------------------

class TestPersistence:
    def test_status_survives_reload(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr._status.outage_detected_at = 1700000000.0
        mgr._status.failover_started_at = 1700001000.0
        mgr._status.backup_label = "today at 14:32"
        mgr._save()

        mgr2 = _manager(tmp_path)
        assert mgr2._status.outage_detected_at == 1700000000.0
        assert mgr2._status.failover_started_at == 1700001000.0
        assert mgr2._status.backup_label == "today at 14:32"

    def test_config_survives_reload(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr._config.free_days = 7
        mgr._config.tier = "business_pro"
        mgr._save()

        mgr2 = _manager(tmp_path)
        assert mgr2._config.free_days == 7
        assert mgr2._config.tier == "business_pro"

    def test_state_file_mode_600(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr._save()
        state_file = tmp_path / "failover_state.json"
        assert state_file.exists()
        mode = oct(state_file.stat().st_mode)[-3:]
        assert mode == "600"


# ---------------------------------------------------------------------------
# TestConnectIntegration
# ---------------------------------------------------------------------------

class TestConnectIntegration:
    @pytest.mark.asyncio
    async def test_send_failover_heartbeat_called_with_mode(self, tmp_path):
        c = _mock_connect()
        mgr = _manager(tmp_path, connect=c)
        await mgr.start()
        await asyncio.sleep(0.01)
        await mgr.stop()
        calls = c.send_failover_heartbeat.call_args_list
        assert len(calls) >= 1
        # Mode should be "local"
        _, kwargs = calls[0]
        mode_arg = calls[0][1].get("mode") or calls[0][0][0]
        assert mode_arg == "local"

    @pytest.mark.asyncio
    async def test_accept_calls_connect_accept(self, tmp_path):
        c = _mock_connect()
        mgr = _manager(tmp_path, connect=c)
        await mgr.accept_virtual_controller()
        c.accept_failover.assert_called_once()

    @pytest.mark.asyncio
    async def test_decline_calls_connect_decline(self, tmp_path):
        c = _mock_connect()
        mgr = _manager(tmp_path, connect=c)
        await mgr.decline_virtual_controller()
        c.decline_failover.assert_called_once()

    @pytest.mark.asyncio
    async def test_extend_calls_connect_extend(self, tmp_path):
        c = _mock_connect()
        mgr = _manager(tmp_path, connect=c)
        await mgr.extend_failover(14)
        c.extend_failover.assert_called_once_with(14)
