"""Unit tests for QEMU D-Bus display console."""

import pytest

pytestmark = pytest.mark.unit


class TestQEMUDBusConsole:
    def test_console_path_default(self):
        from qemu_display import QEMUDBusConsole, DBUS_DISPLAY_PATH
        c = QEMUDBusConsole(0)
        assert c.console_path == f"{DBUS_DISPLAY_PATH}/Console_0"

    def test_console_path_multi(self):
        from qemu_display import QEMUDBusConsole, DBUS_DISPLAY_PATH
        c = QEMUDBusConsole(2)
        assert c.console_path == f"{DBUS_DISPLAY_PATH}/Console_2"

    def test_initial_state(self):
        from qemu_display import QEMUDBusConsole
        c = QEMUDBusConsole(0)
        assert not c.connected
        assert c.width == 0
        assert c.height == 0
        assert c.label == ""

    def test_parse_variant_uint(self):
        from qemu_display import QEMUDBusConsole
        c = QEMUDBusConsole(0)
        text = "({'Width': <uint32 1920>, 'Height': <uint32 1080>},)"
        assert c._parse_variant_uint(text, "Width") == 1920
        assert c._parse_variant_uint(text, "Height") == 1080
        assert c._parse_variant_uint(text, "Missing") == 0

    def test_parse_variant_string(self):
        from qemu_display import QEMUDBusConsole
        c = QEMUDBusConsole(0)
        text = "({'Label': <'virtio-vga'>, 'Type': <'Graphic'>},)"
        assert c._parse_variant_string(text, "Label") == "virtio-vga"
        assert c._parse_variant_string(text, "Type") == "Graphic"
        assert c._parse_variant_string(text, "Missing") == ""


class TestLookingGlassCapture:
    def test_initial_state(self):
        from looking_glass import LookingGlassCapture
        lg = LookingGlassCapture("test-vm", shm_path="/nonexistent")
        assert not lg.available
        assert lg.device_path is None
        assert lg.width == 0
        assert lg.height == 0

    def test_kvmfr_magic(self):
        from looking_glass import KVMFR_MAGIC
        assert KVMFR_MAGIC == b"KVMFR---"

    def test_frame_type_constants(self):
        from looking_glass import (FRAME_TYPE_INVALID, FRAME_TYPE_BGRA,
                                    FRAME_TYPE_RGBA, FRAME_TYPE_RGB_24)
        assert FRAME_TYPE_INVALID == 0
        assert FRAME_TYPE_BGRA == 1
        assert FRAME_TYPE_RGBA == 2
        assert FRAME_TYPE_RGB_24 == 3

    def test_header_struct_size(self):
        import ctypes
        from looking_glass import KVMFRHeader
        assert ctypes.sizeof(KVMFRHeader) == 16
