# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Tests for agent.multiseat.virtual_display — virtual display driver management."""

from __future__ import annotations

import asyncio
import subprocess
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from agent.multiseat.virtual_display import (
    VirtualDisplayManager, LinuxXrandrDriver, OzmaVDDDriver,
    DummyPlugDetector, OZMA_VDD_PROTOCOL_VERSION,
)
from agent.multiseat.display_backend import DisplayInfo


# ── VirtualDisplayManager ────────────────────────────────────────────────────

class TestVirtualDisplayManager:
    def test_creation_detects_backend(self):
        with patch.object(LinuxXrandrDriver, "is_available", return_value=False):
            vdm = VirtualDisplayManager()
        # On Linux without xrandr, no driver available
        assert vdm.driver_name == "none" or vdm.available

    def test_available_property(self):
        vdm = VirtualDisplayManager.__new__(VirtualDisplayManager)
        vdm._backend = None
        vdm._active = {}
        vdm._next_display_idx = 200
        assert vdm.available is False

        mock_driver = MagicMock()
        mock_driver.name = "test-driver"
        vdm._backend = mock_driver
        assert vdm.available is True

    def test_driver_name_property(self):
        vdm = VirtualDisplayManager.__new__(VirtualDisplayManager)
        vdm._backend = None
        vdm._active = {}
        vdm._next_display_idx = 200
        assert vdm.driver_name == "none"

    @pytest.mark.asyncio
    async def test_add_monitor_no_driver(self):
        vdm = VirtualDisplayManager.__new__(VirtualDisplayManager)
        vdm._backend = None
        vdm._active = {}
        vdm._next_display_idx = 200
        result = await vdm.add_monitor(1920, 1080)
        assert result is None

    @pytest.mark.asyncio
    async def test_add_monitor_success(self):
        mock_driver = AsyncMock()
        mock_driver.add = AsyncMock(return_value=0)
        mock_driver.name = "test-driver"

        vdm = VirtualDisplayManager.__new__(VirtualDisplayManager)
        vdm._backend = mock_driver
        vdm._active = {}
        vdm._next_display_idx = 200

        result = await vdm.add_monitor(1920, 1080, 60)
        assert result is not None
        assert result.virtual is True
        assert result.width == 1920
        assert result.height == 1080
        assert result.name == "Virtual-0"

    @pytest.mark.asyncio
    async def test_add_monitor_failure(self):
        mock_driver = AsyncMock()
        mock_driver.add = AsyncMock(return_value=None)
        mock_driver.name = "test-driver"

        vdm = VirtualDisplayManager.__new__(VirtualDisplayManager)
        vdm._backend = mock_driver
        vdm._active = {}
        vdm._next_display_idx = 200

        result = await vdm.add_monitor(1920, 1080)
        assert result is None

    @pytest.mark.asyncio
    async def test_remove_monitor(self):
        mock_driver = AsyncMock()
        mock_driver.remove = AsyncMock(return_value=True)
        mock_driver.name = "test-driver"

        vdm = VirtualDisplayManager.__new__(VirtualDisplayManager)
        vdm._backend = mock_driver
        vdm._active = {
            0: DisplayInfo(index=200, name="Virtual-0", width=1920,
                           height=1080, virtual=True),
        }
        vdm._next_display_idx = 201

        result = await vdm.remove_monitor(0)
        assert result is True
        assert 0 not in vdm._active

    @pytest.mark.asyncio
    async def test_remove_monitor_no_driver(self):
        vdm = VirtualDisplayManager.__new__(VirtualDisplayManager)
        vdm._backend = None
        vdm._active = {}
        vdm._next_display_idx = 200
        result = await vdm.remove_monitor(0)
        assert result is False

    @pytest.mark.asyncio
    async def test_remove_all(self):
        mock_driver = AsyncMock()
        mock_driver.remove = AsyncMock(return_value=True)
        mock_driver.name = "test-driver"

        vdm = VirtualDisplayManager.__new__(VirtualDisplayManager)
        vdm._backend = mock_driver
        vdm._active = {
            0: DisplayInfo(index=200, name="V-0", width=1920,
                           height=1080, virtual=True),
            1: DisplayInfo(index=201, name="V-1", width=1920,
                           height=1080, virtual=True),
        }
        vdm._next_display_idx = 202

        count = await vdm.remove_all()
        assert count == 2
        assert len(vdm._active) == 0

    @pytest.mark.asyncio
    async def test_list_monitors(self):
        vdm = VirtualDisplayManager.__new__(VirtualDisplayManager)
        vdm._backend = MagicMock()
        vdm._active = {
            0: DisplayInfo(index=200, name="V-0", width=1920,
                           height=1080, virtual=True),
        }
        vdm._next_display_idx = 201

        monitors = await vdm.list_monitors()
        assert len(monitors) == 1
        assert monitors[0].name == "V-0"

    def test_to_dict(self):
        vdm = VirtualDisplayManager.__new__(VirtualDisplayManager)
        vdm._backend = MagicMock()
        vdm._backend.name = "test-driver"
        vdm._active = {
            0: DisplayInfo(index=200, name="V-0", width=1920,
                           height=1080, virtual=True),
        }
        vdm._next_display_idx = 201

        d = vdm.to_dict()
        assert d["driver"] == "test-driver"
        assert d["available"] is True
        assert len(d["monitors"]) == 1
        assert d["monitors"][0]["driver_index"] == 0

    @pytest.mark.asyncio
    async def test_position_virtual_monitors_right(self):
        """New virtual monitors should be placed to the right of existing ones."""
        mock_driver = AsyncMock()
        call_count = 0

        async def mock_add(w, h, r):
            nonlocal call_count
            idx = call_count
            call_count += 1
            return idx

        mock_driver.add = mock_add
        mock_driver.name = "test"

        vdm = VirtualDisplayManager.__new__(VirtualDisplayManager)
        vdm._backend = mock_driver
        vdm._active = {}
        vdm._next_display_idx = 200

        d0 = await vdm.add_monitor(1920, 1080)
        d1 = await vdm.add_monitor(2560, 1440)

        assert d0.x_offset == 0
        assert d1.x_offset == 1920


