# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Unit tests for alerts.py — AlertManager session lifecycle, delivery, expiry."""
import asyncio
import sys
import time
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock

import pytest

sys.path.insert(0, str(Path(__file__).parent.parent.parent.parent / "controller"))
pytestmark = pytest.mark.unit


# ── Fixtures ──────────────────────────────────────────────────────────────────

@pytest.fixture
def state():
    s = MagicMock()
    s.events = asyncio.Queue()
    return s


@pytest.fixture
def mgr(state):
    from alerts import AlertManager
    return AlertManager(state=state)


async def drain(state, n=30) -> list[dict]:
    events = []
    for _ in range(n):
        try:
            events.append(state.events.get_nowait())
        except asyncio.QueueEmpty:
            break
    return events


async def first_of_type(state, typ: str) -> dict | None:
    return next((e for e in await drain(state) if e["type"] == typ), None)


# ── Creation ──────────────────────────────────────────────────────────────────

class TestCreate:
    @pytest.mark.asyncio
    async def test_returns_alert_session(self, mgr):
        from alerts import AlertSession
        alert = await mgr.create("timer", "Pasta", "8 min timer")
        assert isinstance(alert, AlertSession)

    @pytest.mark.asyncio
    async def test_alert_id_is_set(self, mgr):
        alert = await mgr.create("timer", "Pasta", "8 min timer")
        assert alert.id and len(alert.id) == 8

    @pytest.mark.asyncio
    async def test_kind_title_body_stored(self, mgr):
        alert = await mgr.create("doorbell", "Doorbell", "Front door")
        assert alert.kind == "doorbell"
        assert alert.title == "Doorbell"
        assert alert.body == "Front door"

    @pytest.mark.asyncio
    async def test_state_is_active(self, mgr):
        alert = await mgr.create("reminder", "Meeting", "In 5 min")
        assert alert.state == "active"

    @pytest.mark.asyncio
    async def test_fires_alert_created_event(self, mgr, state):
        await mgr.create("timer", "Pasta", "done")
        evt = await first_of_type(state, "alert.created")
        assert evt is not None
        assert evt["kind"] == "timer"

    @pytest.mark.asyncio
    async def test_optional_camera_person_stored(self, mgr):
        alert = await mgr.create(
            "doorbell", "Doorbell", "Front door",
            camera="front_door", person="Matt",
        )
        assert alert.camera == "front_door"
        assert alert.person == "Matt"

    @pytest.mark.asyncio
    async def test_to_dict_includes_person(self, mgr):
        alert = await mgr.create("doorbell", "Doorbell", "x", person="Matt")
        d = alert.to_dict()
        assert d["person"] == "Matt"

    @pytest.mark.asyncio
    async def test_to_dict_omits_empty_person(self, mgr):
        alert = await mgr.create("timer", "Pasta", "x")
        assert "person" not in alert.to_dict()

    @pytest.mark.asyncio
    async def test_snapshot_url_proxied_in_to_dict(self, mgr):
        alert = await mgr.create("doorbell", "D", "x", snapshot_url="http://cam/snap.jpg")
        d = alert.to_dict()
        assert "/api/v1/alerts/" in d["snapshot_url"]
        assert alert.id in d["snapshot_url"]

    @pytest.mark.asyncio
    async def test_no_snapshot_url_omitted_from_dict(self, mgr):
        alert = await mgr.create("timer", "Pasta", "x")
        assert "snapshot_url" not in alert.to_dict()


# ── Acknowledge / Dismiss ─────────────────────────────────────────────────────

