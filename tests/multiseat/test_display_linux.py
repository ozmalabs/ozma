# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Tests for agent.multiseat.display_linux — xrandr display enumeration."""

from __future__ import annotations

import subprocess
from unittest.mock import MagicMock, patch

import pytest

from agent.multiseat.display_linux import LinuxDisplayBackend


# ── xrandr output parsing ────────────────────────────────────────────────────

class TestXrandrParsing:
    """Test display enumeration from mock xrandr output."""

    XRANDR_TWO_MONITORS = """\
Monitors: 2
 0: +*HDMI-1 1920/530x1080/300+0+0  HDMI-1
 1: +DP-2 2560/600x1440/340+1920+0  DP-2
"""

    XRANDR_THREE_MONITORS = """\
Monitors: 3
 0: +*DP-1 3840/600x2160/340+0+0  DP-1
 1: +HDMI-1 1920/530x1080/300+3840+0  HDMI-1
 2: +HDMI-2 1920/530x1080/300+5760+0  HDMI-2
"""

    XRANDR_VIRTUAL = """\
Monitors: 2
 0: +*HDMI-1 1920/530x1080/300+0+0  HDMI-1
 1: +OZMA-VIRTUAL-0 1920/1920x1080/1080+1920+0  OZMA-VIRTUAL-0
"""

    def test_parse_two_monitors(self):
        backend = LinuxDisplayBackend()
        mock_result = MagicMock(
            returncode=0,
            stdout=self.XRANDR_TWO_MONITORS,
        )
        with patch("subprocess.run", return_value=mock_result):
            displays = backend.enumerate()

        assert len(displays) == 2
        assert displays[0].name == "HDMI-1"
        assert displays[0].width == 1920
        assert displays[0].height == 1080
        assert displays[0].x_offset == 0
        assert displays[0].y_offset == 0
        assert displays[0].primary is True

        assert displays[1].name == "DP-2"
        assert displays[1].width == 2560
        assert displays[1].height == 1440
        assert displays[1].x_offset == 1920

    def test_parse_three_monitors(self):
        backend = LinuxDisplayBackend()
        mock_result = MagicMock(
            returncode=0,
            stdout=self.XRANDR_THREE_MONITORS,
        )
        with patch("subprocess.run", return_value=mock_result):
            displays = backend.enumerate()

        assert len(displays) == 3
        assert displays[0].name == "DP-1"
        assert displays[0].width == 3840
        assert displays[0].height == 2160
        assert displays[2].name == "HDMI-2"
        assert displays[2].x_offset == 5760

    def test_parse_with_virtual_monitor(self):
        backend = LinuxDisplayBackend()
        mock_result = MagicMock(
            returncode=0,
            stdout=self.XRANDR_VIRTUAL,
        )
        with patch("subprocess.run", return_value=mock_result):
            displays = backend.enumerate()

        assert len(displays) == 2
        assert displays[1].name == "OZMA-VIRTUAL-0"

    def test_xrandr_not_found(self):
        backend = LinuxDisplayBackend()
        with patch("subprocess.run", side_effect=FileNotFoundError):
            displays = backend.enumerate()
        # Should fall back to single default display
        assert len(displays) == 1
        assert displays[0].name == "default"
        assert displays[0].width == 1920

    def test_xrandr_timeout(self):
        backend = LinuxDisplayBackend()
        with patch("subprocess.run", side_effect=subprocess.TimeoutExpired("xrandr", 5)):
            displays = backend.enumerate()
        assert len(displays) == 1
        assert displays[0].name == "default"

    def test_xrandr_failure(self):
        backend = LinuxDisplayBackend()
        mock_result = MagicMock(
            returncode=1,
            stderr="xrandr: failed",
            stdout="",
        )
        with patch("subprocess.run", return_value=mock_result):
            displays = backend.enumerate()
        assert len(displays) == 1
        assert displays[0].name == "default"

    def test_empty_xrandr_output(self):
        backend = LinuxDisplayBackend()
        mock_result = MagicMock(
            returncode=0,
            stdout="Monitors: 0\n",
        )
        with patch("subprocess.run", return_value=mock_result):
            displays = backend.enumerate()
        # No monitors parsed = fallback
        assert len(displays) == 1
        assert displays[0].name == "default"


# ── Virtual display creation ─────────────────────────────────────────────────

class TestVirtualDisplayCreation:
    def test_create_virtual_command(self):
        backend = LinuxDisplayBackend()

        mock_enum = MagicMock(returncode=0, stdout="""\
Monitors: 1
 0: +*HDMI-1 1920/530x1080/300+0+0  HDMI-1
""")
        mock_create = MagicMock(returncode=0)

        with patch("subprocess.run", side_effect=[mock_enum, mock_create]):
            display = backend.create_virtual(1920, 1080)

        assert display is not None
        assert display.virtual is True
        assert display.width == 1920
        assert display.height == 1080
        assert display.x_offset == 1920  # placed after HDMI-1

    def test_create_virtual_xrandr_not_found(self):
        backend = LinuxDisplayBackend()
        mock_enum = MagicMock(returncode=0, stdout="Monitors: 0\n")

        with patch("subprocess.run", side_effect=[mock_enum, FileNotFoundError]):
            display = backend.create_virtual(1920, 1080)

        assert display is None

    def test_create_virtual_failure(self):
        backend = LinuxDisplayBackend()
        mock_enum = MagicMock(returncode=0, stdout="Monitors: 0\n")
        mock_create = MagicMock(returncode=1, stderr="error")

        with patch("subprocess.run", side_effect=[mock_enum, mock_create]):
            display = backend.create_virtual(1920, 1080)

        assert display is None


# ── Virtual display destruction ──────────────────────────────────────────────

class TestVirtualDisplayDestruction:
    def test_destroy_virtual(self):
        backend = LinuxDisplayBackend()
        from agent.multiseat.display_backend import DisplayInfo

        display = DisplayInfo(index=100, name="VIRTUAL-100", width=1920,
                              height=1080, virtual=True)
        backend._virtual_displays.append(display)

        mock_result = MagicMock(returncode=0)
        with patch("subprocess.run", return_value=mock_result):
            result = backend.destroy_virtual(display)

        assert result is True
        assert display not in backend._virtual_displays

    def test_destroy_non_virtual_fails(self):
        backend = LinuxDisplayBackend()
        from agent.multiseat.display_backend import DisplayInfo

        display = DisplayInfo(index=0, name="HDMI-1", width=1920,
                              height=1080, virtual=False)
        result = backend.destroy_virtual(display)
        assert result is False


# ── Capture parameters ───────────────────────────────────────────────────────

class TestCaptureParameters:
    def test_get_display_for_capture(self):
        backend = LinuxDisplayBackend()
        from agent.multiseat.display_backend import DisplayInfo

        display = DisplayInfo(
            index=1, name="DP-2", width=2560, height=1440,
            x_offset=1920, y_offset=0, x_screen=":0",
        )
        params = backend.get_display_for_capture(display)

        assert params["display"] == ":0"
        assert params["grab_x"] == 1920
        assert params["grab_y"] == 0
        assert params["width"] == 2560
        assert params["height"] == 1440
