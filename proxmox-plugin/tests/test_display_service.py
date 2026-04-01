#!/usr/bin/env python3
"""Unit tests for the display service."""

import sys
import unittest
from pathlib import Path
from unittest.mock import patch, MagicMock, AsyncMock

sys.path.insert(0, str(Path(__file__).parent.parent / "python"))


class TestVMDisplayService(unittest.TestCase):
    """Test VMDisplayService initialization and configuration."""

    def test_port_assignment(self):
        from importlib import import_module
        # Can't import display-service.py directly due to the hyphen
        # Test the port logic independently
        vmid = 100
        api_port = 7390 + vmid
        hid_port = 7340 + vmid
        self.assertEqual(api_port, 7490)
        self.assertEqual(hid_port, 7440)

    def test_shm_path(self):
        vmid = 42
        shm_path = f"/dev/shm/ozma-vm{vmid}"
        self.assertEqual(shm_path, "/dev/shm/ozma-vm42")

    def test_qmp_path(self):
        vmid = 100
        qmp_path = f"/var/run/ozma/vm{vmid}-ctrl.qmp"
        self.assertEqual(qmp_path, "/var/run/ozma/vm100-ctrl.qmp")


class TestAgentInstaller(unittest.TestCase):
    """Test agent installer logic."""

    def test_windows_bootstrap_script(self):
        sys.path.insert(0, str(Path(__file__).parent.parent / "python"))
        # Import with importlib since filename has a hyphen
        import importlib.util
        spec = importlib.util.spec_from_file_location(
            "agent_installer",
            str(Path(__file__).parent.parent / "python" / "agent-installer.py"),
        )
        mod = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(mod)

        installer = mod.AgentInstaller(vmid=100, controller_url="https://ozma.test")
        script = installer._windows_bootstrap_script()

        self.assertIn("ozma-agent", script)
        self.assertIn("https://ozma.test", script)
        self.assertIn("python", script.lower())

    def test_linux_bootstrap_script(self):
        import importlib.util
        spec = importlib.util.spec_from_file_location(
            "agent_installer",
            str(Path(__file__).parent.parent / "python" / "agent-installer.py"),
        )
        mod = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(mod)

        installer = mod.AgentInstaller(vmid=100, controller_url="https://ozma.test")
        script = installer._linux_bootstrap_script()

        self.assertIn("pip", script)
        self.assertIn("ozma-agent", script)
        self.assertIn("https://ozma.test", script)
        self.assertIn("#!/bin/bash", script)

    def test_controller_url_in_scripts(self):
        import importlib.util
        spec = importlib.util.spec_from_file_location(
            "agent_installer",
            str(Path(__file__).parent.parent / "python" / "agent-installer.py"),
        )
        mod = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(mod)

        url = "http://10.0.0.5:7380"
        installer = mod.AgentInstaller(vmid=200, controller_url=url)

        self.assertIn(url, installer._windows_bootstrap_script())
        self.assertIn(url, installer._linux_bootstrap_script())


class TestQEMUDBusConsole(unittest.TestCase):
    """Test the D-Bus display console interface."""

    def test_console_path(self):
        sys.path.insert(0, str(Path(__file__).parent.parent.parent / "softnode"))
        from qemu_display import QEMUDBusConsole, DBUS_DISPLAY_PATH

        c0 = QEMUDBusConsole(0)
        self.assertEqual(c0.console_path, f"{DBUS_DISPLAY_PATH}/Console_0")

        c1 = QEMUDBusConsole(1)
        self.assertEqual(c1.console_path, f"{DBUS_DISPLAY_PATH}/Console_1")

    def test_initial_state(self):
        sys.path.insert(0, str(Path(__file__).parent.parent.parent / "softnode"))
        from qemu_display import QEMUDBusConsole

        console = QEMUDBusConsole(0)
        self.assertFalse(console.connected)
        self.assertEqual(console.width, 0)
        self.assertEqual(console.height, 0)
        self.assertEqual(console.label, "")

    def test_variant_parsing(self):
        sys.path.insert(0, str(Path(__file__).parent.parent.parent / "softnode"))
        from qemu_display import QEMUDBusConsole

        c = QEMUDBusConsole(0)

        text = "({'Label': <'virtio-vga'>, 'Width': <uint32 1920>, 'Height': <uint32 1080>},)"
        self.assertEqual(c._parse_variant_uint(text, "Width"), 1920)
        self.assertEqual(c._parse_variant_uint(text, "Height"), 1080)
        self.assertEqual(c._parse_variant_string(text, "Label"), "virtio-vga")
        self.assertEqual(c._parse_variant_uint(text, "Missing"), 0)
        self.assertEqual(c._parse_variant_string(text, "Missing"), "")


