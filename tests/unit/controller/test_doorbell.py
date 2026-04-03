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
    return s


@pytest.fixture
def mgr(state):
    from doorbell import DoorbellManager
    return DoorbellManager(state=state, frigate_url="http://frigate:5000")


class TestSessionLifecycle:
    @pytest.mark.asyncio
    async def test_receive_event_creates_session(self, mgr):
        session = await mgr.receive_event("front_door", "doorbell", {})
        assert session is not None
        assert session.camera == "front_door"
        assert session.state == "ringing"

    @pytest.mark.asyncio
    async def test_receive_event_fires_ringing_event(self, mgr, state):
        await mgr.receive_event("front_door", "doorbell", {})
        evt = state.events.get_nowait()
        assert evt["type"] == "doorbell.ringing"
        assert evt["camera"] == "front_door"

    @pytest.mark.asyncio
    async def test_session_captures_active_node(self, mgr, state):
        session = await mgr.receive_event("front_door", "doorbell", {})
        assert session.active_node_id == state.active_node_id

    @pytest.mark.asyncio
    async def test_answer_changes_state(self, mgr):
        session = await mgr.receive_event("front_door", "doorbell", {})
        result = await mgr.answer(session.id)
        assert result is True
        assert mgr.get_session(session.id).state == "answered"

    @pytest.mark.asyncio
    async def test_answer_fires_answered_event(self, mgr, state):
        session = await mgr.receive_event("front_door", "doorbell", {})
        state.events.get_nowait()  # consume ringing
        await mgr.answer(session.id)
        evt = state.events.get_nowait()
        assert evt["type"] == "doorbell.answered"

    @pytest.mark.asyncio
    async def test_dismiss_changes_state(self, mgr):
        session = await mgr.receive_event("front_door", "doorbell", {})
        result = await mgr.dismiss(session.id)
        assert result is True
        assert mgr.get_session(session.id).state == "dismissed"

    @pytest.mark.asyncio
    async def test_dismiss_fires_dismissed_event(self, mgr, state):
        session = await mgr.receive_event("front_door", "doorbell", {})
        state.events.get_nowait()  # consume ringing
        await mgr.dismiss(session.id)
        evt = state.events.get_nowait()
        assert evt["type"] == "doorbell.dismissed"

    @pytest.mark.asyncio
    async def test_answer_unknown_session_returns_false(self, mgr):
        assert not await mgr.answer("no-such-id")

    @pytest.mark.asyncio
    async def test_dismiss_unknown_session_returns_false(self, mgr):
        assert not await mgr.dismiss("no-such-id")

    @pytest.mark.asyncio
    async def test_answer_already_answered_returns_false(self, mgr):
        session = await mgr.receive_event("front_door", "doorbell", {})
        await mgr.answer(session.id)
        assert not await mgr.answer(session.id)

    @pytest.mark.asyncio
    async def test_dismiss_already_dismissed_returns_false(self, mgr):
        session = await mgr.receive_event("front_door", "doorbell", {})
        await mgr.dismiss(session.id)
        assert not await mgr.dismiss(session.id)


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
        s1 = await mgr.receive_event("front_door", "person", {})
        s2 = await mgr.receive_event("front_door", "doorbell", {})
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
    async def test_expire_loop_fires_expired_event(self, state):
        from doorbell import DoorbellManager, RING_TIMEOUT_S
        mgr = DoorbellManager(state=state, frigate_url="http://frigate:5000")
        session = await mgr.receive_event("front_door", "doorbell", {})
        state.events.get_nowait()  # consume ringing

        # Manually back-date the session past the timeout
        session.started_at = time.time() - RING_TIMEOUT_S - 1

        # Run one cycle of the expire loop
        await mgr._expire_loop_once()
        assert mgr.get_session(session.id).state == "expired"

        evt = state.events.get_nowait()
        assert evt["type"] == "doorbell.expired"