class TestAcknowledge:
    @pytest.mark.asyncio
    async def test_acknowledge_returns_true(self, mgr):
        alert = await mgr.create("timer", "T", "x")
        assert await mgr.acknowledge(alert.id) is True

    @pytest.mark.asyncio
    async def test_state_becomes_acknowledged(self, mgr):
        alert = await mgr.create("timer", "T", "x")
        await mgr.acknowledge(alert.id)
        assert alert.state == "acknowledged"

    @pytest.mark.asyncio
    async def test_fires_acknowledged_event(self, mgr, state):
        alert = await mgr.create("timer", "T", "x")
        await drain(state)
        await mgr.acknowledge(alert.id)
        evt = await first_of_type(state, "alert.acknowledged")
        assert evt is not None
        assert evt["id"] == alert.id

    @pytest.mark.asyncio
    async def test_double_acknowledge_returns_false(self, mgr):
        alert = await mgr.create("timer", "T", "x")
        await mgr.acknowledge(alert.id)
        assert await mgr.acknowledge(alert.id) is False

    @pytest.mark.asyncio
    async def test_acknowledge_unknown_id_returns_false(self, mgr):
        assert await mgr.acknowledge("no-such-id") is False

    @pytest.mark.asyncio
    async def test_empty_id_resolves_most_recent_active(self, mgr):
        alert = await mgr.create("timer", "T", "x")
        result = await mgr.acknowledge("")   # empty → most recent active
        assert result is True
        assert alert.state == "acknowledged"


class TestDismiss:
    @pytest.mark.asyncio
    async def test_dismiss_returns_true(self, mgr):
        alert = await mgr.create("timer", "T", "x")
        assert await mgr.dismiss(alert.id) is True

    @pytest.mark.asyncio
    async def test_state_becomes_dismissed(self, mgr):
        alert = await mgr.create("timer", "T", "x")
        await mgr.dismiss(alert.id)
        assert alert.state == "dismissed"

    @pytest.mark.asyncio
    async def test_fires_dismissed_event(self, mgr, state):
        alert = await mgr.create("timer", "T", "x")
        await drain(state)
        await mgr.dismiss(alert.id)
        evt = await first_of_type(state, "alert.dismissed")
        assert evt is not None

    @pytest.mark.asyncio
    async def test_dismiss_acknowledged_alert_returns_false(self, mgr):
        alert = await mgr.create("timer", "T", "x")
        await mgr.acknowledge(alert.id)
        assert await mgr.dismiss(alert.id) is False


# ── Update (enrichment) ───────────────────────────────────────────────────────

class TestUpdate:
    @pytest.mark.asyncio
    async def test_update_person_on_active_alert(self, mgr):
        alert = await mgr.create("doorbell", "D", "x")
        result = await mgr.update(alert.id, person="Matt")
        assert result is True
        assert alert.person == "Matt"

    @pytest.mark.asyncio
    async def test_update_fires_alert_updated_event(self, mgr, state):
        alert = await mgr.create("doorbell", "D", "x")
        await drain(state)
        await mgr.update(alert.id, person="Matt")
        evt = await first_of_type(state, "alert.updated")
        assert evt is not None
        assert evt.get("person") == "Matt"

    @pytest.mark.asyncio
    async def test_update_dismissed_alert_returns_false(self, mgr):
        alert = await mgr.create("doorbell", "D", "x")
        await mgr.dismiss(alert.id)
        assert await mgr.update(alert.id, person="Matt") is False

    @pytest.mark.asyncio
    async def test_update_unknown_alert_returns_false(self, mgr):
        assert await mgr.update("no-such-id", person="Matt") is False


# ── List / query ──────────────────────────────────────────────────────────────

