# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Tests for agent.multiseat.seat_manager — seat orchestration."""

from __future__ import annotations

import asyncio
import json
from unittest.mock import AsyncMock, MagicMock, patch, PropertyMock

import pytest

from agent.multiseat.seat_manager import SeatManager, _StubDisplayBackend, _StubInputBackend, _StubAudioBackend
from agent.multiseat.display_backend import DisplayInfo
from agent.multiseat.input_router import InputGroup
from agent.multiseat.gpu_inventory import GPUInventory, GPUInfo, EncoderInfo
from agent.multiseat.encoder_allocator import EncoderAllocator, EncoderSession, EncoderHints
from agent.multiseat.seat import Seat


# ── Construction ─────────────────────────────────────────────────────────────

class TestSeatManagerCreation:
    def test_default_creation(self):
        sm = SeatManager()
        assert sm.seat_count == 0
        assert sm.seats == []
        assert sm.encoder_allocator is None
        assert sm.game_launcher is None
        assert sm.hotplug is None
        assert sm.virtual_display_manager is None

    def test_custom_params(self):
        sm = SeatManager(
            controller_url="http://localhost:7380",
            base_udp_port=8000,
            base_api_port=9000,
            seat_count=4,
            profile_name="gaming",
            machine_name="test-pc",
        )
        assert sm._controller_url == "http://localhost:7380"
        assert sm._base_udp_port == 8000
        assert sm._base_api_port == 9000
        assert sm._seat_count == 4
        assert sm._profile.name == "gaming"
        assert sm._machine_name == "test-pc"


# ── Port allocation ──────────────────────────────────────────────────────────

class TestPortAllocation:
    def test_sequential_udp_ports(self):
        sm = SeatManager(base_udp_port=7331, base_api_port=7382)
        # Manually create seats to verify port calculation
        for i in range(3):
            seat = Seat(
                name=f"seat-{i}",
                seat_index=i,
                display_index=i,
                udp_port=sm._base_udp_port + i,
                api_port=sm._base_api_port + i,
            )
            sm._seats.append(seat)

        assert sm._seats[0].udp_port == 7331
        assert sm._seats[1].udp_port == 7332
        assert sm._seats[2].udp_port == 7333

    def test_sequential_api_ports(self):
        sm = SeatManager(base_udp_port=7331, base_api_port=7382)
        for i in range(3):
            seat = Seat(
                name=f"seat-{i}",
                seat_index=i,
                display_index=i,
                udp_port=sm._base_udp_port + i,
                api_port=sm._base_api_port + i,
            )
            sm._seats.append(seat)

        assert sm._seats[0].api_port == 7382
        assert sm._seats[1].api_port == 7383
        assert sm._seats[2].api_port == 7384


# ── Display assignment ───────────────────────────────────────────────────────

class TestDisplayAssignment:
    def test_display_set_on_seat(self):
        sm = SeatManager()
        display = DisplayInfo(index=0, name="HDMI-1", width=1920, height=1080)
        seat = Seat(
            name="seat-0", seat_index=0, display_index=0,
            udp_port=7331, api_port=7382,
        )
        seat.display = display
        sm._seats.append(seat)

        assert sm._seats[0].display.name == "HDMI-1"
        assert sm._seats[0].display.width == 1920


# ── Input assignment ─────────────────────────────────────────────────────────

class TestInputAssignment:
    def test_assign_full_groups(self):
        sm = SeatManager()
        sm._input_backend = _StubInputBackend()

        # Two seats
        for i in range(2):
            seat = Seat(name=f"seat-{i}", seat_index=i, display_index=i,
                        udp_port=7331 + i, api_port=7382 + i)
            sm._seats.append(seat)

        # Two full input groups
        sm._input_groups = [
            InputGroup(hub_path="1-1", keyboards=["/dev/input/event0"],
                       mice=["/dev/input/event1"]),
            InputGroup(hub_path="1-2", keyboards=["/dev/input/event2"],
                       mice=["/dev/input/event3"]),
        ]

        sm._assign_inputs()

        assert "/dev/input/event0" in sm._seats[0].input_devices
        assert "/dev/input/event1" in sm._seats[0].input_devices
        assert "/dev/input/event2" in sm._seats[1].input_devices
        assert "/dev/input/event3" in sm._seats[1].input_devices

    def test_assign_gamepad_groups(self):
        sm = SeatManager()
        sm._input_backend = _StubInputBackend()

        seat = Seat(name="seat-0", seat_index=0, display_index=0,
                    udp_port=7331, api_port=7382)
        sm._seats.append(seat)

        sm._input_groups = [
            InputGroup(hub_path="1-1", keyboards=["/dev/input/event0"],
                       mice=["/dev/input/event1"]),
            InputGroup(hub_path="1-2", gamepads=["/dev/input/event4"]),
        ]

        sm._assign_inputs()

        # Full group assigned to seat 0
        assert "/dev/input/event0" in sm._seats[0].input_devices
        # Gamepad also assigned to seat 0
        assert "/dev/input/event4" in sm._seats[0].input_devices

    def test_assign_no_groups(self):
        sm = SeatManager()
        sm._input_backend = _StubInputBackend()
        sm._input_groups = []
        sm._seats = [Seat(name="seat-0", seat_index=0, display_index=0,
                          udp_port=7331, api_port=7382)]
        # Should not raise
        sm._assign_inputs()
        assert sm._seats[0].input_devices == []

    def test_assign_no_seats(self):
        sm = SeatManager()
        sm._input_backend = _StubInputBackend()
        sm._input_groups = [
            InputGroup(hub_path="1-1", keyboards=["kbd"], mice=["mouse"]),
        ]
        # Should not raise
        sm._assign_inputs()


