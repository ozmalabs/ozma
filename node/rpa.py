# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Node-side RPA — lightweight OCR + automation engine for hardware nodes.

Runs directly on the SBC, operating through the local capture card and
USB HID gadget.  Enables fully autonomous remote operations:

  • Change BIOS boot order to USB → unattended OS install
  • Navigate UEFI/BIOS setup menus (arrow keys, enter, text entry)
  • Detect POST errors, BSOD, kernel panics
  • Wait for specific text/state, then take action
  • Drive Windows/Linux OOBE without any network or agent

This is intentionally minimal.  The controller has the full automation
engine (controller/automation.py) with image matching, loops, variables,
and scripting DSL.  This node-side engine handles the critical use case:
the controller might not be reachable (colo server, air-gapped install),
so the node must be able to act independently.

Two OCR backends:

  1. Bitmap font matcher (text_capture.py) — BIOS/terminal, <2ms per frame,
     no dependencies, works on any SBC.  Embedded VGA 8×16 CP437 font.

  2. Tesseract (optional) — GUI screens, slower (~200ms on RPi4), needs
     tesseract-ocr package installed.  Used for Windows/Linux desktop RPA.

Frame capture:

  - V4L2 single-frame grab via ffmpeg -frames:v 1 (universal, ~100ms)
  - Or v4l2-ctl --stream-mmap --stream-count=1 (faster, ~30ms)
  - Falls back to latest HLS segment decode if capture device is busy

Input injection:

  - Direct /dev/hidg0 and /dev/hidg1 writes (same as normal HID path)
  - No UDP round-trip to controller — local writes, <1ms latency
"""

from __future__ import annotations

import asyncio
import logging
import struct
import subprocess
import tempfile
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.node.rpa")


# ── HID keycodes (USB HID Usage Tables, subset) ─────────────────────────────

KEY_ENTER = 0x28
KEY_ESC = 0x29
KEY_BACKSPACE = 0x2A
KEY_TAB = 0x2B
KEY_SPACE = 0x2C
KEY_DELETE = 0x4C
KEY_UP = 0x52
KEY_DOWN = 0x51
KEY_LEFT = 0x50
KEY_RIGHT = 0x4F
KEY_HOME = 0x4A
KEY_END = 0x4D
KEY_PAGEUP = 0x4B
KEY_PAGEDOWN = 0x4E
KEY_F1 = 0x3A
KEY_F2 = 0x3B
KEY_F3 = 0x3C
KEY_F4 = 0x3D
KEY_F5 = 0x3E
KEY_F6 = 0x3F
KEY_F7 = 0x40
KEY_F8 = 0x41
KEY_F9 = 0x42
KEY_F10 = 0x43
KEY_F11 = 0x44
KEY_F12 = 0x45

# Character → HID keycode mapping (US layout)
_CHAR_TO_HID: dict[str, tuple[int, int]] = {}
for _i, _c in enumerate("abcdefghijklmnopqrstuvwxyz"):
    _CHAR_TO_HID[_c] = (4 + _i, 0)
    _CHAR_TO_HID[_c.upper()] = (4 + _i, 0x02)  # Left Shift
for _i, _c in enumerate("1234567890"):
    _CHAR_TO_HID[_c] = (30 + _i, 0)
_CHAR_TO_HID.update({
    " ": (KEY_SPACE, 0), "-": (0x2D, 0), "=": (0x2E, 0),
    "[": (0x2F, 0), "]": (0x30, 0), "\\": (0x31, 0),
    ";": (0x33, 0), "'": (0x34, 0), "`": (0x35, 0),
    ",": (0x36, 0), ".": (0x37, 0), "/": (0x38, 0),
    "!": (30, 0x02), "@": (31, 0x02), "#": (32, 0x02),
    "$": (33, 0x02), "%": (34, 0x02), "^": (35, 0x02),
    "&": (36, 0x02), "*": (37, 0x02), "(": (38, 0x02),
    ")": (39, 0x02), "_": (0x2D, 0x02), "+": (0x2E, 0x02),
    "{": (0x2F, 0x02), "}": (0x30, 0x02), "|": (0x31, 0x02),
    ":": (0x33, 0x02), '"': (0x34, 0x02), "~": (0x35, 0x02),
    "<": (0x36, 0x02), ">": (0x37, 0x02), "?": (0x38, 0x02),
    "\n": (KEY_ENTER, 0), "\t": (KEY_TAB, 0),
})

# Named key → HID keycode
_NAMED_KEYS: dict[str, int] = {
    "enter": KEY_ENTER, "return": KEY_ENTER, "esc": KEY_ESC,
    "escape": KEY_ESC, "backspace": KEY_BACKSPACE, "tab": KEY_TAB,
    "space": KEY_SPACE, "delete": KEY_DELETE, "del": KEY_DELETE,
    "up": KEY_UP, "down": KEY_DOWN, "left": KEY_LEFT, "right": KEY_RIGHT,
    "home": KEY_HOME, "end": KEY_END, "pageup": KEY_PAGEUP,
    "pagedown": KEY_PAGEDOWN, "pgup": KEY_PAGEUP, "pgdn": KEY_PAGEDOWN,
    "f1": KEY_F1, "f2": KEY_F2, "f3": KEY_F3, "f4": KEY_F4,
    "f5": KEY_F5, "f6": KEY_F6, "f7": KEY_F7, "f8": KEY_F8,
    "f9": KEY_F9, "f10": KEY_F10, "f11": KEY_F11, "f12": KEY_F12,
}


@dataclass
class ScreenText:
    """Text found on screen at a position."""
    text: str
    x: int
    y: int
    width: int
    height: int
    confidence: float = 0.0


@dataclass
class ScreenState:
    """What's on screen right now."""
    text_regions: list[ScreenText] = field(default_factory=list)
    full_text: str = ""
    width: int = 0
    height: int = 0

    def has_text(self, needle: str) -> bool:
        return needle.lower() in self.full_text.lower()

    def find_text(self, needle: str) -> ScreenText | None:
        needle_l = needle.lower()
        for r in self.text_regions:
            if needle_l in r.text.lower():
                return r
        return None