class TestListAlerts:
    @pytest.mark.asyncio
    async def test_list_returns_all(self, mgr):
        await mgr.create("timer", "T1", "x")
        await mgr.create("doorbell", "D1", "x")
        alerts = mgr.list_alerts()
        assert len(alerts) == 2

    @pytest.mark.asyncio
    async def test_filter_by_kind(self, mgr):
        await mgr.create("timer", "T", "x")
        await mgr.create("doorbell", "D", "x")
        timers = mgr.list_alerts(kind="timer")
        assert len(timers) == 1
        assert timers[0]["kind"] == "timer"

    @pytest.mark.asyncio
    async def test_filter_by_state(self, mgr):
        a1 = await mgr.create("timer", "T", "x")
        a2 = await mgr.create("doorbell", "D", "x")
        await mgr.acknowledge(a1.id)
        active = mgr.list_alerts(state="active")
        assert len(active) == 1
        assert active[0]["id"] == a2.id

    @pytest.mark.asyncio
    async def test_get_most_recent_active_returns_last(self, mgr):
        a1 = await mgr.create("timer", "T1", "x")
        a2 = await mgr.create("timer", "T2", "x")
        recent = mgr.get_most_recent_active()
        assert recent.id == a2.id

    @pytest.mark.asyncio
    async def test_get_most_recent_active_filtered_by_kind(self, mgr):
        await mgr.create("timer", "T", "x")
        d = await mgr.create("doorbell", "D", "x")
        recent = mgr.get_most_recent_active(kind="doorbell")
        assert recent.id == d.id

    @pytest.mark.asyncio
    async def test_get_most_recent_active_no_active_returns_none(self, mgr):
        alert = await mgr.create("timer", "T", "x")
        await mgr.acknowledge(alert.id)
        assert mgr.get_most_recent_active() is None


# ── Debounce ──────────────────────────────────────────────────────────────────

class TestDebounce:
    @pytest.mark.asyncio
    async def test_same_kind_camera_within_window_suppressed(self, mgr):
        a1 = await mgr.create("doorbell", "D", "x", camera="front", debounce_key="front", debounce_s=10)
        a2 = await mgr.create("doorbell", "D", "x", camera="front", debounce_key="front", debounce_s=10)
        assert a1 is not None
        assert a2 is None

    @pytest.mark.asyncio
    async def test_different_camera_not_suppressed(self, mgr):
        a1 = await mgr.create("doorbell", "D", "x", camera="front", debounce_key="front", debounce_s=10)
        a2 = await mgr.create("doorbell", "D", "x", camera="back", debounce_key="back", debounce_s=10)
        assert a1 is not None
        assert a2 is not None

    @pytest.mark.asyncio
    async def test_different_kind_same_camera_not_suppressed(self, mgr):
        """Doorbell and motion on the same camera should not debounce each other."""
        a1 = await mgr.create("doorbell", "D", "x", camera="front", debounce_key="front", debounce_s=30)
        a2 = await mgr.create("motion", "M", "x", camera="front", debounce_key="front", debounce_s=30)
        assert a1 is not None
        assert a2 is not None

    @pytest.mark.asyncio
    async def test_no_debounce_key_never_suppressed(self, mgr):
        a1 = await mgr.create("timer", "T", "x")
        a2 = await mgr.create("timer", "T", "x")
        assert a1 is not None
        assert a2 is not None


# ── Expiry ────────────────────────────────────────────────────────────────────

class TestExpiry:
    @pytest.mark.asyncio
    async def test_expired_alert_state_changes(self, mgr, state):
        alert = await mgr.create("doorbell", "D", "x", timeout_s=30)
        await drain(state)
        alert.started_at = time.time() - 31
        await mgr._expire_sweep()
        assert alert.state == "expired"

    @pytest.mark.asyncio
    async def test_expired_event_fired(self, mgr, state):
        alert = await mgr.create("doorbell", "D", "x", timeout_s=30)
        await drain(state)
        alert.started_at = time.time() - 31
        await mgr._expire_sweep()
        evt = await first_of_type(state, "alert.expired")
        assert evt is not None and evt["id"] == alert.id

    @pytest.mark.asyncio
    async def test_zero_timeout_never_expires(self, mgr, state):
        alert = await mgr.create("alarm", "A", "x", timeout_s=0)
        alert.started_at = time.time() - 9999
        await mgr._expire_sweep()
        assert alert.state == "active"

    @pytest.mark.asyncio
    async def test_stale_resolved_alerts_cleaned_up(self, mgr, state):
        from alerts import ALERT_TTL_S
        alert = await mgr.create("timer", "T", "x", timeout_s=1)
        alert.started_at = time.time() - ALERT_TTL_S - 1
        alert.state = "acknowledged"
        await mgr._expire_sweep()
        assert mgr.get_alert(alert.id) is None