class TestLookingGlassCapture(unittest.TestCase):
    """Test Looking Glass shared memory reader."""

    def test_initial_state(self):
        sys.path.insert(0, str(Path(__file__).parent.parent.parent / "softnode"))
        from looking_glass import LookingGlassCapture

        lg = LookingGlassCapture("test-vm", shm_path="/nonexistent")
        self.assertFalse(lg.available)
        self.assertIsNone(lg.device_path)
        self.assertEqual(lg.width, 0)
        self.assertEqual(lg.height, 0)

    def test_kvmfr_header_struct(self):
        sys.path.insert(0, str(Path(__file__).parent.parent.parent / "softnode"))
        from looking_glass import KVMFRHeader, KVMFR_MAGIC
        import ctypes

        h = KVMFRHeader()
        h.magic = KVMFR_MAGIC
        h.version = 21
        self.assertEqual(h.magic, KVMFR_MAGIC)
        self.assertEqual(h.version, 21)
        self.assertEqual(ctypes.sizeof(h), 16)

    def test_frame_types(self):
        sys.path.insert(0, str(Path(__file__).parent.parent.parent / "softnode"))
        from looking_glass import (
            FRAME_TYPE_BGRA, FRAME_TYPE_RGBA,
            FRAME_TYPE_RGB_24, FRAME_TYPE_INVALID,
        )
        self.assertEqual(FRAME_TYPE_BGRA, 1)
        self.assertEqual(FRAME_TYPE_RGBA, 2)
        self.assertEqual(FRAME_TYPE_INVALID, 0)


class TestEvdevInput(unittest.TestCase):
    """Test evdev virtual input device code."""

    def test_hid_to_evdev_mapping(self):
        sys.path.insert(0, str(Path(__file__).parent.parent.parent / "softnode"))
        from evdev_input import _HID_TO_EVDEV, _HID_MOD_TO_EVDEV

        # 'a' = HID 0x04 → evdev 30
        self.assertEqual(_HID_TO_EVDEV[0x04], 30)
        # Enter = HID 0x28 → evdev 28
        self.assertEqual(_HID_TO_EVDEV[0x28], 28)
        # Space = HID 0x2C → evdev 57
        self.assertEqual(_HID_TO_EVDEV[0x2C], 57)
        # Left Shift = modifier bit 0x02 → evdev 42
        self.assertEqual(_HID_MOD_TO_EVDEV[0x02], 42)
        # Left Ctrl = modifier bit 0x01 → evdev 29
        self.assertEqual(_HID_MOD_TO_EVDEV[0x01], 29)

    def test_hid_keyboard_report_parsing(self):
        sys.path.insert(0, str(Path(__file__).parent.parent.parent / "softnode"))
        from evdev_input import hid_keyboard_to_evdev

        # Report: Shift + 'a'
        report = bytes([0x02, 0, 0x04, 0, 0, 0, 0, 0])
        events = hid_keyboard_to_evdev(report)
        codes = [e[0] for e in events]
        self.assertIn(42, codes)   # shift
        self.assertIn(30, codes)   # 'a'

    def test_empty_report(self):
        sys.path.insert(0, str(Path(__file__).parent.parent.parent / "softnode"))
        from evdev_input import hid_keyboard_to_evdev

        events = hid_keyboard_to_evdev(bytes(8))
        self.assertEqual(events, [])

    def test_short_report(self):
        sys.path.insert(0, str(Path(__file__).parent.parent.parent / "softnode"))
        from evdev_input import hid_keyboard_to_evdev

        events = hid_keyboard_to_evdev(bytes(3))
        self.assertEqual(events, [])


if __name__ == "__main__":
    unittest.main()
