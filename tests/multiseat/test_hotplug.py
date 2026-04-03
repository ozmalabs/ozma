# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Tests for agent.multiseat.hotplug — USB hotplug monitoring and seat persistence."""

from __future__ import annotations

import asyncio
import json
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from agent.multiseat.hotplug import (
    HotplugMonitor, SeatPersistence, SeatMapping,
    SETTLE_DELAY_S, REMOVAL_GRACE_S,
)
from agent.multiseat.input_router import InputGroup


# ── SeatMapping data model ───────────────────────────────────────────────────

class TestSeatMapping:
    def test_basic_mapping(self):
        m = SeatMapping(
            hub_path="1-1",
            seat_name="gaming-seat",
            seat_index=0,
            device_signatures=["1234:5678"],
        )
        assert m.hub_path == "1-1"
        assert m.seat_name == "gaming-seat"
        assert m.seat_index == 0

    def test_to_dict(self):
        m = SeatMapping(
            hub_path="1-1", seat_name="seat-0",
            seat_index=0, device_signatures=["1234:5678", "abcd:ef01"],
        )
        d = m.to_dict()
        assert d["hub_path"] == "1-1"
        assert d["seat_name"] == "seat-0"
        assert d["seat_index"] == 0
        assert len(d["device_signatures"]) == 2

    def test_from_dict(self):
        d = {
            "hub_path": "2-3",
            "seat_name": "work-seat",
            "seat_index": 1,
            "device_signatures": ["aaaa:bbbb"],
        }
        m = SeatMapping.from_dict(d)
        assert m.hub_path == "2-3"
        assert m.seat_name == "work-seat"
        assert m.seat_index == 1

    def test_from_dict_defaults(self):
        m = SeatMapping.from_dict({})
        assert m.hub_path == ""
        assert m.seat_name == ""
        assert m.seat_index == 0
        assert m.device_signatures == []


# ── SeatPersistence ──────────────────────────────────────────────────────────

class TestSeatPersistence:
    def test_save_and_load(self, tmp_path):
        path = tmp_path / "seats.json"
        sp = SeatPersistence(path=path)

        sp.set(SeatMapping(hub_path="1-1", seat_name="seat-0", seat_index=0))
        sp.set(SeatMapping(hub_path="1-2", seat_name="seat-1", seat_index=1))
        assert sp.save() is True

        # Load into a new instance
        sp2 = SeatPersistence(path=path)
        mappings = sp2.load()
        assert len(mappings) == 2
        assert mappings["1-1"].seat_name == "seat-0"
        assert mappings["1-2"].seat_name == "seat-1"

    def test_load_nonexistent(self, tmp_path):
        path = tmp_path / "nonexistent.json"
        sp = SeatPersistence(path=path)
        result = sp.load()
        assert result == {}

    def test_load_corrupt_json(self, tmp_path):
        path = tmp_path / "seats.json"
        path.write_text("not valid json {{{")
        sp = SeatPersistence(path=path)
        result = sp.load()
        assert result == {}

    def test_load_not_list(self, tmp_path):
        path = tmp_path / "seats.json"
        path.write_text('{"key": "value"}')
        sp = SeatPersistence(path=path)
        result = sp.load()
        assert result == {}

    def test_get_mapping(self, tmp_path):
        sp = SeatPersistence(path=tmp_path / "seats.json")
        sp.set(SeatMapping(hub_path="1-1", seat_name="seat-0", seat_index=0))
        assert sp.get("1-1").seat_name == "seat-0"
        assert sp.get("nonexistent") is None

    def test_remove_mapping(self, tmp_path):
        sp = SeatPersistence(path=tmp_path / "seats.json")
        sp.set(SeatMapping(hub_path="1-1", seat_name="seat-0", seat_index=0))
        sp.remove("1-1")
        assert sp.get("1-1") is None

    def test_save_creates_parent_dirs(self, tmp_path):
        path = tmp_path / "deep" / "nested" / "seats.json"
        sp = SeatPersistence(path=path)
        sp.set(SeatMapping(hub_path="1-1", seat_name="seat-0", seat_index=0))
        assert sp.save() is True
        assert path.exists()

    def test_mappings_property(self, tmp_path):
        sp = SeatPersistence(path=tmp_path / "seats.json")
        sp.set(SeatMapping(hub_path="1-1", seat_name="seat-0", seat_index=0))
        mappings = sp.mappings
        assert len(mappings) == 1
        # Dict is a shallow copy — adding new keys doesn't affect original
        mappings["new-key"] = SeatMapping(hub_path="new", seat_name="new", seat_index=9)
        assert sp.get("new-key") is None

    def test_load_with_device_signatures(self, tmp_path):
        path = tmp_path / "seats.json"
        data = [
            {
                "hub_path": "1-1",
                "seat_name": "seat-0",
                "seat_index": 0,
                "device_signatures": ["046d:c52b", "046d:c07d"],
            },
        ]
        path.write_text(json.dumps(data))
        sp = SeatPersistence(path=path)
        mappings = sp.load()
        assert mappings["1-1"].device_signatures == ["046d:c52b", "046d:c07d"]


# ── HotplugMonitor ───────────────────────────────────────────────────────────