# ── Kind-specific event aliases ───────────────────────────────────────────────

class TestKindAliases:
    @pytest.mark.asyncio
    async def test_doorbell_created_fires_ringing_alias(self, mgr, state):
        await mgr.create("doorbell", "D", "x")
        events = await drain(state)
        types = {e["type"] for e in events}
        assert "doorbell.ringing" in types

    @pytest.mark.asyncio
    async def test_doorbell_acknowledged_fires_answered_alias(self, mgr, state):
        alert = await mgr.create("doorbell", "D", "x")
        await drain(state)
        await mgr.acknowledge(alert.id)
        events = await drain(state)
        types = {e["type"] for e in events}
        assert "doorbell.answered" in types

    @pytest.mark.asyncio
    async def test_timer_has_no_doorbell_alias(self, mgr, state):
        alert = await mgr.create("timer", "T", "x")
        await mgr.acknowledge(alert.id)
        events = await drain(state)
        types = {e["type"] for e in events}
        assert "doorbell.answered" not in types


# ── Acknowledge callback ──────────────────────────────────────────────────────

class TestAckCallback:
    @pytest.mark.asyncio
    async def test_callback_fires_on_acknowledge(self, mgr):
        called = []
        async def cb(alert_id): called.append(alert_id)
        alert = await mgr.create("timer", "T", "x")
        mgr.register_acknowledge_callback(alert.id, cb)
        await mgr.acknowledge(alert.id)
        await asyncio.sleep(0)
        assert called == [alert.id]

    @pytest.mark.asyncio
    async def test_callback_not_fired_on_dismiss(self, mgr):
        called = []
        async def cb(alert_id): called.append(alert_id)
        alert = await mgr.create("timer", "T", "x")
        mgr.register_acknowledge_callback(alert.id, cb)
        await mgr.dismiss(alert.id)
        await asyncio.sleep(0)
        assert called == []

    @pytest.mark.asyncio
    async def test_callback_consumed_after_first_call(self, mgr):
        count = [0]
        async def cb(alert_id): count[0] += 1
        alert = await mgr.create("timer", "T", "x")
        mgr.register_acknowledge_callback(alert.id, cb)
        await mgr.acknowledge(alert.id)
        await asyncio.sleep(0)
        # Callback is removed; second ack is a no-op AND should not re-call
        await mgr.acknowledge(alert.id)
        await asyncio.sleep(0)
        assert count[0] == 1


# ── KDE Connect / Notifier delivery ──────────────────────────────────────────

class TestDelivery:
    @pytest.mark.asyncio
    async def test_kdeconnect_ping_called_for_connected_device(self, state):
        from alerts import AlertManager
        kdc = MagicMock()
        device = MagicMock()
        device.connected = True
        device.id = "phone-1"
        kdc._devices = {"phone-1": device}
        kdc.ping = AsyncMock()
        mgr = AlertManager(state=state, kdeconnect=kdc)
        await mgr.create("doorbell", "D", "Someone at door")
        kdc.ping.assert_awaited_once()
        call_args = kdc.ping.call_args
        assert "Someone at door" in str(call_args)

    @pytest.mark.asyncio
    async def test_kdeconnect_skipped_for_disconnected_device(self, state):
        from alerts import AlertManager
        kdc = MagicMock()
        device = MagicMock()
        device.connected = False
        kdc._devices = {"phone-1": device}
        kdc.ping = AsyncMock()
        mgr = AlertManager(state=state, kdeconnect=kdc)
        await mgr.create("doorbell", "D", "x")
        kdc.ping.assert_not_awaited()

    @pytest.mark.asyncio
    async def test_notifier_on_event_called(self, state):
        from alerts import AlertManager
        notifier = MagicMock()
        notifier.on_event = AsyncMock()
        mgr = AlertManager(state=state, notifier=notifier)
        await mgr.create("timer", "Pasta", "done")
        notifier.on_event.assert_awaited_once()
        args = notifier.on_event.call_args[0]
        assert args[0] == "alert.timer"