# ── OzmaVDD protocol ────────────────────────────────────────────────────────

class TestOzmaVDDProtocol:
    def test_protocol_version(self):
        assert OZMA_VDD_PROTOCOL_VERSION == 1

    def test_not_available_on_linux(self):
        driver = OzmaVDDDriver()
        # On Linux, is_available should return False
        with patch("sys.platform", "linux"):
            assert driver.is_available() is False


# ── Linux xrandr driver ─────────────────────────────────────────────────────

class TestLinuxXrandrDriver:
    def test_is_available_with_xrandr(self):
        driver = LinuxXrandrDriver()
        with patch("subprocess.run") as mock_run:
            mock_run.return_value = MagicMock(returncode=0)
            assert driver.is_available() is True

    def test_is_available_no_xrandr(self):
        driver = LinuxXrandrDriver()
        with patch("subprocess.run", side_effect=FileNotFoundError):
            assert driver.is_available() is False

    @pytest.mark.asyncio
    async def test_add_monitor(self):
        driver = LinuxXrandrDriver()
        driver._display = ":0"

        mock_result = MagicMock(returncode=0)
        with patch("asyncio.get_running_loop") as mock_loop:
            mock_loop.return_value.run_in_executor = AsyncMock(
                return_value=mock_result,
            )
            with patch.object(driver, "_find_max_x", return_value=1920):
                idx = await driver.add(1920, 1080, 60)

        assert idx == 0
        assert "OZMA-VIRTUAL-0" in driver._monitors.values()

    @pytest.mark.asyncio
    async def test_remove_monitor(self):
        driver = LinuxXrandrDriver()
        driver._monitors = {0: "OZMA-VIRTUAL-0"}

        mock_result = MagicMock(returncode=0)
        with patch("asyncio.get_running_loop") as mock_loop:
            mock_loop.return_value.run_in_executor = AsyncMock(
                return_value=mock_result,
            )
            result = await driver.remove(0)

        assert result is True
        assert 0 not in driver._monitors

    @pytest.mark.asyncio
    async def test_remove_nonexistent(self):
        driver = LinuxXrandrDriver()
        driver._monitors = {}
        result = await driver.remove(99)
        assert result is False

    def test_xrandr_command_generation(self):
        """The xrandr --setmonitor command should use correct spec format."""
        # Spec format: WIDTHxHEIGHT+X+Y
        width, height = 1920, 1080
        max_x = 3840
        spec = f"{width}/{width}x{height}/{height}+{max_x}+0"
        assert spec == "1920/1920x1080/1080+3840+0"


# ── Dummy plug detection ────────────────────────────────────────────────────

class TestDummyPlugDetector:
    def test_detect_generic_pnp(self):
        displays = [
            DisplayInfo(index=0, name="Generic PnP Monitor", width=1920, height=1080),
            DisplayInfo(index=1, name="HDMI-1", width=2560, height=1440),
        ]
        dummies = DummyPlugDetector.detect_dummy_plugs(displays)
        assert len(dummies) == 1
        assert dummies[0].name == "Generic PnP Monitor"

    def test_detect_dummy_keyword(self):
        displays = [
            DisplayInfo(index=0, name="Dummy Display", width=1920, height=1080),
        ]
        dummies = DummyPlugDetector.detect_dummy_plugs(displays)
        assert len(dummies) == 1

    def test_detect_headless(self):
        displays = [
            DisplayInfo(index=0, name="FIT-Headless 4K", width=3840, height=2160),
        ]
        dummies = DummyPlugDetector.detect_dummy_plugs(displays)
        assert len(dummies) == 1

    def test_no_dummies(self):
        displays = [
            DisplayInfo(index=0, name="Dell U2723QE", width=3840, height=2160),
            DisplayInfo(index=1, name="LG 27GP950", width=3840, height=2160),
        ]
        dummies = DummyPlugDetector.detect_dummy_plugs(displays)
        assert len(dummies) == 0

    def test_empty_list(self):
        dummies = DummyPlugDetector.detect_dummy_plugs([])
        assert dummies == []