class TestHotplugMonitor:
    def test_creation(self):
        mock_manager = MagicMock()
        hm = HotplugMonitor(mock_manager)
        assert hm.persistence is not None
        assert len(hm._known_groups) == 0

    def test_to_dict(self):
        mock_manager = MagicMock()
        hm = HotplugMonitor(mock_manager)
        d = hm.to_dict()
        assert d["known_groups"] == 0
        assert d["hub_to_seat"] == {}
        assert d["pending_additions"] == 0
        assert d["pending_removals"] == {}
        assert "persisted_mappings" in d

    @pytest.mark.asyncio
    async def test_stop_saves_persistence(self):
        mock_manager = MagicMock()
        hm = HotplugMonitor(mock_manager)
        hm._persistence = MagicMock()
        hm._persistence.save = MagicMock()

        await hm.stop()
        hm._persistence.save.assert_called_once()

    @pytest.mark.asyncio
    async def test_stop_cancels_pending_removals(self):
        mock_manager = MagicMock()
        hm = HotplugMonitor(mock_manager)

        # Add a pending removal
        from agent.multiseat.hotplug import _PendingRemoval
        pending = _PendingRemoval(seat_name="seat-0", removed_at=0.0)
        hm._pending_removals["1-1"] = pending

        hm._persistence = MagicMock()
        hm._persistence.save = MagicMock()

        await hm.stop()
        assert pending.cancel_event.is_set()
        assert len(hm._pending_removals) == 0


# ── Debounce and grace period constants ──────────────────────────────────────

class TestConstants:
    def test_settle_delay(self):
        assert SETTLE_DELAY_S == 0.5

    def test_removal_grace(self):
        assert REMOVAL_GRACE_S == 5.0


# ── Grace period logic ───────────────────────────────────────────────────────

class TestGracePeriod:
    @pytest.mark.asyncio
    async def test_grace_period_cancellation(self):
        """If cancel_event fires during grace period, seat is not destroyed."""
        mock_manager = MagicMock()
        mock_manager.destroy_hotplug_seat = AsyncMock()

        hm = HotplugMonitor(mock_manager)

        from agent.multiseat.hotplug import _PendingRemoval
        pending = _PendingRemoval(seat_name="seat-0", removed_at=0.0)

        # Cancel immediately
        pending.cancel_event.set()

        await hm._grace_period_destroy("1-1", pending)
        mock_manager.destroy_hotplug_seat.assert_not_called()

    @pytest.mark.asyncio
    async def test_grace_period_expiry(self):
        """If grace period expires, seat is destroyed."""
        mock_manager = MagicMock()
        mock_manager.destroy_hotplug_seat = AsyncMock()

        hm = HotplugMonitor(mock_manager)
        hm._hub_to_seat["1-1"] = "seat-0"
        hm._pending_removals["1-1"] = MagicMock()

        from agent.multiseat.hotplug import _PendingRemoval
        # Very short grace period for testing
        pending = _PendingRemoval(seat_name="seat-0", removed_at=0.0)

        with patch("agent.multiseat.hotplug.REMOVAL_GRACE_S", 0.01):
            await hm._grace_period_destroy("1-1", pending)

        mock_manager.destroy_hotplug_seat.assert_called_once_with("seat-0")


# ── Seat restoration from persistence ────────────────────────────────────────

class TestSeatRestoration:
    @pytest.mark.asyncio
    async def test_restore_persisted_seat(self):
        mock_manager = MagicMock()
        mock_manager.seats = []
        mock_manager._machine_name = "test-pc"
        mock_seat = MagicMock()
        mock_seat.name = "gaming-seat"
        mock_seat.seat_index = 0
        mock_manager.create_hotplug_seat = AsyncMock(return_value=mock_seat)

        hm = HotplugMonitor(mock_manager)
        hm._persistence = MagicMock()
        hm._persistence.get = MagicMock(return_value=SeatMapping(
            hub_path="1-1", seat_name="gaming-seat", seat_index=0,
        ))
        hm._persistence.set = MagicMock()
        hm._persistence.save = MagicMock()

        group = InputGroup(
            hub_path="1-1",
            keyboards=["/dev/input/event0"],
            mice=["/dev/input/event1"],
        )

        await hm._on_group_added(group)

        # Should create seat with persisted name
        mock_manager.create_hotplug_seat.assert_called_once_with(
            name="gaming-seat", seat_index=0, input_group=group,
        )

    @pytest.mark.asyncio
    async def test_new_seat_no_persistence(self):
        mock_manager = MagicMock()
        mock_manager.seats = []
        mock_manager._machine_name = "test-pc"
        mock_seat = MagicMock()
        mock_seat.name = "test-pc-seat-0"
        mock_seat.seat_index = 0
        mock_manager.create_hotplug_seat = AsyncMock(return_value=mock_seat)

        hm = HotplugMonitor(mock_manager)
        hm._persistence = MagicMock()
        hm._persistence.get = MagicMock(return_value=None)
        hm._persistence.set = MagicMock()
        hm._persistence.save = MagicMock()

        group = InputGroup(
            hub_path="1-1",
            keyboards=["/dev/input/event0"],
            mice=["/dev/input/event1"],
        )

        await hm._on_group_added(group)

        mock_manager.create_hotplug_seat.assert_called_once()
        call_kwargs = mock_manager.create_hotplug_seat.call_args
        assert call_kwargs.kwargs["name"] == "test-pc-seat-0"
