# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Paste-as-typing — send text to the active machine via HID keystrokes.

Converts UTF-8 text into a sequence of HID keyboard reports and sends
them to the active node's UDP port at a controlled rate.  This is
essential for KVM use where the target machine has no clipboard access:

  - Type a password into a BIOS setup screen
  - Paste an IP address or command into a Linux console at boot
  - Send OCR'd text from one machine's screen to another
  - Type long commands without physical keyboard access

Keyboard layouts:
  Different keyboard layouts produce different characters for the same
  physical key.  The layout maps define which HID key + modifiers produce
  each character.  US layout is default; UK, DE, FR, etc. are supported.

Rate limiting:
  BIOS and UEFI input handlers are often slow.  Characters sent too fast
  are dropped.  Default rate: 30 chars/sec (33ms between keystrokes).
  Configurable down to 5 chars/sec for very slow targets.

Wire format:
  Uses the same UDP HID protocol as normal keyboard input:
    [0x01, modifier, 0x00, key, 0, 0, 0, 0, 0]  (key down)
    [0x01, 0x00, 0x00, 0, 0, 0, 0, 0, 0]          (all keys up)
"""

from __future__ import annotations

import asyncio
import logging
import socket
from dataclasses import dataclass
from typing import Any

log = logging.getLogger("ozma.paste_typing")

# HID modifier bits
MOD_NONE = 0x00
MOD_LSHIFT = 0x02
MOD_RALT = 0x40     # AltGr (used in non-US layouts)

# HID usage IDs for keys
HID_KEYS = {
    "a": 0x04, "b": 0x05, "c": 0x06, "d": 0x07, "e": 0x08, "f": 0x09,
    "g": 0x0A, "h": 0x0B, "i": 0x0C, "j": 0x0D, "k": 0x0E, "l": 0x0F,
    "m": 0x10, "n": 0x11, "o": 0x12, "p": 0x13, "q": 0x14, "r": 0x15,
    "s": 0x16, "t": 0x17, "u": 0x18, "v": 0x19, "w": 0x1A, "x": 0x1B,
    "y": 0x1C, "z": 0x1D,
    "1": 0x1E, "2": 0x1F, "3": 0x20, "4": 0x21, "5": 0x22, "6": 0x23,
    "7": 0x24, "8": 0x25, "9": 0x26, "0": 0x27,
    "enter": 0x28, "esc": 0x29, "backspace": 0x2A, "tab": 0x2B,
    "space": 0x2C, "minus": 0x2D, "equal": 0x2E,
    "lbracket": 0x2F, "rbracket": 0x30, "backslash": 0x31,
    "semicolon": 0x33, "quote": 0x34, "grave": 0x35,
    "comma": 0x36, "period": 0x37, "slash": 0x38,
    "f1": 0x3A, "f2": 0x3B, "f3": 0x3C, "f4": 0x3D,
    "f5": 0x3E, "f6": 0x3F, "f7": 0x40, "f8": 0x41,
    "f9": 0x42, "f10": 0x43, "f11": 0x44, "f12": 0x45,
}


@dataclass
class KeyStroke:
    """A single HID keystroke: modifier + key usage ID."""
    modifier: int
    key: int


# ── Keyboard layout maps ────────────────────────────────────────────────────
# Each layout maps a character to (modifier, hid_key).
# Only characters that differ from unshifted need entries.

def _build_us_layout() -> dict[str, KeyStroke]:
    """US QWERTY layout — character → KeyStroke."""
    layout: dict[str, KeyStroke] = {}

    # Lowercase letters (no modifier)
    for c in "abcdefghijklmnopqrstuvwxyz":
        layout[c] = KeyStroke(MOD_NONE, HID_KEYS[c])

    # Uppercase letters (shift)
    for c in "abcdefghijklmnopqrstuvwxyz":
        layout[c.upper()] = KeyStroke(MOD_LSHIFT, HID_KEYS[c])

    # Digits
    for c in "1234567890":
        layout[c] = KeyStroke(MOD_NONE, HID_KEYS[c])

    # Shift+digits → symbols
    shift_digits = {
        "!": "1", "@": "2", "#": "3", "$": "4", "%": "5",
        "^": "6", "&": "7", "*": "8", "(": "9", ")": "0",
    }
    for sym, digit in shift_digits.items():
        layout[sym] = KeyStroke(MOD_LSHIFT, HID_KEYS[digit])

    # Unshifted punctuation
    layout[" "] = KeyStroke(MOD_NONE, HID_KEYS["space"])
    layout["-"] = KeyStroke(MOD_NONE, HID_KEYS["minus"])
    layout["="] = KeyStroke(MOD_NONE, HID_KEYS["equal"])
    layout["["] = KeyStroke(MOD_NONE, HID_KEYS["lbracket"])
    layout["]"] = KeyStroke(MOD_NONE, HID_KEYS["rbracket"])
    layout["\\"] = KeyStroke(MOD_NONE, HID_KEYS["backslash"])
    layout[";"] = KeyStroke(MOD_NONE, HID_KEYS["semicolon"])
    layout["'"] = KeyStroke(MOD_NONE, HID_KEYS["quote"])
    layout["`"] = KeyStroke(MOD_NONE, HID_KEYS["grave"])
    layout[","] = KeyStroke(MOD_NONE, HID_KEYS["comma"])
    layout["."] = KeyStroke(MOD_NONE, HID_KEYS["period"])
    layout["/"] = KeyStroke(MOD_NONE, HID_KEYS["slash"])

    # Shifted punctuation
    layout["_"] = KeyStroke(MOD_LSHIFT, HID_KEYS["minus"])
    layout["+"] = KeyStroke(MOD_LSHIFT, HID_KEYS["equal"])
    layout["{"] = KeyStroke(MOD_LSHIFT, HID_KEYS["lbracket"])
    layout["}"] = KeyStroke(MOD_LSHIFT, HID_KEYS["rbracket"])
    layout["|"] = KeyStroke(MOD_LSHIFT, HID_KEYS["backslash"])
    layout[":"] = KeyStroke(MOD_LSHIFT, HID_KEYS["semicolon"])
    layout['"'] = KeyStroke(MOD_LSHIFT, HID_KEYS["quote"])
    layout["~"] = KeyStroke(MOD_LSHIFT, HID_KEYS["grave"])
    layout["<"] = KeyStroke(MOD_LSHIFT, HID_KEYS["comma"])
    layout[">"] = KeyStroke(MOD_LSHIFT, HID_KEYS["period"])
    layout["?"] = KeyStroke(MOD_LSHIFT, HID_KEYS["slash"])

    # Special keys
    layout["\n"] = KeyStroke(MOD_NONE, HID_KEYS["enter"])
    layout["\t"] = KeyStroke(MOD_NONE, HID_KEYS["tab"])

    return layout


def _build_uk_layout() -> dict[str, KeyStroke]:
    """UK QWERTY — differences from US."""
    layout = _build_us_layout()
    # UK differences
    layout['"'] = KeyStroke(MOD_LSHIFT, HID_KEYS["2"])      # Shift+2 = "
    layout["@"] = KeyStroke(MOD_LSHIFT, HID_KEYS["quote"])   # Shift+' = @
    layout["£"] = KeyStroke(MOD_LSHIFT, HID_KEYS["3"])       # Shift+3 = £
    layout["#"] = KeyStroke(MOD_NONE, 0x32)                   # Non-US hash key
    layout["~"] = KeyStroke(MOD_LSHIFT, 0x32)                 # Shift+hash = ~
    layout["\\"] = KeyStroke(MOD_NONE, 0x64)                  # Non-US backslash
    layout["|"] = KeyStroke(MOD_LSHIFT, 0x64)
    return layout


def _build_de_layout() -> dict[str, KeyStroke]:
    """German QWERTZ layout."""
    layout = _build_us_layout()
    # Z and Y swapped
    layout["z"] = KeyStroke(MOD_NONE, HID_KEYS["y"])
    layout["y"] = KeyStroke(MOD_NONE, HID_KEYS["z"])
    layout["Z"] = KeyStroke(MOD_LSHIFT, HID_KEYS["y"])
    layout["Y"] = KeyStroke(MOD_LSHIFT, HID_KEYS["z"])
    # German-specific via AltGr
    layout["@"] = KeyStroke(MOD_RALT, HID_KEYS["q"])
    layout["€"] = KeyStroke(MOD_RALT, HID_KEYS["e"])
    layout["{"] = KeyStroke(MOD_RALT, HID_KEYS["7"])
    layout["}"] = KeyStroke(MOD_RALT, HID_KEYS["0"])
    layout["["] = KeyStroke(MOD_RALT, HID_KEYS["8"])
    layout["]"] = KeyStroke(MOD_RALT, HID_KEYS["9"])
    layout["\\"] = KeyStroke(MOD_RALT, HID_KEYS["minus"])
    layout["|"] = KeyStroke(MOD_RALT, 0x64)
    layout["~"] = KeyStroke(MOD_RALT, HID_KEYS["rbracket"])
    return layout


LAYOUTS: dict[str, dict[str, KeyStroke]] = {
    "us": _build_us_layout(),
    "uk": _build_uk_layout(),
    "de": _build_de_layout(),
}


# ── Paste typing engine ──────────────────────────────────────────────────────

class PasteTyper:
    """
    Sends text to the active node as HID keystrokes.

    Usage::

        typer = PasteTyper(state)
        await typer.type_text("Hello, World!\\n", layout="us", rate=30)
    """

    def __init__(self, state: Any) -> None:
        self._state = state
        self._sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self._typing = False

    @property
    def is_typing(self) -> bool:
        return self._typing

    async def type_text(
        self,
        text: str,
        layout: str = "us",
        rate: float = 30.0,
        node_id: str | None = None,
    ) -> dict[str, Any]:
        """
        Type text to the active (or specified) node via HID keystrokes.

        Args:
            text: Text to type
            layout: Keyboard layout ("us", "uk", "de")
            rate: Characters per second (5-100, default 30)
            node_id: Target node (None = active node)

        Returns:
            {"ok": True, "chars_sent": N, "chars_skipped": N}
        """
        node = self._state.nodes.get(node_id) if node_id else self._state.get_active_node()
        if not node:
            return {"ok": False, "error": "No active node"}

        keymap = LAYOUTS.get(layout, LAYOUTS["us"])
        delay = 1.0 / max(min(rate, 100), 5)
        self._typing = True

        chars_sent = 0
        chars_skipped = 0

        try:
            for char in text:
                stroke = keymap.get(char)
                if not stroke:
                    chars_skipped += 1
                    continue

                # Key down
                report = bytes([
                    stroke.modifier, 0x00,
                    stroke.key, 0, 0, 0, 0, 0,
                ])
                self._send_hid(node.host, node.port, report)
                await asyncio.sleep(delay * 0.4)

                # Key up (all released)
                release = bytes([0, 0, 0, 0, 0, 0, 0, 0])
                self._send_hid(node.host, node.port, release)
                await asyncio.sleep(delay * 0.6)

                chars_sent += 1
        finally:
            self._typing = False

        return {"ok": True, "chars_sent": chars_sent, "chars_skipped": chars_skipped}

    async def type_key(
        self,
        key: str,
        modifier: int = 0,
        node_id: str | None = None,
    ) -> bool:
        """Send a single named key (e.g., "enter", "f1", "esc")."""
        node = self._state.nodes.get(node_id) if node_id else self._state.get_active_node()
        if not node:
            return False

        hid_key = HID_KEYS.get(key.lower())
        if not hid_key:
            return False

        report = bytes([modifier, 0x00, hid_key, 0, 0, 0, 0, 0])
        self._send_hid(node.host, node.port, report)
        await asyncio.sleep(0.05)
        self._send_hid(node.host, node.port, bytes(8))
        return True

    def _send_hid(self, host: str, port: int, report: bytes) -> None:
        packet = bytes([0x01]) + report  # 0x01 = keyboard packet type
        try:
            self._sock.sendto(packet, (host, port))
        except OSError:
            pass

    @staticmethod
    def available_layouts() -> list[str]:
        return list(LAYOUTS.keys())
