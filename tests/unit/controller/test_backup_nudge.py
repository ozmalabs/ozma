# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Unit tests for BackupNudgeService + BackupStatusTracker additions."""

import asyncio
import json
import time
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest
import sys

sys.path.insert(0, str(Path(__file__).parents[3] / "controller"))

from backup_status import BackupNudgeService, BackupStatusTracker, NodeBackupReport


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _node(node_id: str, host: str = "10.0.0.1", api_port: int = 7382) -> MagicMock:
    n = MagicMock()
    n.id = node_id
    n.host = host
    n.api_port = api_port
    n.name = f"node-{node_id}"
    return n


def _state(nodes: dict) -> MagicMock:
    s = MagicMock()
    s.nodes = nodes
    return s


def _tracker(tmp_path: Path) -> BackupStatusTracker:
    return BackupStatusTracker(state_path=tmp_path / "fleet.json")


# ---------------------------------------------------------------------------
# BackupNudgeService — agent URL building
# ---------------------------------------------------------------------------

class TestAgentUrl:
    def test_returns_url_for_known_node(self, tmp_path):
        state = _state({"n1": _node("n1", "192.168.1.10", 7382)})
        svc = BackupNudgeService(state=state, tracker=_tracker(tmp_path))
        url = svc._agent_url("n1", "/api/v1/backup/status")
        assert url == "http://192.168.1.10:7382/api/v1/backup/status"

    def test_returns_none_for_unknown_node(self, tmp_path):
        state = _state({})
        svc = BackupNudgeService(state=state, tracker=_tracker(tmp_path))
        assert svc._agent_url("missing", "/path") is None

    def test_returns_none_when_no_api_port(self, tmp_path):
        node = _node("n1")
        node.api_port = None
        state = _state({"n1": node})
        svc = BackupNudgeService(state=state, tracker=_tracker(tmp_path))
        assert svc._agent_url("n1", "/path") is None

    def test_returns_none_when_no_host(self, tmp_path):
        node = _node("n1")
        node.host = None
        state = _state({"n1": node})
        svc = BackupNudgeService(state=state, tracker=_tracker(tmp_path))
        assert svc._agent_url("n1", "/path") is None


# ---------------------------------------------------------------------------
# BackupNudgeService — proxy helpers
# ---------------------------------------------------------------------------

class TestProxyHelpers:
    @pytest.mark.asyncio
    async def test_proxy_get_success(self, tmp_path):
        state = _state({"n1": _node("n1")})
        svc = BackupNudgeService(state=state, tracker=_tracker(tmp_path))

        mock_resp = MagicMock()
        mock_resp.status = 200
        mock_resp.json = AsyncMock(return_value={"health": "green"})

        mock_cm = MagicMock()
        mock_cm.__aenter__ = AsyncMock(return_value=mock_resp)
        mock_cm.__aexit__ = AsyncMock(return_value=False)

        mock_session = MagicMock()
        mock_session.__aenter__ = AsyncMock(return_value=mock_session)
        mock_session.__aexit__ = AsyncMock(return_value=False)
        mock_session.get = MagicMock(return_value=mock_cm)

        import backup_status
        with patch.object(backup_status.aiohttp, "ClientSession", return_value=mock_session):
            result = await svc.proxy_get("n1", "/api/v1/backup/status")
        assert result == {"health": "green"}

    @pytest.mark.asyncio
    async def test_proxy_get_node_not_found(self, tmp_path):
        state = _state({})
        svc = BackupNudgeService(state=state, tracker=_tracker(tmp_path))
        result = await svc.proxy_get("missing", "/api/v1/backup/status")
        assert result is None

    @pytest.mark.asyncio
    async def test_proxy_get_returns_none_on_network_error(self, tmp_path):
        state = _state({"n1": _node("n1")})
        svc = BackupNudgeService(state=state, tracker=_tracker(tmp_path))

        import backup_status
        mock_session = MagicMock()
        mock_session.__aenter__ = AsyncMock(return_value=mock_session)
        mock_session.__aexit__ = AsyncMock(return_value=False)
        mock_session.get = MagicMock(side_effect=Exception("connection refused"))

        with patch.object(backup_status.aiohttp, "ClientSession", return_value=mock_session):
            result = await svc.proxy_get("n1", "/api/v1/backup/status")
        assert result is None

    @pytest.mark.asyncio
    async def test_proxy_post_success(self, tmp_path):
        state = _state({"n1": _node("n1")})
        svc = BackupNudgeService(state=state, tracker=_tracker(tmp_path))

        mock_resp = MagicMock()
        mock_resp.status = 200
        mock_resp.json = AsyncMock(return_value={"ok": True})

        mock_cm = MagicMock()
        mock_cm.__aenter__ = AsyncMock(return_value=mock_resp)
        mock_cm.__aexit__ = AsyncMock(return_value=False)

        mock_session = MagicMock()
        mock_session.__aenter__ = AsyncMock(return_value=mock_session)
        mock_session.__aexit__ = AsyncMock(return_value=False)
        mock_session.post = MagicMock(return_value=mock_cm)

        import backup_status
        with patch.object(backup_status.aiohttp, "ClientSession", return_value=mock_session):
            result = await svc.proxy_post("n1", "/api/v1/backup/run", {"mode": "smart"})
        assert result == {"ok": True}

    @pytest.mark.asyncio
    async def test_proxy_post_returns_none_for_missing_node(self, tmp_path):
        state = _state({})
        svc = BackupNudgeService(state=state, tracker=_tracker(tmp_path))
        result = await svc.proxy_post("missing", "/api/v1/backup/run", {})
        assert result is None


