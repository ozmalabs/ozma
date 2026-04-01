# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
HID Usage ID (keyboard page 0x07) → QMP qcode translation.

Also handles:
  - Modifier byte diffing (byte 0 of boot report → individual key events)
  - Key slot diffing (bytes 2–7 of boot report → pressed/released sets)
  - Mouse report decoding → QMP InputMoveEvent + InputBtnEvent

QMP input-send-event reference:
  https://www.qemu.org/docs/master/interop/qemu-qmp-ref.html#qapidoc-2255

QMP qcodes are strings from the QEMU keycode enum. The full list is in
qapi/ui.json in the QEMU source. The subset needed for a full keyboard is here.
"""

from dataclasses import dataclass, field

# ---------------------------------------------------------------------------
# HID Usage ID → QMP qcode
# ---------------------------------------------------------------------------

HID_TO_QCODE: dict[int, str] = {
    # Letters
    0x04: "a",  0x05: "b",  0x06: "c",  0x07: "d",  0x08: "e",
    0x09: "f",  0x0A: "g",  0x0B: "h",  0x0C: "i",  0x0D: "j",
    0x0E: "k",  0x0F: "l",  0x10: "m",  0x11: "n",  0x12: "o",
    0x13: "p",  0x14: "q",  0x15: "r",  0x16: "s",  0x17: "t",
    0x18: "u",  0x19: "v",  0x1A: "w",  0x1B: "x",  0x1C: "y",
    0x1D: "z",
    # Numbers
    0x1E: "1",  0x1F: "2",  0x20: "3",  0x21: "4",  0x22: "5",
    0x23: "6",  0x24: "7",  0x25: "8",  0x26: "9",  0x27: "0",
    # Control keys
    0x28: "ret",        # Enter
    0x29: "esc",
    0x2A: "backspace",
    0x2B: "tab",
    0x2C: "spc",        # Space
    0x2D: "minus",
    0x2E: "equal",
    0x2F: "bracket_left",
    0x30: "bracket_right",
    0x31: "backslash",
    0x33: "semicolon",
    0x34: "apostrophe",
    0x35: "grave_accent",
    0x36: "comma",
    0x37: "dot",
    0x38: "slash",
    0x39: "caps_lock",
    # Function keys
    0x3A: "f1",   0x3B: "f2",   0x3C: "f3",   0x3D: "f4",
    0x3E: "f5",   0x3F: "f6",   0x40: "f7",   0x41: "f8",
    0x42: "f9",   0x43: "f10",  0x44: "f11",  0x45: "f12",
    # System / nav
    0x46: "print",
    0x47: "scroll_lock",
    0x48: "pause",
    0x49: "insert",
    0x4A: "home",
    0x4B: "pgup",
    0x4C: "delete",
    0x4D: "end",
    0x4E: "pgdn",
    0x4F: "right",
    0x50: "left",
    0x51: "down",
    0x52: "up",
    # Numpad
    0x53: "num_lock",
    0x54: "kp_divide",
    0x55: "kp_multiply",
    0x56: "kp_subtract",
    0x57: "kp_add",
    0x58: "kp_enter",
    0x59: "kp_1",  0x5A: "kp_2",  0x5B: "kp_3",
    0x5C: "kp_4",  0x5D: "kp_5",  0x5E: "kp_6",
    0x5F: "kp_7",  0x60: "kp_8",  0x61: "kp_9",
    0x62: "kp_0",  0x63: "kp_decimal",
    # Misc
    0x64: "less",           # 102nd key (non-US backslash)
    0x65: "compose",
    0x66: "power",
    0x67: "kp_equals",
    0x68: "f13",  0x69: "f14",  0x6A: "f15",  0x6B: "f16",
    0x6C: "f17",  0x6D: "f18",  0x6E: "f19",  0x6F: "f20",
    0x70: "f21",  0x71: "f22",  0x72: "f23",  0x73: "f24",
    0x7F: "audmute",
    0x80: "volinc",
    0x81: "voldec",
    # Modifier Usage IDs (byte 0 bits map to these separately)
    0xE0: "ctrl",
    0xE1: "shift",
    0xE2: "alt",
    0xE3: "meta_l",
    0xE4: "ctrl_r",
    0xE5: "shift_r",
    0xE6: "alt_r",
    0xE7: "meta_r",
}

# Modifier byte bit index → HID Usage ID
_MODIFIER_BITS: list[int] = [0xE0, 0xE1, 0xE2, 0xE3, 0xE4, 0xE5, 0xE6, 0xE7]

# Mouse button bit → QMP button name
_MOUSE_BUTTONS: dict[int, str] = {
    0: "left",
    1: "right",
    2: "middle",
}


# ---------------------------------------------------------------------------
# QMP event builders
# ---------------------------------------------------------------------------

def _key_event(qcode: str, down: bool) -> dict:
    return {
        "type": "key",
        "data": {"down": down, "key": {"type": "qcode", "data": qcode}},
    }


def _btn_event(button: str, down: bool) -> dict:
    return {
        "type": "btn",
        "data": {"down": down, "button": button},
    }


def _abs_event(axis: str, value: int) -> dict:
    return {
        "type": "abs",
        "data": {"axis": axis, "value": value},
    }


# ---------------------------------------------------------------------------
# Boot report state machine
# ---------------------------------------------------------------------------

@dataclass
class KeyboardReportState:
    """
    Shadows the last HID boot report and diffs each new report against it,
    producing a list of QMP events representing only the changes.

    The HID boot report is 8 bytes:
      [0] modifier bitmask
      [1] reserved (ignored)
      [2..7] up to 6 key usage IDs (0x00 = empty slot)
    """
    _prev_modifiers: int = 0
    _prev_keys: frozenset = field(default_factory=frozenset)

    def diff(self, report: bytes) -> list[dict]:
        """Return QMP events for changes between prev report and this one."""
        if len(report) < 8:
            return []

        events: list[dict] = []
        modifier_byte = report[0]
        current_keys = frozenset(k for k in report[2:8] if k != 0x00)

        # Diff modifier bits
        changed_mods = self._prev_modifiers ^ modifier_byte
        for bit in range(8):
            if changed_mods & (1 << bit):
                hid = _MODIFIER_BITS[bit]
                qcode = HID_TO_QCODE.get(hid)
                if qcode:
                    down = bool(modifier_byte & (1 << bit))
                    events.append(_key_event(qcode, down))

        # Diff key slots (treat as sets — order can change between reports)
        released = self._prev_keys - current_keys
        pressed = current_keys - self._prev_keys

        for hid in sorted(released):
            qcode = HID_TO_QCODE.get(hid)
            if qcode:
                events.append(_key_event(qcode, False))

        for hid in sorted(pressed):
            qcode = HID_TO_QCODE.get(hid)
            if qcode:
                events.append(_key_event(qcode, True))

        self._prev_modifiers = modifier_byte
        self._prev_keys = current_keys
        return events

    def release_all(self) -> list[dict]:
        """Generate key-up events for everything currently held."""
        events: list[dict] = []
        for bit in range(8):
            if self._prev_modifiers & (1 << bit):
                hid = _MODIFIER_BITS[bit]
                qcode = HID_TO_QCODE.get(hid)
                if qcode:
                    events.append(_key_event(qcode, False))
        for hid in sorted(self._prev_keys):
            qcode = HID_TO_QCODE.get(hid)
            if qcode:
                events.append(_key_event(qcode, False))
        self._prev_modifiers = 0
        self._prev_keys = frozenset()
        return events


@dataclass
class MouseReportState:
    """
    Decodes the 6-byte absolute mouse report and produces QMP events.

      [0] buttons bitmask (bit 0=left, 1=right, 2=middle)
      [1..2] X little-endian 0–32767
      [3..4] Y little-endian 0–32767
      [5]    scroll (signed byte)
    """
    _prev_buttons: int = 0

    def decode(self, report: bytes) -> list[dict]:
        if len(report) < 6:
            return []

        events: list[dict] = []
        buttons = report[0]
        x = report[1] | (report[2] << 8)
        y = report[3] | (report[4] << 8)
        scroll = report[5] if report[5] < 128 else report[5] - 256  # signed

        # Absolute position
        events.append(_abs_event("x", x))
        events.append(_abs_event("y", y))

        # Scroll wheel → vertical button events
        if scroll > 0:
            for _ in range(scroll):
                events.append(_btn_event("wheel-up", True))
                events.append(_btn_event("wheel-up", False))
        elif scroll < 0:
            for _ in range(-scroll):
                events.append(_btn_event("wheel-down", True))
                events.append(_btn_event("wheel-down", False))

        # Button diffs
        changed = self._prev_buttons ^ buttons
        for bit, name in _MOUSE_BUTTONS.items():
            if changed & (1 << bit):
                events.append(_btn_event(name, bool(buttons & (1 << bit))))

        self._prev_buttons = buttons
        return events
