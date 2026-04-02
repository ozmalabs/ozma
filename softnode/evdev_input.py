# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Virtual evdev input devices for QEMU integration.

Creates virtual keyboard and mouse devices via uinput that QEMU reads
directly using its `input-linux` object. This completely bypasses QMP
for HID input — no socket, no JSON, no reader/writer races.

Architecture:
  UDP HID packet → soft node → evdev_input.py → /dev/input/eventN
                                                        ↑
                                              QEMU input-linux reads this

QEMU command line:
  -object input-linux,id=kbd,evdev=/dev/input/by-id/ozma-kbd-vm1
  -object input-linux,id=mouse,evdev=/dev/input/by-id/ozma-mouse-vm1,grab_all=on

The virtual devices are created with stable names via udev rules or
symlinks, so QEMU can find them reliably.
"""

from __future__ import annotations

import logging
import struct
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.softnode.evdev_input")

try:
    import evdev
    from evdev import UInput, AbsInfo, ecodes
    _EVDEV_AVAILABLE = True
except ImportError:
    _EVDEV_AVAILABLE = False


class VirtualKeyboard:
    """
    Virtual keyboard via uinput. QEMU reads from the evdev device directly.
    """

    def __init__(self, name: str = "ozma-keyboard") -> None:
        self._name = name
        self._device: Any = None
        self._path: str = ""

    def start(self) -> str | None:
        """Create the virtual keyboard. Returns the evdev path or None."""
        if not _EVDEV_AVAILABLE:
            log.warning("python-evdev not available — evdev input disabled")
            return None

        try:
            # All standard keyboard keys
            keys = list(range(1, 249))  # KEY_ESC through KEY_MICMUTE
            cap = {ecodes.EV_KEY: keys}

            self._device = UInput(
                cap,
                name=self._name,
                vendor=0x1209,   # pid.codes test VID
                product=0x0001,
                version=1,
            )
            self._path = self._device.device.path
            log.info("Virtual keyboard created: %s → %s", self._name, self._path)

            # Create a stable symlink
            self._create_symlink()

            return self._path
        except Exception as e:
            log.error("Failed to create virtual keyboard: %s", e)
            return None

    def stop(self) -> None:
        if self._device:
            try:
                self._device.close()
            except Exception:
                pass
            self._device = None
        # Remove symlink
        link = Path(f"/dev/input/by-id/{self._name}")
        if link.is_symlink():
            try:
                link.unlink()
            except Exception:
                pass

    @property
    def path(self) -> str:
        return self._path

    def key_event(self, keycode: int, down: bool) -> None:
        """Send a key press/release. keycode is Linux evdev keycode."""
        if not self._device:
            return
        try:
            self._device.write(ecodes.EV_KEY, keycode, 1 if down else 0)
            self._device.syn()
        except Exception as e:
            log.debug("Key event failed: %s", e)

    def _create_symlink(self) -> None:
        """Create /dev/input/by-id/ozma-kbd-NAME symlink for stable QEMU reference."""
        try:
            link = Path(f"/dev/input/by-id/{self._name}")
            link.parent.mkdir(parents=True, exist_ok=True)
            if link.exists() or link.is_symlink():
                link.unlink()
            link.symlink_to(self._path)
        except Exception as e:
            log.debug("Symlink creation failed (non-fatal): %s", e)


class VirtualMouse:
    """
    Virtual mouse via uinput with both absolute and relative modes.

    Supports:
    - Absolute coordinates (ABS_X/ABS_Y) for desktop use and AI agent clicks
    - Relative movement (REL_X/REL_Y) for gaming (FPS, mouselook, raw input)

    Both modes are available simultaneously — the HID report format determines
    which is used. Games that capture the mouse via raw input will see REL events.
    Desktop use and AI agent control use ABS events for pixel-perfect positioning.
    """

    def __init__(self, name: str = "ozma-mouse",
                 width: int = 32768, height: int = 32768) -> None:
        self._name = name
        self._width = width
        self._height = height
        self._device: Any = None
        self._path: str = ""

    def start(self) -> str | None:
        if not _EVDEV_AVAILABLE:
            return None

        try:
            cap = {
                ecodes.EV_KEY: [
                    ecodes.BTN_LEFT, ecodes.BTN_RIGHT, ecodes.BTN_MIDDLE,
                    ecodes.BTN_SIDE, ecodes.BTN_EXTRA,  # mouse4/mouse5
                ],
                ecodes.EV_ABS: [
                    (ecodes.ABS_X, AbsInfo(0, 0, self._width - 1, 0, 0, 0)),
                    (ecodes.ABS_Y, AbsInfo(0, 0, self._height - 1, 0, 0, 0)),
                ],
                ecodes.EV_REL: [
                    ecodes.REL_X,
                    ecodes.REL_Y,
                    ecodes.REL_WHEEL,
                    ecodes.REL_HWHEEL,  # horizontal scroll
                ],
            }

            self._device = UInput(
                cap,
                name=self._name,
                vendor=0x1209,
                product=0x0002,
                version=1,
            )
            self._path = self._device.device.path
            log.info("Virtual mouse created: %s → %s (abs+rel)", self._name, self._path)

            self._create_symlink()
            return self._path
        except Exception as e:
            log.error("Failed to create virtual mouse: %s", e)
            return None

    def stop(self) -> None:
        if self._device:
            try:
                self._device.close()
            except Exception:
                pass
            self._device = None
        link = Path(f"/dev/input/by-id/{self._name}")
        if link.is_symlink():
            try:
                link.unlink()
            except Exception:
                pass

    @property
    def path(self) -> str:
        return self._path

    def move(self, x: int, y: int) -> None:
        """Move to absolute coordinates (0-32767 range). Desktop / AI agent mode."""
        if not self._device:
            return
        try:
            self._device.write(ecodes.EV_ABS, ecodes.ABS_X, x)
            self._device.write(ecodes.EV_ABS, ecodes.ABS_Y, y)
            self._device.syn()
        except Exception as e:
            log.debug("Mouse move failed: %s", e)

    def move_relative(self, dx: int, dy: int) -> None:
        """Move by relative delta. Gaming / raw input mode."""
        if not self._device:
            return
        try:
            if dx:
                self._device.write(ecodes.EV_REL, ecodes.REL_X, dx)
            if dy:
                self._device.write(ecodes.EV_REL, ecodes.REL_Y, dy)
            if dx or dy:
                self._device.syn()
        except Exception as e:
            log.debug("Mouse move_relative failed: %s", e)

    def button(self, button: int, down: bool) -> None:
        """Press/release a mouse button. button: bitmask (1=L, 2=R, 4=M, 8=side, 16=extra)."""
        if not self._device:
            return
        try:
            btn = {
                1: ecodes.BTN_LEFT, 2: ecodes.BTN_RIGHT, 4: ecodes.BTN_MIDDLE,
                8: ecodes.BTN_SIDE, 16: ecodes.BTN_EXTRA,
            }.get(button, ecodes.BTN_LEFT)
            self._device.write(ecodes.EV_KEY, btn, 1 if down else 0)
            self._device.syn()
        except Exception as e:
            log.debug("Mouse button failed: %s", e)

    def scroll(self, amount: int) -> None:
        """Vertical scroll. Positive = up, negative = down."""
        if not self._device:
            return
        try:
            self._device.write(ecodes.EV_REL, ecodes.REL_WHEEL, amount)
            self._device.syn()
        except Exception as e:
            log.debug("Mouse scroll failed: %s", e)

    def scroll_horizontal(self, amount: int) -> None:
        """Horizontal scroll. Positive = right, negative = left."""
        if not self._device:
            return
        try:
            self._device.write(ecodes.EV_REL, ecodes.REL_HWHEEL, amount)
            self._device.syn()
        except Exception as e:
            log.debug("Mouse hscroll failed: %s", e)

    def click(self, x: int, y: int, button: int = 1) -> None:
        """Move + click in one operation (absolute mode)."""
        self.move(x, y)
        self.button(button, True)
        self.button(button, False)

    def _create_symlink(self) -> None:
        try:
            link = Path(f"/dev/input/by-id/{self._name}")
            link.parent.mkdir(parents=True, exist_ok=True)
            if link.exists() or link.is_symlink():
                link.unlink()
            link.symlink_to(self._path)
        except Exception as e:
            log.debug("Symlink creation failed (non-fatal): %s", e)


# ── HID report → evdev translation ──────────────────────────────────────────

# HID keyboard usage ID → Linux evdev keycode
_HID_TO_EVDEV: dict[int, int] = {
    0x04: 30,   # a
    0x05: 48,   # b
    0x06: 46,   # c
    0x07: 32,   # d
    0x08: 18,   # e
    0x09: 33,   # f
    0x0A: 34,   # g
    0x0B: 35,   # h
    0x0C: 23,   # i
    0x0D: 36,   # j
    0x0E: 37,   # k
    0x0F: 38,   # l
    0x10: 50,   # m
    0x11: 49,   # n
    0x12: 24,   # o
    0x13: 25,   # p
    0x14: 16,   # q
    0x15: 19,   # r
    0x16: 31,   # s
    0x17: 20,   # t
    0x18: 22,   # u
    0x19: 47,   # v
    0x1A: 17,   # w
    0x1B: 45,   # x
    0x1C: 21,   # y
    0x1D: 44,   # z
    0x1E: 2,    # 1
    0x1F: 3,    # 2
    0x20: 4,    # 3
    0x21: 5,    # 4
    0x22: 6,    # 5
    0x23: 7,    # 6
    0x24: 8,    # 7
    0x25: 9,    # 8
    0x26: 10,   # 9
    0x27: 11,   # 0
    0x28: 28,   # Enter
    0x29: 1,    # Escape
    0x2A: 14,   # Backspace
    0x2B: 15,   # Tab
    0x2C: 57,   # Space
    0x2D: 12,   # -
    0x2E: 13,   # =
    0x2F: 26,   # [
    0x30: 27,   # ]
    0x31: 43,   # backslash
    0x33: 39,   # ;
    0x34: 40,   # '
    0x35: 41,   # `
    0x36: 51,   # ,
    0x37: 52,   # .
    0x38: 53,   # /
    0x39: 58,   # CapsLock
    0x3A: 59,   # F1
    0x3B: 60,   # F2
    0x3C: 61,   # F3
    0x3D: 62,   # F4
    0x3E: 63,   # F5
    0x3F: 64,   # F6
    0x40: 65,   # F7
    0x41: 66,   # F8
    0x42: 67,   # F9
    0x43: 68,   # F10
    0x44: 87,   # F11
    0x45: 88,   # F12
    0x46: 99,   # PrintScreen
    0x47: 70,   # ScrollLock
    0x48: 119,  # Pause
    0x49: 110,  # Insert
    0x4A: 102,  # Home
    0x4B: 104,  # PageUp
    0x4C: 111,  # Delete
    0x4D: 107,  # End
    0x4E: 109,  # PageDown
    0x4F: 106,  # Right
    0x50: 105,  # Left
    0x51: 108,  # Down
    0x52: 103,  # Up
    0x53: 69,   # NumLock
    0x65: 127,  # Menu/Compose
}

# HID modifier bit → evdev keycode
_HID_MOD_TO_EVDEV: dict[int, int] = {
    0x01: 29,   # Left Ctrl
    0x02: 42,   # Left Shift
    0x04: 56,   # Left Alt
    0x08: 125,  # Left Meta (Win)
    0x10: 97,   # Right Ctrl
    0x20: 54,   # Right Shift
    0x40: 100,  # Right Alt
    0x80: 126,  # Right Meta
}


def hid_keyboard_to_evdev(report: bytes) -> list[tuple[int, bool]]:
    """
    Convert HID keyboard boot report (8 bytes) to evdev key events.

    Returns list of (evdev_keycode, down) tuples.
    This is stateless — caller must diff against previous report.
    """
    if len(report) < 8:
        return []

    modifier = report[0]
    keys = [k for k in report[2:8] if k != 0]

    events = []

    # Modifier keys
    for bit, evcode in _HID_MOD_TO_EVDEV.items():
        if modifier & bit:
            events.append((evcode, True))

    # Regular keys
    for hid_code in keys:
        evcode = _HID_TO_EVDEV.get(hid_code)
        if evcode:
            events.append((evcode, True))

    return events


class EvdevHIDTranslator:
    """
    Stateful translator: HID boot reports → evdev events with proper key up/down tracking.
    """

    def __init__(self, keyboard: VirtualKeyboard, mouse: VirtualMouse) -> None:
        self._kbd = keyboard
        self._mouse = mouse
        self._prev_modifier = 0
        self._prev_keys: set[int] = set()
        self._prev_buttons: int = 0

    def handle_keyboard(self, report: bytes) -> None:
        """Process an 8-byte HID keyboard boot report."""
        if len(report) < 8:
            return

        modifier = report[0]
        keys = {k for k in report[2:8] if k != 0}

        # Modifier diff
        for bit, evcode in _HID_MOD_TO_EVDEV.items():
            was_pressed = bool(self._prev_modifier & bit)
            is_pressed = bool(modifier & bit)
            if is_pressed and not was_pressed:
                self._kbd.key_event(evcode, True)
            elif not is_pressed and was_pressed:
                self._kbd.key_event(evcode, False)

        # Key diff
        released = self._prev_keys - keys
        pressed = keys - self._prev_keys

        for hid_code in released:
            evcode = _HID_TO_EVDEV.get(hid_code)
            if evcode:
                self._kbd.key_event(evcode, False)

        for hid_code in pressed:
            evcode = _HID_TO_EVDEV.get(hid_code)
            if evcode:
                self._kbd.key_event(evcode, True)

        self._prev_modifier = modifier
        self._prev_keys = keys

    def handle_mouse(self, report: bytes) -> None:
        """Process a 6-byte HID mouse report (absolute coords)."""
        if len(report) < 5:
            return

        buttons = report[0]
        x = report[1] | (report[2] << 8)
        y = report[3] | (report[4] << 8)
        scroll = report[5] if len(report) > 5 else 0

        # Move (absolute)
        self._mouse.move(x, y)

        # Buttons
        self._update_buttons(buttons)

        # Scroll
        if scroll:
            val = scroll if scroll < 128 else scroll - 256
            self._mouse.scroll(val)

    def handle_mouse_relative(self, report: bytes) -> None:
        """Process a relative mouse report (gaming mode).

        Format: [buttons, dx_lo, dx_hi, dy_lo, dy_hi, scroll]
        dx/dy are signed 16-bit little-endian deltas.
        """
        if len(report) < 5:
            return

        buttons = report[0]
        dx = struct.unpack_from("<h", report, 1)[0]
        dy = struct.unpack_from("<h", report, 3)[0]
        scroll = report[5] if len(report) > 5 else 0

        # Move (relative)
        self._mouse.move_relative(dx, dy)

        # Buttons
        self._update_buttons(buttons)

        # Scroll
        if scroll:
            val = scroll if scroll < 128 else scroll - 256
            self._mouse.scroll(val)

    def _update_buttons(self, buttons: int) -> None:
        """Diff button state and emit press/release events."""
        prev = self._prev_buttons
        for bit in (1, 2, 4, 8, 16):
            was = bool(prev & bit)
            now = bool(buttons & bit)
            if now and not was:
                self._mouse.button(bit, True)
            elif was and not now:
                self._mouse.button(bit, False)
        self._prev_buttons = buttons
