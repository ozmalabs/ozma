# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Unit tests for doorbell.py — session lifecycle, debounce, expiry."""
import asyncio
import sys
import time
from pathlib import Path
from unittest.mock import MagicMock

import pytest

sys.path.insert(0, str(Path(__file__).parent.parent.parent.parent / "controller"))
pytestmark = pytest.mark.unit


@pytest.fixture
def state():
    s = MagicMock()
    s.active_node_id = "node-a._ozma._udp.local."
    s.events = asyncio.Queue()
    s.nodes = {}
    return s


@pytest.fixture
def alert_mgr(state):
    from alerts import AlertManager
    return AlertManager(state=state)


@pytest.fixture
def mgr(state, alert_mgr):
    from doorbell import DoorbellManager
    return DoorbellManager(
        state=state,
        frigate_url="http://frigate:5000",
        alert_mgr=alert_mgr,
    )


async def drain_events(state, max_events: int = 20) -> list[dict]:
    """Drain all immediately-available events from the state queue."""
    events = []
    for _ in range(max_events):
        try:
            events.append(state.events.get_nowait())
        except asyncio.QueueEmpty:
            break
    return events


async def first_event_of_type(state, evt_type: str, max_events: int = 20) -> dict | None:
    events = await drain_events(state, max_events)
    return next((e for e in events if e["type"] == evt_type), None)


class TestSessionLifecycle:
    @pytest.mark.asyncio
    async def test_receive_event_creates_session(self, mgr):
        session = await mgr.receive_event("front_door", "doorbell", {})
        assert session is not None
        assert session.camera == "front_door"
        assert session.state == "active"

    @pytest.mark.asyncio
    async def test_receive_event_fires_ringing_event(self, mgr, state):
        await mgr.receive_event("front_door", "doorbell", {})
        evt = await first_event_of_type(state, "doorbell.ringing")
        assert evt is not None
        assert evt["camera"] == "front_door"

    @pytest.mark.asyncio
    async def test_session_captures_active_node(self, mgr, state):
        session = await mgr.receive_event("front_door", "doorbell", {})
        assert session.node_id == state.active_node_id

    @pytest.mark.asyncio
    async def test_answer_changes_state(self, mgr, alert_mgr):
        session = await mgr.receive_event("front_door", "doorbell", {})
        result = await alert_mgr.acknowledge(session.id)
        assert result is True
        assert mgr.get_session(session.id).state == "acknowledged"

    @pytest.mark.asyncio
    async def test_answer_fires_answered_event(self, mgr, alert_mgr, state):
        session = await mgr.receive_event("front_door", "doorbell", {})
        await drain_events(state)  # consume create/ringing events
        await alert_mgr.acknowledge(session.id)
        evt = await first_event_of_type(state, "doorbell.answered")
        assert evt is not None

    @pytest.mark.asyncio
    async def test_dismiss_changes_state(self, mgr, alert_mgr):
        session = await mgr.receive_event("front_door", "doorbell", {})
        result = await alert_mgr.dismiss(session.id)
        assert result is True
        assert mgr.get_session(session.id).state == "dismissed"

    @pytest.mark.asyncio
    async def test_dismiss_fires_dismissed_event(self, mgr, alert_mgr, state):
        session = await mgr.receive_event("front_door", "doorbell", {})
        await drain_events(state)
        await alert_mgr.dismiss(session.id)
        evt = await first_event_of_type(state, "doorbell.dismissed")
        assert evt is not None

    @pytest.mark.asyncio
    async def test_answer_unknown_session_returns_false(self, alert_mgr):
        assert not await alert_mgr.acknowledge("no-such-id")

    @pytest.mark.asyncio
    async def test_dismiss_unknown_session_returns_false(self, alert_mgr):
        assert not await alert_mgr.dismiss("no-such-id")

    @pytest.mark.asyncio
    async def test_answer_already_answered_returns_false(self, mgr, alert_mgr):
        session = await mgr.receive_event("front_door", "doorbell", {})
        await alert_mgr.acknowledge(session.id)
        assert not await alert_mgr.acknowledge(session.id)

    @pytest.mark.asyncio
    async def test_dismiss_already_dismissed_returns_false(self, mgr, alert_mgr):
        session = await mgr.receive_event("front_door", "doorbell", {})
        await alert_mgr.dismiss(session.id)
        assert not await alert_mgr.dismiss(session.id)


