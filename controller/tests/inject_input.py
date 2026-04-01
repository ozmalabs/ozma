#!/usr/bin/env python3
"""
UDP input injector for Ozma Controller testing.

Sends tinynode wire-format packets directly to a target host:port.
No kernel input devices — nothing reaches your display server.

Wire format:
  Keyboard: 0x01 + 8-byte HID boot report [modifier, 0x00, key1..key6]
  Mouse:    0x02 + 6-byte report [buttons, x_lo, x_hi, y_lo, y_hi, scroll]

Usage:
  # Automated demo sequence (types 'hello', moves mouse, clicks):
  python tests/inject_input.py

  # Interactive mode:
  python tests/inject_input.py --interactive

  # Target a specific host (default: 127.0.0.1:7331):
  python tests/inject_input.py --host 10.200.0.5 --port 7331

Interactive commands:
  key <KEY_NAME>        press and release, e.g.  key A  or  key ENTER
  type <string>         type a string
  mouse <x> <y>         move to absolute coords (0–32767)
  click [left|right|middle]
  scroll <N>            +up / -down
  quit
"""

import argparse
import socket
import struct
import sys
import time

# HID Usage IDs (USB HID keyboard page 0x07)
HID: dict[str, int] = {
    "A": 0x04, "B": 0x05, "C": 0x06, "D": 0x07, "E": 0x08, "F": 0x09,
    "G": 0x0A, "H": 0x0B, "I": 0x0C, "J": 0x0D, "K": 0x0E, "L": 0x0F,
    "M": 0x10, "N": 0x11, "O": 0x12, "P": 0x13, "Q": 0x14, "R": 0x15,
    "S": 0x16, "T": 0x17, "U": 0x18, "V": 0x19, "W": 0x1A, "X": 0x1B,
    "Y": 0x1C, "Z": 0x1D,
    "1": 0x1E, "2": 0x1F, "3": 0x20, "4": 0x21, "5": 0x22,
    "6": 0x23, "7": 0x24, "8": 0x25, "9": 0x26, "0": 0x27,
    "ENTER": 0x28, "ESC": 0x29, "BACKSPACE": 0x2A, "TAB": 0x2B,
    "SPACE": 0x2C, "MINUS": 0x2D, "EQUAL": 0x2E,
    "F1": 0x3A, "F2": 0x3B, "F3": 0x3C, "F4": 0x3D,
    "F5": 0x3E, "F6": 0x3F, "F7": 0x40, "F8": 0x41,
    "F9": 0x42, "F10": 0x43, "F11": 0x44, "F12": 0x45,
    "RIGHT": 0x4F, "LEFT": 0x50, "DOWN": 0x51, "UP": 0x52,
    "DELETE": 0x4C, "INSERT": 0x49, "HOME": 0x4A, "END": 0x4D,
    "PAGEUP": 0x4B, "PAGEDOWN": 0x4E,
}

# Modifier bit → HID Usage ID pairs
MODS: dict[str, tuple[int, int]] = {
    "LCTRL":  (0x01, 0xE0), "LSHIFT": (0x02, 0xE1),
    "LALT":   (0x04, 0xE2), "LGUI":   (0x08, 0xE3),
    "RCTRL":  (0x10, 0xE4), "RSHIFT": (0x20, 0xE5),
    "RALT":   (0x40, 0xE6), "RGUI":   (0x80, 0xE7),
}

# ASCII → (hid_usage, shift_required)
ASCII_MAP: dict[str, tuple[int, bool]] = {
    **{c.lower(): (HID[c], False) for c in "ABCDEFGHIJKLMNOPQRSTUVWXYZ"},
    **{c.upper(): (HID[c], True)  for c in "ABCDEFGHIJKLMNOPQRSTUVWXYZ"},
    **{str(i): (HID[str(i)], False) for i in range(10)},
    " ": (HID["SPACE"], False), "\n": (HID["ENTER"], False),
    "\t": (HID["TAB"], False),
    "!": (0x1E, True), "@": (0x1F, True), "#": (0x20, True),
    "$": (0x21, True), "%": (0x22, True), "^": (0x23, True),
    "&": (0x24, True), "*": (0x25, True), "(": (0x26, True),
    ")": (0x27, True), "-": (0x2D, False), "_": (0x2D, True),
    "=": (0x2E, False), "+": (0x2E, True), ".": (0x37, False),
    ",": (0x36, False), "/": (0x38, False), "?": (0x38, True),
}

ABSOLUTE_MAX = 32767


