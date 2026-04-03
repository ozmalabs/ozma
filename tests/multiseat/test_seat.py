# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Tests for agent.multiseat.seat — individual seat lifecycle and API."""

from __future__ import annotations

import asyncio
import json
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from agent.multiseat.seat import Seat
from agent.multiseat.display_backend import DisplayInfo


# ── Construction ─────────────────────────────────────────────────────────────

class TestSeatCreation:
    """Seat construction and port allocation."""

    def test_basic_creation(self):
        seat = Seat(
            name="test-seat-0",
            seat_index=0,
            display_index=0,
            udp_port=7331,
            api_port=7382,
        )
        assert seat.name == "test-seat-0"
        assert seat.seat_index == 0
        assert seat.display_index == 0
        assert seat.udp_port == 7331
        assert seat.api_port == 7382

    def test_default_values(self):
        seat = Seat(
            name="s", seat_index=1, display_index=1,
            udp_port=7332, api_port=7383,
        )
        assert seat.input_devices == []
        assert seat.audio_sink is None
        assert seat.capture_fps == 15
        assert seat.capture_width == 1920
        assert seat.capture_height == 1080
        assert seat.encoder_args == []
        assert seat.display is None

    def test_custom_params(self):
        seat = Seat(
            name="gaming-seat",
            seat_index=2,
            display_index=3,
            udp_port=7333,
            api_port=7384,
            input_devices=["/dev/input/event0", "/dev/input/event1"],
            audio_sink="ozma-gaming-seat",
            capture_fps=60,
            capture_width=2560,
            capture_height=1440,
            encoder_args=["-c:v", "h264_nvenc", "-preset", "p4"],
        )
        assert seat.input_devices == ["/dev/input/event0", "/dev/input/event1"]
        assert seat.audio_sink == "ozma-gaming-seat"
        assert seat.capture_fps == 60
        assert seat.capture_width == 2560
        assert seat.capture_height == 1440
        assert seat.encoder_args == ["-c:v", "h264_nvenc", "-preset", "p4"]

    def test_sequential_port_allocation(self):
        """Seats should use sequential ports based on seat_index."""
        base_udp = 7331
        base_api = 7382
        seats = []
        for i in range(4):
            seats.append(Seat(
                name=f"seat-{i}",
                seat_index=i,
                display_index=i,
                udp_port=base_udp + i,
                api_port=base_api + i,
            ))

        assert [s.udp_port for s in seats] == [7331, 7332, 7333, 7334]
        assert [s.api_port for s in seats] == [7382, 7383, 7384, 7385]

    def test_output_dir_per_seat(self):
        s0 = Seat(name="s0", seat_index=0, display_index=0,
                  udp_port=7331, api_port=7382)
        s1 = Seat(name="s1", seat_index=1, display_index=1,
                  udp_port=7332, api_port=7383)
        assert s0._output_dir == Path("/tmp/ozma-seat-0")
        assert s1._output_dir == Path("/tmp/ozma-seat-1")


# ── Serialization ────────────────────────────────────────────────────────────

class TestSeatSerialization:
    """Seat.to_dict() output format."""

    def test_to_dict_basic(self):
        seat = Seat(
            name="test-seat-0",
            seat_index=0,
            display_index=0,
            udp_port=7331,
            api_port=7382,
        )
        d = seat.to_dict()
        assert d["name"] == "test-seat-0"
        assert d["seat_index"] == 0
        assert d["display_index"] == 0
        assert d["udp_port"] == 7331
        assert d["api_port"] == 7382
        assert d["input_devices"] == []
        assert d["audio_sink"] is None
        assert d["display_name"] is None

    def test_to_dict_with_display(self):
        seat = Seat(
            name="test-seat-0",
            seat_index=0,
            display_index=0,
            udp_port=7331,
            api_port=7382,
        )
        seat.display = DisplayInfo(
            index=0, name="HDMI-1", width=1920, height=1080,
        )
        d = seat.to_dict()
        assert d["display_name"] == "HDMI-1"

    def test_to_dict_capture_inactive_when_no_process(self):
        seat = Seat(
            name="test-seat-0",
            seat_index=0,
            display_index=0,
            udp_port=7331,
            api_port=7382,
        )
        d = seat.to_dict()
        assert d["capture"]["active"] is False
        assert d["capture"]["fps"] == 15
        assert d["capture"]["resolution"] == "1920x1080"
        assert d["capture"]["stream_path"] is None

    def test_to_dict_capture_active_with_mock_process(self):
        seat = Seat(
            name="test-seat-0",
            seat_index=0,
            display_index=0,
            udp_port=7331,
            api_port=7382,
        )
        # Mock a running process
        mock_proc = MagicMock()
        mock_proc.returncode = None
        seat._screen_proc = mock_proc
        d = seat.to_dict()
        assert d["capture"]["active"] is True
        assert d["capture"]["stream_path"] == "/tmp/ozma-seat-0/stream.m3u8"

    def test_to_dict_encoder_args(self):
        seat = Seat(
            name="test-seat-0",
            seat_index=0,
            display_index=0,
            udp_port=7331,
            api_port=7382,
            encoder_args=["-c:v", "h264_nvenc"],
        )
        d = seat.to_dict()
        assert d["capture"]["encoder_args"] == ["-c:v", "h264_nvenc"]

    def test_to_dict_webrtc_unavailable(self):
        seat = Seat(
            name="test-seat-0",
            seat_index=0,
            display_index=0,
            udp_port=7331,
            api_port=7382,
        )
        d = seat.to_dict()
        assert d["webrtc"]["available"] is False
        assert d["webrtc"]["peers"] == 0

    def test_to_dict_is_json_serializable(self):
        seat = Seat(
            name="test-seat-0",
            seat_index=0,
            display_index=0,
            udp_port=7331,
            api_port=7382,
        )
        # Should not raise
        json.dumps(seat.to_dict())