# ── Serialization ────────────────────────────────────────────────────────────

class TestSeatManagerSerialization:
    def test_to_dict_empty(self):
        sm = SeatManager(machine_name="test-box")
        d = sm.to_dict()
        assert d["machine"] == "test-box"
        assert d["seat_count"] == 0
        assert d["seats"] == []
        assert d["displays"] == 0
        assert d["input_groups"] == 0

    def test_to_dict_includes_seats(self):
        sm = SeatManager(machine_name="test-box")
        seat = Seat(name="seat-0", seat_index=0, display_index=0,
                    udp_port=7331, api_port=7382)
        sm._seats.append(seat)

        d = sm.to_dict()
        assert d["seat_count"] == 1
        assert len(d["seats"]) == 1
        assert d["seats"][0]["name"] == "seat-0"

    def test_to_dict_includes_encoders(self):
        sm = SeatManager(machine_name="test-box")
        inv = GPUInventory()
        inv._software_encoders = [
            EncoderInfo(name="libx264", codec="h264", gpu_index=-1,
                        max_sessions=-1, quality=9, latency=8),
        ]
        inv._discovered = True
        sm._encoder_allocator = EncoderAllocator(inv)

        d = sm.to_dict()
        assert "encoders" in d

    def test_to_dict_json_serializable(self):
        sm = SeatManager(machine_name="test-box")
        seat = Seat(name="seat-0", seat_index=0, display_index=0,
                    udp_port=7331, api_port=7382)
        sm._seats.append(seat)
        # Should not raise
        json.dumps(sm.to_dict())


# ── Seat lookup ──────────────────────────────────────────────────────────────

class TestSeatLookup:
    def test_get_seat_found(self):
        sm = SeatManager()
        seat = Seat(name="gaming", seat_index=0, display_index=0,
                    udp_port=7331, api_port=7382)
        sm._seats.append(seat)
        assert sm.get_seat("gaming") is seat

    def test_get_seat_not_found(self):
        sm = SeatManager()
        assert sm.get_seat("missing") is None


# ── Seat config loading ─────────────────────────────────────────────────────

class TestSeatConfigLoading:
    def test_load_config_from_file(self, tmp_path):
        config = [
            {"name": "gaming-seat", "profile": "gaming"},
            {"name": "work-seat", "profile": "workstation"},
        ]
        config_path = tmp_path / "seats.json"
        config_path.write_text(json.dumps(config))

        sm = SeatManager(seat_config_path=str(config_path))
        result = sm._load_seat_config()
        assert result is not None
        assert len(result) == 2
        assert result[0]["name"] == "gaming-seat"

    def test_load_config_missing_file(self, tmp_path):
        sm = SeatManager(seat_config_path=str(tmp_path / "nonexistent.json"))
        result = sm._load_seat_config()
        assert result is None

    def test_load_config_invalid_json(self, tmp_path):
        config_path = tmp_path / "seats.json"
        config_path.write_text("not json")
        sm = SeatManager(seat_config_path=str(config_path))
        result = sm._load_seat_config()
        assert result is None

    def test_load_config_not_list(self, tmp_path):
        config_path = tmp_path / "seats.json"
        config_path.write_text('{"key": "value"}')
        sm = SeatManager(seat_config_path=str(config_path))
        result = sm._load_seat_config()
        assert result is None

    def test_load_config_no_path(self):
        sm = SeatManager()
        result = sm._load_seat_config()
        assert result is None


# ── Stub backends ────────────────────────────────────────────────────────────

class TestStubBackends:
    def test_stub_display_enumerate(self):
        backend = _StubDisplayBackend()
        displays = backend.enumerate()
        assert len(displays) == 1
        assert displays[0].name == "default"
        assert displays[0].width == 1920

    def test_stub_display_create_virtual(self):
        backend = _StubDisplayBackend()
        assert backend.create_virtual() is None

    def test_stub_display_destroy_virtual(self):
        backend = _StubDisplayBackend()
        display = DisplayInfo(index=0, name="test", width=1920, height=1080)
        assert backend.destroy_virtual(display) is False

    def test_stub_input_enumerate(self):
        backend = _StubInputBackend()
        groups = backend.enumerate_groups()
        assert len(groups) == 1
        assert groups[0].hub_path == "default"
        assert len(groups[0].keyboards) == 1

    def test_stub_input_assign(self):
        backend = _StubInputBackend()
        group = InputGroup(hub_path="test")
        assert backend.assign(group, MagicMock()) is True

    def test_stub_input_unassign(self):
        backend = _StubInputBackend()
        group = InputGroup(hub_path="test")
        assert backend.unassign(group) is True

    @pytest.mark.asyncio
    async def test_stub_audio_create_sink(self):
        backend = _StubAudioBackend()
        assert await backend.create_sink("test") is None

    @pytest.mark.asyncio
    async def test_stub_audio_destroy_sink(self):
        backend = _StubAudioBackend()
        assert await backend.destroy_sink("test") is True

    @pytest.mark.asyncio
    async def test_stub_audio_list_sinks(self):
        backend = _StubAudioBackend()
        assert await backend.list_sinks() == []
