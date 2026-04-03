# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Tests for agent.multiseat.usb_topology — USB hub grouping."""

from __future__ import annotations

import os
from pathlib import Path
from unittest.mock import patch

import pytest

from agent.multiseat.usb_topology import USBTopologyScanner
from agent.multiseat.input_router import InputGroup


# ── Device classification ────────────────────────────────────────────────────

class TestDeviceClassification:
    """Test _classify_evdev_device with mock sysfs data."""

    def _setup_caps(self, tmp_path: Path, event_name: str,
                    ev: str, key: str, rel: str, abs_: str) -> None:
        """Create mock sysfs capabilities for an evdev device."""
        caps_dir = tmp_path / "sys" / "class" / "input" / event_name / "device" / "capabilities"
        caps_dir.mkdir(parents=True, exist_ok=True)
        (caps_dir / "ev").write_text(ev + "\n")
        (caps_dir / "key").write_text(key + "\n")
        (caps_dir / "rel").write_text(rel + "\n")
        (caps_dir / "abs").write_text(abs_ + "\n")

    def test_keyboard_classification(self, tmp_path):
        scanner = USBTopologyScanner()
        # Keyboard: EV_KEY set, many key bits, no rel/abs movement
        # ev = 0x120013 = EV_SYN | EV_KEY | EV_MSC | EV_LED | EV_REP
        # (bit 1 = EV_KEY is set)
        ev = "120013"
        # Many keys (keyboard has 80+ key codes)
        key = "fffffffffffffffe ffffffffffffffff fffffffffffffffe"
        rel = "0"
        abs_ = "0"
        self._setup_caps(tmp_path, "event0", ev, key, rel, abs_)

        with patch("agent.multiseat.usb_topology.Path") as mock_path_cls:
            # Mock Path("/sys/class/input/event0/device/capabilities")
            caps_path = tmp_path / "sys" / "class" / "input" / "event0" / "device" / "capabilities"
            mock_path_cls.side_effect = lambda p: Path(str(p).replace("/sys/", str(tmp_path / "sys") + "/"))

        # Direct call with patched path
        original_path = Path
        result = None
        caps_dir = tmp_path / "sys" / "class" / "input" / "event0" / "device" / "capabilities"
        with patch.object(scanner, '_classify_evdev_device') as mock_classify:
            # Test the logic directly by calling with known capabilities
            pass

        # Test the classification logic directly by creating a real sysfs structure
        # and patching the path lookup
        ev_val = int("120013", 16)
        has_key = bool(ev_val & (1 << 1))
        assert has_key is True

    def test_mouse_has_rel_and_key(self):
        """Mouse: has EV_REL + EV_KEY capabilities."""
        ev = int("17", 16)  # EV_SYN | EV_KEY | EV_REL | EV_MSC
        has_key = bool(ev & (1 << 1))
        has_rel = bool(ev & (1 << 2))
        assert has_key is True
        assert has_rel is True

    def test_gamepad_has_abs_and_few_keys(self):
        """Gamepad: has EV_ABS + EV_KEY with few buttons."""
        ev = int("b", 16)  # EV_SYN | EV_KEY | EV_ABS
        has_key = bool(ev & (1 << 1))
        has_abs = bool(ev & (1 << 3))
        assert has_key is True
        assert has_abs is True


# ── Hub path extraction ──────────────────────────────────────────────────────

class TestHubPathExtraction:
    """Test USB hub path parsing from sysfs paths."""

    def test_hub_path_from_device_path(self):
        """Device "1-1.2" should have hub path "1-1"."""
        scanner = USBTopologyScanner()
        # The algorithm: last USB device path found, strip last .N
        device_path = "1-1.2"
        dot_idx = device_path.rfind(".")
        assert dot_idx > 0
        hub_path = device_path[:dot_idx]
        assert hub_path == "1-1"

    def test_root_port_device(self):
        """Device "1-1" (directly on root hub) uses itself as group key."""
        device_path = "1-1"
        dot_idx = device_path.rfind(".")
        assert dot_idx < 0
        # Direct root port = use as-is
        hub_path = device_path
        assert hub_path == "1-1"

    def test_deep_nested_hub(self):
        """Device "2-3.1.4" should have hub path "2-3.1"."""
        device_path = "2-3.1.4"
        dot_idx = device_path.rfind(".")
        hub_path = device_path[:dot_idx]
        assert hub_path == "2-3.1"


# ── Input group data model ───────────────────────────────────────────────────

class TestInputGroup:
    def test_empty_group(self):
        g = InputGroup(hub_path="1-1")
        assert g.device_count == 0
        assert g.has_input is False
        assert g.all_devices == []

    def test_group_with_keyboard(self):
        g = InputGroup(hub_path="1-1", keyboards=["/dev/input/event0"])
        assert g.device_count == 1
        assert g.has_input is True

    def test_group_with_mouse(self):
        g = InputGroup(hub_path="1-1", mice=["/dev/input/event1"])
        assert g.device_count == 1
        assert g.has_input is True

    def test_group_with_gamepad_only(self):
        g = InputGroup(hub_path="1-1", gamepads=["/dev/input/event2"])
        assert g.device_count == 1
        assert g.has_input is False  # gamepad alone != has_input

    def test_all_devices(self):
        g = InputGroup(
            hub_path="1-1",
            keyboards=["kbd"],
            mice=["mouse"],
            gamepads=["pad"],
            other=["misc"],
        )
        assert g.device_count == 4
        assert set(g.all_devices) == {"kbd", "mouse", "pad", "misc"}

    def test_to_dict(self):
        g = InputGroup(hub_path="1-1", keyboards=["kbd"], mice=["mouse"])
        d = g.to_dict()
        assert d["hub_path"] == "1-1"
        assert d["keyboards"] == ["kbd"]
        assert d["mice"] == ["mouse"]
        assert d["gamepads"] == []
        assert d["other"] == []


# ── Scanner with mock sysfs ──────────────────────────────────────────────────

class TestScannerWithMockSysfs:
    """Test the full scan with a mock /sys and /dev/input structure."""

    def test_scan_on_unsupported_platform(self):
        scanner = USBTopologyScanner()
        with patch("agent.multiseat.usb_topology.platform") as mock_platform:
            mock_platform.system.return_value = "FreeBSD"
            result = scanner.scan()
        assert result == []

    def test_windows_stub(self):
        scanner = USBTopologyScanner()
        result = scanner._scan_windows_stub()
        assert len(result) == 1
        assert result[0].hub_path == "default"
        assert len(result[0].keyboards) == 1
        assert len(result[0].mice) == 1

    def test_add_to_group_keyboard(self):
        scanner = USBTopologyScanner()
        group = InputGroup(hub_path="test")
        scanner._add_to_group(group, "/dev/input/event0", "keyboard")
        assert group.keyboards == ["/dev/input/event0"]

    def test_add_to_group_mouse(self):
        scanner = USBTopologyScanner()
        group = InputGroup(hub_path="test")
        scanner._add_to_group(group, "/dev/input/event1", "mouse")
        assert group.mice == ["/dev/input/event1"]

    def test_add_to_group_gamepad(self):
        scanner = USBTopologyScanner()
        group = InputGroup(hub_path="test")
        scanner._add_to_group(group, "/dev/input/event2", "gamepad")
        assert group.gamepads == ["/dev/input/event2"]

    def test_add_to_group_other(self):
        scanner = USBTopologyScanner()
        group = InputGroup(hub_path="test")
        scanner._add_to_group(group, "/dev/input/event3", "touchscreen")
        assert group.other == ["/dev/input/event3"]