# ── Lifecycle ────────────────────────────────────────────────────────────────

class TestSeatLifecycle:
    """Seat start/stop with mocked internals."""

    @pytest.mark.asyncio
    async def test_stop_when_not_started(self):
        """Stopping a seat that was never started should not raise."""
        seat = Seat(
            name="test-seat-0",
            seat_index=0,
            display_index=0,
            udp_port=7331,
            api_port=7382,
        )
        await seat.stop()  # should be a no-op

    @pytest.mark.asyncio
    async def test_stop_terminates_screen_proc(self):
        seat = Seat(
            name="test-seat-0",
            seat_index=0,
            display_index=0,
            udp_port=7331,
            api_port=7382,
        )
        mock_proc = AsyncMock()
        mock_proc.returncode = None
        mock_proc.terminate = MagicMock()
        mock_proc.kill = MagicMock()
        mock_proc.wait = AsyncMock()
        seat._screen_proc = mock_proc

        await seat.stop()
        mock_proc.terminate.assert_called_once()
        assert seat._screen_proc is None

    @pytest.mark.asyncio
    async def test_stop_closes_transport(self):
        seat = Seat(
            name="test-seat-0",
            seat_index=0,
            display_index=0,
            udp_port=7331,
            api_port=7382,
        )
        mock_transport = MagicMock()
        seat._transport = mock_transport

        await seat.stop()
        mock_transport.close.assert_called_once()
        assert seat._transport is None

    @pytest.mark.asyncio
    async def test_stop_cleans_up_runner(self):
        seat = Seat(
            name="test-seat-0",
            seat_index=0,
            display_index=0,
            udp_port=7331,
            api_port=7382,
        )
        mock_runner = AsyncMock()
        seat._runner = mock_runner

        await seat.stop()
        mock_runner.cleanup.assert_called_once()
        assert seat._runner is None

    @pytest.mark.asyncio
    async def test_stop_stops_hid_injector(self):
        seat = Seat(
            name="test-seat-0",
            seat_index=0,
            display_index=0,
            udp_port=7331,
            api_port=7382,
        )
        mock_injector = AsyncMock()
        seat._hid_injector = mock_injector

        await seat.stop()
        mock_injector.stop.assert_called_once()
        assert seat._hid_injector is None

    @pytest.mark.asyncio
    async def test_stop_cleans_up_webrtc(self):
        seat = Seat(
            name="test-seat-0",
            seat_index=0,
            display_index=0,
            udp_port=7331,
            api_port=7382,
        )
        mock_webrtc = AsyncMock()
        seat._webrtc = mock_webrtc

        await seat.stop()
        mock_webrtc.cleanup.assert_called_once()
        assert seat._webrtc is None


# ── HID packet handling ──────────────────────────────────────────────────────

class TestSeatHIDPacket:
    """Packet dispatch from UDP handler."""

    def test_on_packet_ignores_short(self):
        seat = Seat(
            name="test-seat-0",
            seat_index=0,
            display_index=0,
            udp_port=7331,
            api_port=7382,
        )
        # Should not raise
        seat._on_packet(b"", ("127.0.0.1", 1234))
        seat._on_packet(b"\x01", ("127.0.0.1", 1234))

    def test_on_packet_keyboard(self):
        seat = Seat(
            name="test-seat-0",
            seat_index=0,
            display_index=0,
            udp_port=7331,
            api_port=7382,
        )
        mock_injector = MagicMock()
        seat._hid_injector = mock_injector

        # Type 0x01 = keyboard, 8 bytes payload
        payload = bytes([0x01, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00])
        seat._on_packet(payload, ("127.0.0.1", 1234))
        mock_injector.inject_keyboard.assert_called_once()

    def test_on_packet_mouse(self):
        seat = Seat(
            name="test-seat-0",
            seat_index=0,
            display_index=0,
            udp_port=7331,
            api_port=7382,
        )
        mock_injector = MagicMock()
        seat._hid_injector = mock_injector

        # Type 0x02 = mouse, 6 bytes payload
        payload = bytes([0x02, 0x01, 0x00, 0x80, 0x00, 0x80, 0x00])
        seat._on_packet(payload, ("127.0.0.1", 1234))
        mock_injector.inject_mouse.assert_called_once()

    def test_on_packet_no_injector(self):
        seat = Seat(
            name="test-seat-0",
            seat_index=0,
            display_index=0,
            udp_port=7331,
            api_port=7382,
        )
        seat._hid_injector = None

        # Should not raise even though no injector is set
        payload = bytes([0x01, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00])
        seat._on_packet(payload, ("127.0.0.1", 1234))


# ── Resolve IP ───────────────────────────────────────────────────────────────

class TestResolveIP:
    def test_resolve_returns_string(self):
        ip = Seat._resolve_local_ip()
        assert isinstance(ip, str)
        assert len(ip) > 0