class TestDebounce:
    @pytest.mark.asyncio
    async def test_rapid_events_deduplicated(self, mgr):
        s1 = await mgr.receive_event("front_door", "doorbell", {})
        s2 = await mgr.receive_event("front_door", "doorbell", {})
        assert s1 is not None
        assert s2 is None  # debounced

    @pytest.mark.asyncio
    async def test_different_cameras_not_deduplicated(self, mgr):
        s1 = await mgr.receive_event("front_door", "doorbell", {})
        s2 = await mgr.receive_event("back_door", "doorbell", {})
        assert s1 is not None
        assert s2 is not None

    @pytest.mark.asyncio
    async def test_person_event_also_debounced(self, mgr):
        # Two events on same camera within debounce window (different kinds, same camera)
        # person uses debounce_s=30, so a second person event is debounced
        s1 = await mgr.receive_person_detected("front_door")
        s2 = await mgr.receive_person_detected("front_door")
        assert s1 is not None
        assert s2 is None


class TestSessionLookup:
    @pytest.mark.asyncio
    async def test_get_session_by_id(self, mgr):
        session = await mgr.receive_event("front_door", "doorbell", {})
        fetched = mgr.get_session(session.id)
        assert fetched is not None
        assert fetched.id == session.id

    @pytest.mark.asyncio
    async def test_get_sessions_returns_list(self, mgr):
        await mgr.receive_event("front_door", "doorbell", {})
        sessions = mgr.get_sessions()
        assert isinstance(sessions, list)
        assert len(sessions) == 1

    @pytest.mark.asyncio
    async def test_get_snapshot_url(self, mgr):
        session = await mgr.receive_event("front_door", "doorbell", {})
        url = mgr.get_snapshot_url(session.id)
        assert url is not None
        assert "front_door" in url or session.id in url

    def test_get_snapshot_url_unknown_returns_none(self, mgr):
        assert mgr.get_snapshot_url("no-such-id") is None

    def test_get_session_unknown_returns_none(self, mgr):
        assert mgr.get_session("no-such-id") is None


class TestExpiry:
    @pytest.mark.asyncio
    async def test_expire_sweep_fires_expired_event(self, mgr, alert_mgr, state):
        session = await mgr.receive_event("front_door", "doorbell", {})
        await drain_events(state)  # consume create/ringing events

        # Manually back-date the session past the timeout
        session.started_at = time.time() - session.timeout_s - 1

        # Run one cycle of the expire sweep
        await alert_mgr._expire_sweep()
        assert mgr.get_session(session.id).state == "expired"

        evt = await first_event_of_type(state, "doorbell.expired")
        assert evt is not None


class TestAcknowledgeCallback:
    @pytest.mark.asyncio
    async def test_callback_called_on_acknowledge(self, mgr, alert_mgr):
        """register_acknowledge_callback fires when the alert is acknowledged."""
        called_with: list[str] = []

        async def cb(alert_id: str) -> None:
            called_with.append(alert_id)

        session = await mgr.receive_event("front_door", "doorbell", {})
        alert_mgr.register_acknowledge_callback(session.id, cb)
        await alert_mgr.acknowledge(session.id)

        # Task runs in the event loop — give it a cycle
        await asyncio.sleep(0)
        assert called_with == [session.id]

    @pytest.mark.asyncio
    async def test_callback_not_called_on_dismiss(self, mgr, alert_mgr):
        """Callback is NOT fired when the alert is dismissed."""
        called_with: list[str] = []

        async def cb(alert_id: str) -> None:
            called_with.append(alert_id)

        session = await mgr.receive_event("front_door", "doorbell", {})
        alert_mgr.register_acknowledge_callback(session.id, cb)
        await alert_mgr.dismiss(session.id)

        await asyncio.sleep(0)
        assert called_with == []

    @pytest.mark.asyncio
    async def test_callback_consumed_after_first_ack(self, mgr, alert_mgr):
        """Callback is removed after firing — second ack doesn't re-fire."""
        call_count = [0]

        async def cb(alert_id: str) -> None:
            call_count[0] += 1

        session = await mgr.receive_event("front_door", "doorbell", {})
        alert_mgr.register_acknowledge_callback(session.id, cb)
        await alert_mgr.acknowledge(session.id)
        await asyncio.sleep(0)
        # Second acknowledge should fail (already acknowledged) AND not re-call
        await alert_mgr.acknowledge(session.id)
        await asyncio.sleep(0)
        assert call_count[0] == 1


class TestAudioStartWiring:
    @pytest.mark.asyncio
    async def test_start_audio_stub_logs_without_camera_config(self, mgr, alert_mgr):
        """start_audio() silently skips when no camera config is present."""
        session = await mgr.receive_event("front_door", "doorbell", {})
        # No OZMA_DOORBELL_CAMERAS set — should return without error
        await mgr.start_audio(session.id)
        assert mgr._audio == {}

    @pytest.mark.asyncio
    async def test_start_audio_skips_unknown_alert(self, mgr):
        """start_audio() with unknown alert_id is a no-op."""
        await mgr.start_audio("no-such-id")
        assert mgr._audio == {}