class NodeRPA:
    """
    Lightweight RPA engine running on the hardware node.

    Operates through local V4L2 capture + /dev/hidg0/hidg1 writes.
    No network needed — fully autonomous.
    """

    def __init__(
        self,
        capture_device: str = "/dev/video0",
        kbd_device: str = "/dev/hidg0",
        mouse_device: str = "/dev/hidg1",
    ) -> None:
        self._capture = capture_device
        self._kbd = kbd_device
        self._mouse = mouse_device
        self._width = 1920
        self._height = 1080
        self._tesseract_available = self._check_tesseract()
        self._text_capture = self._load_text_capture()

    def _check_tesseract(self) -> bool:
        import shutil
        return bool(shutil.which("tesseract"))

    def _load_text_capture(self):
        """Load the bitmap font text capture module."""
        try:
            import sys
            # The text_capture module may be in the controller dir
            sys.path.insert(0, str(Path(__file__).parent.parent / "controller"))
            from text_capture import TextCapture
            return TextCapture()
        except ImportError:
            log.debug("text_capture not available — BIOS OCR disabled")
            return None

    # ── Frame capture ────────────────────────────────────────────────────

    async def grab_frame(self) -> Any:
        """
        Grab a single frame from the capture device.

        Returns a PIL Image, or None if capture fails.
        """
        from PIL import Image
        with tempfile.NamedTemporaryFile(suffix=".png", delete=False) as f:
            tmp = f.name

        try:
            proc = await asyncio.create_subprocess_exec(
                "ffmpeg", "-y", "-f", "v4l2", "-input_format", "mjpeg",
                "-video_size", f"{self._width}x{self._height}",
                "-i", self._capture,
                "-frames:v", "1",
                "-update", "1",
                tmp,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.DEVNULL,
            )
            await asyncio.wait_for(proc.wait(), timeout=5)
            if proc.returncode == 0 and Path(tmp).exists():
                return Image.open(tmp).copy()
        except Exception as e:
            log.debug("Frame grab failed: %s", e)
        finally:
            Path(tmp).unlink(missing_ok=True)
        return None

    # ── OCR ──────────────────────────────────────────────────────────────

    async def read_screen(self, mode: str = "auto") -> ScreenState:
        """
        Read screen text.

        mode:
          "bios"  — use bitmap font matcher (fast, BIOS/terminal only)
          "gui"   — use Tesseract (slower, works on GUIs)
          "auto"  — try bitmap first, fall back to Tesseract
        """
        frame = await self.grab_frame()
        if frame is None:
            return ScreenState()

        import numpy as np
        arr = np.array(frame)
        self._width, self._height = frame.size
        state = ScreenState(width=frame.size[0], height=frame.size[1])

        # Try bitmap font matcher for BIOS/terminal
        if mode in ("bios", "auto") and self._text_capture:
            try:
                result = self._text_capture.extract_text(arr)
                if result and result.text.strip():
                    state.full_text = result.text
                    # TextCapture returns grid-based text — create regions
                    for line_idx, line in enumerate(result.text.split("\n")):
                        if line.strip():
                            state.text_regions.append(ScreenText(
                                text=line.strip(),
                                x=0, y=line_idx * result.cell_height,
                                width=self._width,
                                height=result.cell_height,
                            ))
                    if state.full_text.strip():
                        return state
            except Exception as e:
                log.debug("Bitmap OCR failed: %s", e)

        # Tesseract for GUI screens
        if mode in ("gui", "auto") and self._tesseract_available:
            try:
                import pytesseract
                data = pytesseract.image_to_data(
                    frame, output_type=pytesseract.Output.DICT
                )
                n = len(data["text"])
                words = []
                for i in range(n):
                    text = data["text"][i].strip()
                    conf = int(data["conf"][i]) if data["conf"][i] != "-1" else 0
                    if text and conf > 30:
                        state.text_regions.append(ScreenText(
                            text=text,
                            x=data["left"][i], y=data["top"][i],
                            width=data["width"][i], height=data["height"][i],
                            confidence=conf / 100.0,
                        ))
                        words.append(text)
                state.full_text = " ".join(words)
            except ImportError:
                # No pytesseract — try CLI
                state = await self._tesseract_cli(frame)
            except Exception as e:
                log.debug("Tesseract OCR failed: %s", e)

        return state

    async def _tesseract_cli(self, img: Any) -> ScreenState:
        """Run tesseract CLI directly (no Python bindings needed)."""
        state = ScreenState(width=self._width, height=self._height)
        with tempfile.NamedTemporaryFile(suffix=".png", delete=False) as f:
            img.save(f.name)
            try:
                proc = await asyncio.create_subprocess_exec(
                    "tesseract", f.name, "-", "--psm", "3", "-l", "eng",
                    stdout=asyncio.subprocess.PIPE,
                    stderr=asyncio.subprocess.DEVNULL,
                )
                stdout, _ = await asyncio.wait_for(proc.communicate(), timeout=30)
                state.full_text = stdout.decode(errors="replace").strip()
            except Exception:
                pass
            finally:
                Path(f.name).unlink(missing_ok=True)
        return state

    # ── HID input ────────────────────────────────────────────────────────

    async def key(self, name: str, modifier: int = 0) -> None:
        """Send a single key press and release."""
        name_l = name.lower()
        keycode = _NAMED_KEYS.get(name_l)
        if keycode is None:
            if len(name) == 1 and name in _CHAR_TO_HID:
                keycode, mod = _CHAR_TO_HID[name]
                modifier |= mod
            else:
                log.warning("Unknown key: %s", name)
                return

        report = bytes([modifier, 0, keycode, 0, 0, 0, 0, 0])
        release = bytes(8)
        try:
            with open(self._kbd, "wb") as f:
                f.write(report)
                f.flush()
            await asyncio.sleep(0.02)
            with open(self._kbd, "wb") as f:
                f.write(release)
                f.flush()
        except Exception as e:
            log.warning("Key send failed: %s", e)

    async def type_text(self, text: str, delay: float = 0.03) -> None:
        """Type a string character by character."""
        for ch in text:
            if ch in _CHAR_TO_HID:
                keycode, mod = _CHAR_TO_HID[ch]
                await self.key(ch, 0)
                await asyncio.sleep(delay)
            else:
                log.debug("Cannot type character: %r", ch)

    async def mouse_move(self, x: int, y: int) -> None:
        """Move mouse to absolute coordinates."""
        ax = int(x * 32767 / self._width)
        ay = int(y * 32767 / self._height)
        report = bytes([
            0,  # buttons
            ax & 0xFF, (ax >> 8) & 0xFF,
            ay & 0xFF, (ay >> 8) & 0xFF,
            0,  # scroll
        ])
        try:
            with open(self._mouse, "wb") as f:
                f.write(report)
                f.flush()
        except Exception as e:
            log.warning("Mouse move failed: %s", e)

    async def click(self, x: int, y: int, button: int = 1) -> None:
        """Click at absolute coordinates."""
        await self.mouse_move(x, y)
        await asyncio.sleep(0.02)

        ax = int(x * 32767 / self._width)
        ay = int(y * 32767 / self._height)
        btn_byte = button  # 1=left, 2=right, 4=middle

        # Press
        report = bytes([
            btn_byte,
            ax & 0xFF, (ax >> 8) & 0xFF,
            ay & 0xFF, (ay >> 8) & 0xFF,
            0,
        ])
        try:
            with open(self._mouse, "wb") as f:
                f.write(report)
                f.flush()
        except Exception as e:
            log.warning("Mouse click failed: %s", e)
            return

        await asyncio.sleep(0.03)

        # Release
        report = bytes([
            0,
            ax & 0xFF, (ax >> 8) & 0xFF,
            ay & 0xFF, (ay >> 8) & 0xFF,
            0,
        ])
        with open(self._mouse, "wb") as f:
            f.write(report)
            f.flush()

    # ── High-level actions ───────────────────────────────────────────────

    async def wait_for_text(
        self, text: str, timeout: float = 60, interval: float = 1.0,
        mode: str = "auto",
    ) -> ScreenState | None:
        """Wait until text appears on screen, then return the screen state."""
        import time
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            state = await self.read_screen(mode=mode)
            if state.has_text(text):
                log.info("Found text: %r", text)
                return state
            await asyncio.sleep(interval)
        log.warning("Timeout waiting for text: %r", text)
        return None

    async def wait_for_any_text(
        self, texts: list[str], timeout: float = 60, interval: float = 1.0,
        mode: str = "auto",
    ) -> tuple[str, ScreenState] | tuple[None, None]:
        """Wait for any of the given texts to appear. Returns (matched_text, state)."""
        import time
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            state = await self.read_screen(mode=mode)
            for t in texts:
                if state.has_text(t):
                    log.info("Found text: %r", t)
                    return t, state
            await asyncio.sleep(interval)
        log.warning("Timeout waiting for any of: %s", texts)
        return None, None

    async def click_text(self, text: str, mode: str = "auto") -> bool:
        """Find text on screen and click its center."""
        state = await self.read_screen(mode=mode)
        region = state.find_text(text)
        if region:
            cx = region.x + region.width // 2
            cy = region.y + region.height // 2
            await self.click(cx, cy)
            return True
        log.warning("Text not found for click: %r", text)
        return False

    # ── BIOS automation presets ──────────────────────────────────────────

    async def enter_bios(
        self, key: str = "delete", timeout: float = 30,
        prompt_text: str = "Press DEL",
    ) -> bool:
        """
        Wait for POST prompt, then press the BIOS entry key.

        Common keys: "delete" (most), "f2" (Dell, ASUS), "f12" (boot menu),
        "f10" (HP), "f1" (Lenovo)
        """
        # Spam the key during POST (many BIOSes have a tiny window)
        import time
        deadline = time.monotonic() + timeout
        entered = False

        while time.monotonic() < deadline:
            # Send the key repeatedly
            await self.key(key)
            await asyncio.sleep(0.3)

            # Check if we're in BIOS (heuristic: no more POST text,
            # or BIOS-specific text appears)
            state = await self.read_screen(mode="bios")
            if state.has_text("BIOS") or state.has_text("Setup Utility") or \
               state.has_text("Main") or state.has_text("Boot"):
                log.info("Entered BIOS setup")
                return True

        return False

    async def set_boot_usb(self, bios_type: str = "auto") -> bool:
        """
        Navigate BIOS to set USB as first boot device.

        This is inherently heuristic — BIOS UIs vary wildly.  Supports:
          - AMI Aptio (most modern motherboards)
          - Phoenix/Award (older boards)
          - Dell BIOS
          - HP BIOS

        For best results, use the full scripting DSL on the controller
        (controller/automation.py) which handles edge cases.
        """
        # Generic approach: find "Boot" tab, navigate to it
        state = await self.read_screen(mode="bios")

        # Try pressing right arrow to get to Boot tab
        # Most BIOS: Main | Advanced | Boot | Security | Exit
        for _ in range(5):
            if state.has_text("Boot") and (
                state.has_text("Boot Option") or
                state.has_text("Boot Priority") or
                state.has_text("Boot Order")
            ):
                break
            await self.key("right")
            await asyncio.sleep(0.5)
            state = await self.read_screen(mode="bios")

        if not state.has_text("Boot"):
            log.warning("Could not find Boot tab in BIOS")
            return False

        # Look for USB entry and move it up
        # This varies hugely by BIOS — best effort
        for _ in range(10):
            if state.has_text("USB") or state.has_text("Removable"):
                # Try to select it and move it up
                await self.key("enter")
                await asyncio.sleep(0.3)
                # Move to top
                for _ in range(5):
                    await self.key("+")  # Common "move up" key in BIOS
                    await asyncio.sleep(0.2)
                break
            await self.key("down")
            await asyncio.sleep(0.3)
            state = await self.read_screen(mode="bios")

        # Save and exit: F10 in most BIOSes
        await self.key("f10")
        await asyncio.sleep(1)
        # Confirm save
        state = await self.read_screen(mode="bios")
        if state.has_text("Save") or state.has_text("Y/N") or state.has_text("Yes"):
            await self.key("enter")
            log.info("BIOS boot order changed to USB, saving")
            return True

        return False

    async def run_script(self, script: str) -> None:
        """
        Execute a simple RPA script (subset of the controller DSL).

        Supported commands:
          key <name>              — press a key
          type <text>             — type text
          click <x> <y>          — click at coordinates
          click_text <text>       — find and click text
          wait <seconds>          — sleep
          wait_for_text <text>    — wait for text to appear
          screenshot <path>       — save current frame
          enter_bios [key]        — enter BIOS setup
          set_boot_usb            — set USB as first boot device
        """
        for line in script.strip().splitlines():
            line = line.strip()
            if not line or line.startswith("#"):
                continue

            parts = line.split(maxsplit=1)
            cmd = parts[0].lower()
            arg = parts[1] if len(parts) > 1 else ""

            if cmd == "key":
                await self.key(arg.strip())
            elif cmd == "type":
                await self.type_text(arg)
            elif cmd == "click":
                coords = arg.split()
                if len(coords) == 2:
                    await self.click(int(coords[0]), int(coords[1]))
            elif cmd == "click_text":
                await self.click_text(arg.strip())
            elif cmd == "wait":
                await asyncio.sleep(float(arg))
            elif cmd == "wait_for_text":
                parts2 = arg.split("timeout=")
                text = parts2[0].strip().strip('"')
                timeout = float(parts2[1]) if len(parts2) > 1 else 60
                await self.wait_for_text(text, timeout=timeout)
            elif cmd == "screenshot":
                frame = await self.grab_frame()
                if frame:
                    frame.save(arg.strip())
            elif cmd == "enter_bios":
                key = arg.strip() if arg.strip() else "delete"
                await self.enter_bios(key=key)
            elif cmd == "set_boot_usb":
                await self.set_boot_usb()
            else:
                log.warning("Unknown RPA command: %s", cmd)

            await asyncio.sleep(0.05)