class Injector:
    def __init__(self, host: str, port: int) -> None:
        self._addr = (host, port)
        self._sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self._mouse_x = ABSOLUTE_MAX // 2
        self._mouse_y = ABSOLUTE_MAX // 2

    def _send(self, ptype: int, payload: bytes) -> None:
        self._sock.sendto(bytes([ptype]) + payload, self._addr)

    # --- Keyboard ---

    def _kbd_report(self, modifier: int, *keys: int) -> bytes:
        report = [modifier, 0x00] + list(keys) + [0x00] * 6
        return bytes(report[:8])

    def key_down(self, name: str, modifier: int = 0) -> None:
        hid = HID.get(name.upper())
        if hid is None:
            print(f"Unknown key: {name}")
            return
        self._send(0x01, self._kbd_report(modifier, hid))

    def key_up(self) -> None:
        self._send(0x01, self._kbd_report(0))

    def tap(self, name: str, modifier: int = 0, delay: float = 0.05) -> None:
        self.key_down(name, modifier)
        time.sleep(delay)
        self.key_up()
        time.sleep(delay)

    def type_string(self, text: str, delay: float = 0.04) -> None:
        for ch in text:
            entry = ASCII_MAP.get(ch)
            if entry is None:
                continue
            hid, shift = entry
            # Find key name from HID usage
            key_name = next((k for k, v in HID.items() if v == hid), None)
            if key_name is None:
                continue
            mod = 0x02 if shift else 0  # LShift
            self.tap(key_name, modifier=mod, delay=delay)

    # --- Mouse ---

    def _mouse_report(self, buttons: int = 0, scroll: int = 0) -> bytes:
        x, y = self._mouse_x, self._mouse_y
        return bytes([
            buttons,
            x & 0xFF, (x >> 8) & 0xFF,
            y & 0xFF, (y >> 8) & 0xFF,
            scroll & 0xFF,
        ])

    def mouse_move(self, x: int, y: int) -> None:
        self._mouse_x = max(0, min(ABSOLUTE_MAX, x))
        self._mouse_y = max(0, min(ABSOLUTE_MAX, y))
        self._send(0x02, self._mouse_report())

    def click(self, button: str = "left", delay: float = 0.05) -> None:
        btn_map = {"left": 0x01, "right": 0x02, "middle": 0x04}
        bits = btn_map.get(button.lower(), 0x01)
        self._send(0x02, self._mouse_report(buttons=bits))
        time.sleep(delay)
        self._send(0x02, self._mouse_report(buttons=0))

    def scroll(self, clicks: int) -> None:
        self._send(0x02, self._mouse_report(scroll=clicks))

    def close(self) -> None:
        self._sock.close()


def demo_sequence(inj: Injector) -> None:
    print("[inject] Running demo sequence...")
    time.sleep(0.3)

    print("[inject] Step 1: Type 'hello'")
    inj.type_string("hello")
    time.sleep(0.2)

    print("[inject] Step 2: Press Enter")
    inj.tap("ENTER")
    time.sleep(0.2)

    print("[inject] Step 3: Ctrl+C")
    inj.tap("C", modifier=0x01)  # LCtrl
    time.sleep(0.2)

    print("[inject] Step 4: Mouse sweep across screen")
    for x in range(1000, 30000, 1500):
        inj.mouse_move(x, ABSOLUTE_MAX // 2)
        time.sleep(0.03)
    time.sleep(0.2)

    print("[inject] Step 5: Left click, then right click")
    inj.mouse_move(ABSOLUTE_MAX // 2, ABSOLUTE_MAX // 2)
    inj.click("left")
    time.sleep(0.15)
    inj.click("right")
    time.sleep(0.2)

    print("[inject] Step 6: Scroll")
    inj.scroll(3)
    time.sleep(0.1)
    inj.scroll(-2)

    print("[inject] Demo complete.")


def interactive_mode(inj: Injector) -> None:
    print("\n[inject] Interactive mode. Commands:")
    print("  key <NAME>         — tap a key, e.g.  key A  key ENTER  key F5")
    print("  ctrl+<NAME>        — e.g.  ctrl+c  ctrl+z")
    print("  type <string>      — type text")
    print("  mouse <x> <y>      — move (0–32767)")
    print("  click [left|right|middle]")
    print("  scroll <N>")
    print("  quit")
    print()

    while True:
        try:
            line = input("> ").strip()
        except (EOFError, KeyboardInterrupt):
            break
        if not line:
            continue
        parts = line.split(None, 1)
        cmd = parts[0].lower()

        if cmd == "quit":
            break
        elif cmd == "key" and len(parts) == 2:
            inj.tap(parts[1].strip().upper())
        elif "+" in cmd and cmd.startswith("ctrl"):
            key = cmd.split("+", 1)[1].upper()
            inj.tap(key, modifier=0x01)
        elif cmd == "type" and len(parts) == 2:
            inj.type_string(parts[1])
        elif cmd == "mouse":
            coords = parts[1].split() if len(parts) == 2 else []
            if len(coords) == 2:
                try:
                    inj.mouse_move(int(coords[0]), int(coords[1]))
                except ValueError:
                    print("Usage: mouse <x> <y>  (0–32767)")
            else:
                print("Usage: mouse <x> <y>")
        elif cmd == "click":
            btn = parts[1].strip() if len(parts) == 2 else "left"
            inj.click(btn)
        elif cmd == "scroll" and len(parts) == 2:
            try:
                inj.scroll(int(parts[1]))
            except ValueError:
                print("Usage: scroll <N>")
        else:
            print(f"Unknown command: {line}")


def main() -> None:
    p = argparse.ArgumentParser(description="Ozma UDP input injector")
    p.add_argument("--host", default="127.0.0.1", help="Target host (default: 127.0.0.1)")
    p.add_argument("--port", type=int, default=7331, help="Target port (default: 7331)")
    p.add_argument("--interactive", "-i", action="store_true")
    args = p.parse_args()

    print(f"[inject] Sending to {args.host}:{args.port}")
    inj = Injector(args.host, args.port)

    try:
        if args.interactive:
            interactive_mode(inj)
        else:
            demo_sequence(inj)
    finally:
        inj.close()


if __name__ == "__main__":
    main()