# ---------------------------------------------------------------------------
# BackupNudgeService — nudge logic
# ---------------------------------------------------------------------------

class TestNudgeLogic:
    @pytest.mark.asyncio
    async def test_fires_event_for_unconfigured_node(self, tmp_path):
        state = _state({"n1": _node("n1")})
        events: list[dict] = []

        async def fake_queue_put(event):
            events.append(event)

        queue = AsyncMock()
        queue.put = fake_queue_put

        svc = BackupNudgeService(state=state, tracker=_tracker(tmp_path),
                                  event_queue=queue)
        await svc._check_unconfigured()
        assert any(e["type"] == "backup.not_configured" and e["node_id"] == "n1"
                   for e in events)

    @pytest.mark.asyncio
    async def test_no_event_for_configured_node(self, tmp_path):
        tracker = _tracker(tmp_path)
        tracker.ingest("n1", {
            "health": "green", "enabled": True,
            "last_success_at": time.time() - 3600,
            "consecutive_failures": 0,
            "total_size_bytes": 1024,
        })
        state = _state({"n1": _node("n1")})
        events: list[dict] = []
        queue = AsyncMock()
        queue.put = AsyncMock(side_effect=events.append)

        svc = BackupNudgeService(state=state, tracker=tracker, event_queue=queue)
        await svc._check_unconfigured()
        assert not any(e.get("type") == "backup.not_configured" for e in events)

    @pytest.mark.asyncio
    async def test_cooldown_prevents_repeat_nudge(self, tmp_path):
        state = _state({"n1": _node("n1")})
        events: list[dict] = []
        queue = AsyncMock()
        queue.put = AsyncMock(side_effect=lambda e: events.append(e))

        svc = BackupNudgeService(state=state, tracker=_tracker(tmp_path),
                                  event_queue=queue)
        await svc._check_unconfigured()
        first_count = len(events)
        await svc._check_unconfigured()
        assert len(events) == first_count  # cooldown: no second nudge

    @pytest.mark.asyncio
    async def test_nudge_fires_after_cooldown_expires(self, tmp_path):
        state = _state({"n1": _node("n1")})
        events: list[dict] = []
        queue = AsyncMock()
        queue.put = AsyncMock(side_effect=lambda e: events.append(e))

        svc = BackupNudgeService(state=state, tracker=_tracker(tmp_path),
                                  event_queue=queue)
        # Set last nudge in the past beyond cooldown
        svc._last_nudge["n1"] = time.time() - svc._NUDGE_COOLDOWN - 1
        await svc._check_unconfigured()
        assert any(e["type"] == "backup.not_configured" for e in events)

    @pytest.mark.asyncio
    async def test_no_event_when_queue_is_none(self, tmp_path):
        state = _state({"n1": _node("n1")})
        svc = BackupNudgeService(state=state, tracker=_tracker(tmp_path),
                                  event_queue=None)
        # Should not raise
        await svc._check_unconfigured()

    @pytest.mark.asyncio
    async def test_no_event_for_yellow_or_worse(self, tmp_path):
        """Nodes with yellow/orange/red are configured — no nudge."""
        for health in ("yellow", "orange", "red"):
            tracker = _tracker(tmp_path)
            tracker.ingest("n1", {
                "health": health, "enabled": True,
                "last_success_at": time.time() - 7 * 86400,
                "consecutive_failures": 1,
                "total_size_bytes": 0,
            })
            state = _state({"n1": _node("n1")})
            events: list[dict] = []
            queue = AsyncMock()
            queue.put = AsyncMock(side_effect=lambda e: events.append(e))
            svc = BackupNudgeService(state=state, tracker=tracker, event_queue=queue)
            await svc._check_unconfigured()
            assert not any(e.get("type") == "backup.not_configured" for e in events), \
                f"Unexpected nudge for health={health}"


# ---------------------------------------------------------------------------
# BackupNudgeService — lifecycle
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
class TestNudgeLifecycle:
    async def test_start_stop(self, tmp_path):
        state = _state({})
        svc = BackupNudgeService(state=state, tracker=_tracker(tmp_path))
        await svc.start()
        assert svc._task is not None
        await svc.stop()
        assert svc._task.cancelled() or svc._task.done()
