# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Tests for agent.multiseat.audio_linux — PipeWire/PulseAudio per-seat sinks."""

from __future__ import annotations

import subprocess
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from agent.multiseat.audio_linux import LinuxAudioBackend
from agent.multiseat.audio_backend import SeatAudioBackend


# ── Sink creation ────────────────────────────────────────────────────────────

class TestSinkCreation:
    @pytest.mark.asyncio
    async def test_create_sink_success(self):
        backend = LinuxAudioBackend()
        mock_result = MagicMock(returncode=0, stdout="42\n", stderr="")

        with patch("asyncio.get_running_loop") as mock_loop:
            mock_loop.return_value.run_in_executor = AsyncMock(
                return_value=mock_result,
            )
            sink = await backend.create_sink("seat-0")

        assert sink == "ozma-seat-0"
        assert backend._modules["seat-0"] == 42

    @pytest.mark.asyncio
    async def test_create_sink_naming(self):
        """Sink should be named 'ozma-{seat_name}'."""
        backend = LinuxAudioBackend()
        mock_result = MagicMock(returncode=0, stdout="1\n", stderr="")

        with patch("asyncio.get_running_loop") as mock_loop:
            mock_loop.return_value.run_in_executor = AsyncMock(
                return_value=mock_result,
            )
            sink = await backend.create_sink("gaming-pc-seat-2")

        assert sink == "ozma-gaming-pc-seat-2"

    @pytest.mark.asyncio
    async def test_create_sink_already_exists(self):
        backend = LinuxAudioBackend()
        backend._modules["seat-0"] = 42

        sink = await backend.create_sink("seat-0")
        assert sink == "ozma-seat-0"

    @pytest.mark.asyncio
    async def test_create_sink_pactl_failure(self):
        backend = LinuxAudioBackend()
        mock_result = MagicMock(returncode=1, stdout="", stderr="Failure")

        with patch("asyncio.get_running_loop") as mock_loop:
            mock_loop.return_value.run_in_executor = AsyncMock(
                return_value=mock_result,
            )
            sink = await backend.create_sink("seat-0")

        assert sink is None
        assert "seat-0" not in backend._modules

    @pytest.mark.asyncio
    async def test_create_sink_pactl_not_found(self):
        backend = LinuxAudioBackend()

        with patch("asyncio.get_running_loop") as mock_loop:
            mock_loop.return_value.run_in_executor = AsyncMock(
                side_effect=FileNotFoundError("pactl"),
            )
            sink = await backend.create_sink("seat-0")

        assert sink is None


# ── Sink destruction ─────────────────────────────────────────────────────────

class TestSinkDestruction:
    @pytest.mark.asyncio
    async def test_destroy_sink_success(self):
        backend = LinuxAudioBackend()
        backend._modules["seat-0"] = 42
        mock_result = MagicMock(returncode=0, stderr="")

        with patch("asyncio.get_running_loop") as mock_loop:
            mock_loop.return_value.run_in_executor = AsyncMock(
                return_value=mock_result,
            )
            result = await backend.destroy_sink("seat-0")

        assert result is True
        assert "seat-0" not in backend._modules

    @pytest.mark.asyncio
    async def test_destroy_sink_already_gone(self):
        backend = LinuxAudioBackend()
        result = await backend.destroy_sink("nonexistent")
        assert result is True

    @pytest.mark.asyncio
    async def test_destroy_sink_pactl_failure(self):
        backend = LinuxAudioBackend()
        backend._modules["seat-0"] = 42
        mock_result = MagicMock(returncode=1, stderr="Error")

        with patch("asyncio.get_running_loop") as mock_loop:
            mock_loop.return_value.run_in_executor = AsyncMock(
                return_value=mock_result,
            )
            result = await backend.destroy_sink("seat-0")

        assert result is False


# ── Sink listing ─────────────────────────────────────────────────────────────

class TestSinkListing:
    @pytest.mark.asyncio
    async def test_list_sinks_empty(self):
        backend = LinuxAudioBackend()
        sinks = await backend.list_sinks()
        assert sinks == []

    @pytest.mark.asyncio
    async def test_list_sinks_with_modules(self):
        backend = LinuxAudioBackend()
        backend._modules = {"seat-0": 42, "seat-1": 43}

        sinks = await backend.list_sinks()
        assert len(sinks) == 2
        names = {s["seat"] for s in sinks}
        assert names == {"seat-0", "seat-1"}

        for sink in sinks:
            assert "sink_name" in sink
            assert "module_id" in sink
            assert sink["sink_name"].startswith("ozma-")


# ── Audio routing ────────────────────────────────────────────────────────────

class TestAudioRouting:
    @pytest.mark.asyncio
    async def test_assign_output_success(self):
        backend = LinuxAudioBackend()
        mock_result = MagicMock(returncode=0, stderr="")

        with patch("asyncio.get_running_loop") as mock_loop:
            mock_loop.return_value.run_in_executor = AsyncMock(
                return_value=mock_result,
            )
            result = await backend.assign_output(
                "seat-0", "alsa_output.usb-headset",
            )

        assert result is True

    @pytest.mark.asyncio
    async def test_assign_output_failure(self):
        backend = LinuxAudioBackend()
        mock_result = MagicMock(returncode=1, stderr="Error")

        with patch("asyncio.get_running_loop") as mock_loop:
            mock_loop.return_value.run_in_executor = AsyncMock(
                return_value=mock_result,
            )
            result = await backend.assign_output(
                "seat-0", "nonexistent",
            )

        assert result is False


# ── Cleanup ──────────────────────────────────────────────────────────────────

class TestCleanup:
    @pytest.mark.asyncio
    async def test_destroy_all(self):
        backend = LinuxAudioBackend()
        backend._modules = {"seat-0": 42, "seat-1": 43}
        mock_result = MagicMock(returncode=0, stderr="")

        with patch("asyncio.get_running_loop") as mock_loop:
            mock_loop.return_value.run_in_executor = AsyncMock(
                return_value=mock_result,
            )
            await backend.destroy_all()

        assert len(backend._modules) == 0


# ── Abstract backend ─────────────────────────────────────────────────────────

class TestAudioBackendABC:
    def test_abstract_methods(self):
        """SeatAudioBackend is abstract and cannot be instantiated."""
        with pytest.raises(TypeError):
            SeatAudioBackend()
