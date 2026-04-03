# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
#!/usr/bin/env python3
"""
Ozma Desktop Agent — runs INSIDE a target machine.

The agent is the "inside" counterpart to the node (which is "outside").
Install on any Linux, macOS, or Windows machine and it appears in the
ozma dashboard as a manageable machine.

For bare-metal PCs with no hardware node, the agent IS the ozma
presence. It registers directly with the controller.

What it provides:
  - HID injection: receives keyboard/mouse packets from the Controller
    and injects them into the local input system (uinput on Linux,
    CGEvent on macOS, SendInput on Windows)
  - Audio routing: creates a virtual audio device that the Controller
    can route to/from (PipeWire/PulseAudio on Linux, CoreAudio on macOS,
    WASAPI loopback on Windows)
  - Display capture: captures the screen for streaming
  - Clipboard sync, display geometry, wallpaper control
  - System metrics (CPU, RAM, disk, displays, network) via Prometheus
  - Room correction: sweep + FFT + EQ on the machine's own PipeWire
  - mDNS announcement + direct registration with controller

Usage:
  uv pip install ozma-agent
  ozma-agent --name my-desktop
  ozma-agent --name my-desktop --controller https://ozma.hrdwrbob.net
  ozma-agent install --controller https://ozma.hrdwrbob.net  # background service
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
import platform
import signal
import socket
import struct
import sys
from pathlib import Path

from aiohttp import web
from screen_capture import ScreenCaptureBackend

log = logging.getLogger("ozma.agent.desktop")

PROTO_VERSION = 1
MAX_PACKET = 64


# ── HID injection backends ──────────────────────────────────────────────────

class HIDInjectorLinux:
    """Inject HID events via uinput on Linux."""

    def __init__(self) -> None:
        self._kbd_dev = None
        self._mouse_dev = None

    async def start(self) -> bool:
        try:
            import evdev
            from evdev import UInput, ecodes

            # Virtual keyboard
            kbd_cap = {ecodes.EV_KEY: list(range(1, 256))}
            self._kbd_dev = UInput(kbd_cap, name="ozma-softnode-kbd")

            # Virtual absolute pointer
            mouse_cap = {
                ecodes.EV_KEY: [ecodes.BTN_LEFT, ecodes.BTN_RIGHT, ecodes.BTN_MIDDLE],
                ecodes.EV_ABS: [
                    (ecodes.ABS_X, evdev.AbsInfo(0, 0, 32767, 0, 0, 0)),
                    (ecodes.ABS_Y, evdev.AbsInfo(0, 0, 32767, 0, 0, 0)),
                ],
                ecodes.EV_REL: [ecodes.REL_WHEEL],
            }
            self._mouse_dev = UInput(mouse_cap, name="ozma-softnode-mouse")

            log.info("Linux HID injector ready (uinput)")
            return True
        except Exception as e:
            log.warning("Linux HID injector failed: %s", e)
            return False

    def inject_keyboard(self, report: bytes) -> None:
        """Inject an 8-byte HID keyboard report."""
        if not self._kbd_dev:
            return
        from evdev import ecodes

        # HID boot report: [modifier, reserved, key1..key6]
        modifier = report[0]
        keys = [k for k in report[2:8] if k != 0]

        # Map HID keycodes to evdev (simplified — full map in keycodes.py)
        HID_TO_EVDEV = _build_hid_to_evdev_map()

        # Release all then press current
        # This is simplified — a proper implementation tracks state
        for hid_key in keys:
            ev_key = HID_TO_EVDEV.get(hid_key)
            if ev_key:
                self._kbd_dev.write(ecodes.EV_KEY, ev_key, 1)
        self._kbd_dev.syn()

    def inject_mouse(self, report: bytes) -> None:
        """Inject a 6-byte HID mouse report."""
        if not self._mouse_dev:
            return
        from evdev import ecodes

        buttons = report[0]
        x = report[1] | (report[2] << 8)
        y = report[3] | (report[4] << 8)
        scroll = struct.unpack('b', bytes([report[5]]))[0] if len(report) > 5 else 0

        self._mouse_dev.write(ecodes.EV_ABS, ecodes.ABS_X, x)
        self._mouse_dev.write(ecodes.EV_ABS, ecodes.ABS_Y, y)

        # Buttons
        self._mouse_dev.write(ecodes.EV_KEY, ecodes.BTN_LEFT, 1 if buttons & 1 else 0)
        self._mouse_dev.write(ecodes.EV_KEY, ecodes.BTN_RIGHT, 1 if buttons & 2 else 0)
        self._mouse_dev.write(ecodes.EV_KEY, ecodes.BTN_MIDDLE, 1 if buttons & 4 else 0)

        if scroll:
            self._mouse_dev.write(ecodes.EV_REL, ecodes.REL_WHEEL, scroll)

        self._mouse_dev.syn()

    async def stop(self) -> None:
        if self._kbd_dev:
            self._kbd_dev.close()
        if self._mouse_dev:
            self._mouse_dev.close()


class HIDInjectorWindows:
    """Inject HID events via SendInput on Windows. Pure ctypes, no deps."""

    def __init__(self) -> None:
        self._prev_keys: set[int] = set()

    async def start(self) -> bool:
        try:
            import ctypes
            self._user32 = ctypes.windll.user32
            log.info("Windows HID injector ready (SendInput)")
            return True
        except Exception as e:
            log.warning("Windows HID injector failed: %s", e)
            return False

    def inject_keyboard(self, report: bytes) -> None:
        """Inject an 8-byte HID keyboard report via SendInput."""
        import ctypes

        INPUT_KEYBOARD = 1
        KEYEVENTF_KEYUP = 0x0002
        KEYEVENTF_SCANCODE = 0x0008

        class KEYBDINPUT(ctypes.Structure):
            _fields_ = [("wVk", ctypes.c_ushort), ("wScan", ctypes.c_ushort),
                        ("dwFlags", ctypes.c_ulong), ("time", ctypes.c_ulong),
                        ("dwExtraInfo", ctypes.POINTER(ctypes.c_ulong))]

        class INPUT_UNION(ctypes.Union):
            _fields_ = [("ki", KEYBDINPUT)]

        class INPUT(ctypes.Structure):
            _fields_ = [("type", ctypes.c_ulong), ("iu", INPUT_UNION)]

        modifier = report[0]
        current_keys = {k for k in report[2:8] if k != 0}

        # Map HID modifier bits to VK codes
        MOD_VK = [(0x01, 0xA2), (0x02, 0xA0), (0x04, 0xA4), (0x08, 0x5B),  # LCtrl, LShift, LAlt, LWin
                  (0x10, 0xA3), (0x20, 0xA1), (0x40, 0xA5), (0x80, 0x5C)]  # RCtrl, RShift, RAlt, RWin

        inputs = []

        # Modifier keys
        for bit, vk in MOD_VK:
            if modifier & bit:
                inp = INPUT(type=INPUT_KEYBOARD)
                inp.iu.ki = KEYBDINPUT(wVk=vk, dwFlags=0)
                inputs.append(inp)

        # Released keys
        for hid_code in self._prev_keys - current_keys:
            vk = _hid_to_vk(hid_code)
            if vk:
                inp = INPUT(type=INPUT_KEYBOARD)
                inp.iu.ki = KEYBDINPUT(wVk=vk, dwFlags=KEYEVENTF_KEYUP)
                inputs.append(inp)

        # Pressed keys
        for hid_code in current_keys - self._prev_keys:
            vk = _hid_to_vk(hid_code)
            if vk:
                inp = INPUT(type=INPUT_KEYBOARD)
                inp.iu.ki = KEYBDINPUT(wVk=vk, dwFlags=0)
                inputs.append(inp)

        # Released modifiers
        for bit, vk in MOD_VK:
            if not (modifier & bit):
                inp = INPUT(type=INPUT_KEYBOARD)
                inp.iu.ki = KEYBDINPUT(wVk=vk, dwFlags=KEYEVENTF_KEYUP)
                inputs.append(inp)

        if inputs:
            arr = (INPUT * len(inputs))(*inputs)
            self._user32.SendInput(len(inputs), ctypes.byref(arr), ctypes.sizeof(INPUT))

        self._prev_keys = current_keys

    def inject_mouse(self, report: bytes) -> None:
        """Inject a 6-byte HID mouse report via SendInput."""
        import ctypes

        INPUT_MOUSE = 0
        MOUSEEVENTF_ABSOLUTE = 0x8000
        MOUSEEVENTF_MOVE = 0x0001
        MOUSEEVENTF_LEFTDOWN = 0x0002
        MOUSEEVENTF_LEFTUP = 0x0004
        MOUSEEVENTF_RIGHTDOWN = 0x0008
        MOUSEEVENTF_RIGHTUP = 0x0010
        MOUSEEVENTF_MIDDLEDOWN = 0x0020
        MOUSEEVENTF_MIDDLEUP = 0x0040
        MOUSEEVENTF_WHEEL = 0x0800

        class MOUSEINPUT(ctypes.Structure):
            _fields_ = [("dx", ctypes.c_long), ("dy", ctypes.c_long),
                        ("mouseData", ctypes.c_ulong), ("dwFlags", ctypes.c_ulong),
                        ("time", ctypes.c_ulong), ("dwExtraInfo", ctypes.POINTER(ctypes.c_ulong))]

        class INPUT_UNION(ctypes.Union):
            _fields_ = [("mi", MOUSEINPUT)]

        class INPUT(ctypes.Structure):
            _fields_ = [("type", ctypes.c_ulong), ("iu", INPUT_UNION)]

        buttons = report[0]
        x = int.from_bytes(report[1:3], "little")
        y = int.from_bytes(report[3:5], "little")
        scroll = report[5] if len(report) > 5 else 0
        if scroll > 127:
            scroll -= 256

        # Convert 0-32767 absolute to 0-65535 (SendInput range)
        abs_x = int(x * 65535 / 32767) if x <= 32767 else 0
        abs_y = int(y * 65535 / 32767) if y <= 32767 else 0

        flags = MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_MOVE

        # Button flags
        if buttons & 0x01:
            flags |= MOUSEEVENTF_LEFTDOWN
        else:
            flags |= MOUSEEVENTF_LEFTUP
        if buttons & 0x02:
            flags |= MOUSEEVENTF_RIGHTDOWN
        else:
            flags |= MOUSEEVENTF_RIGHTUP
        if buttons & 0x04:
            flags |= MOUSEEVENTF_MIDDLEDOWN
        else:
            flags |= MOUSEEVENTF_MIDDLEUP

        mouse_data = 0
        if scroll:
            flags |= MOUSEEVENTF_WHEEL
            mouse_data = scroll * 120  # WHEEL_DELTA = 120

        inp = INPUT(type=INPUT_MOUSE)
        inp.iu.mi = MOUSEINPUT(dx=abs_x, dy=abs_y, mouseData=mouse_data, dwFlags=flags)
        self._user32.SendInput(1, ctypes.byref(inp), ctypes.sizeof(INPUT))

    async def stop(self) -> None:
        pass


class HIDInjectorMacOS:
    """Inject HID events via Quartz CGEvent on macOS. Pure Python, no deps."""

    def __init__(self) -> None:
        self._prev_keys: set[int] = set()

    async def start(self) -> bool:
        try:
            import Quartz  # noqa: F401
            log.info("macOS HID injector ready (CGEvent)")
            return True
        except ImportError:
            log.warning("macOS HID injector needs pyobjc-framework-Quartz: uv pip install pyobjc-framework-Quartz")
            return False

    def inject_keyboard(self, report: bytes) -> None:
        import Quartz
        current_keys = {k for k in report[2:8] if k != 0}
        for hid_code in current_keys - self._prev_keys:
            vk = _hid_to_mac_keycode(hid_code)
            if vk is not None:
                evt = Quartz.CGEventCreateKeyboardEvent(None, vk, True)
                Quartz.CGEventPost(Quartz.kCGHIDEventTap, evt)
        for hid_code in self._prev_keys - current_keys:
            vk = _hid_to_mac_keycode(hid_code)
            if vk is not None:
                evt = Quartz.CGEventCreateKeyboardEvent(None, vk, False)
                Quartz.CGEventPost(Quartz.kCGHIDEventTap, evt)
        self._prev_keys = current_keys

    def inject_mouse(self, report: bytes) -> None:
        import Quartz
        buttons = report[0]
        x = int.from_bytes(report[1:3], "little")
        y = int.from_bytes(report[3:5], "little")
        # Get screen size for absolute positioning
        main = Quartz.CGMainDisplayID()
        w = Quartz.CGDisplayPixelsWide(main)
        h = Quartz.CGDisplayPixelsHigh(main)
        px = x * w / 32767
        py = y * h / 32767
        point = Quartz.CGPointMake(px, py)
        if buttons & 0x01:
            evt = Quartz.CGEventCreateMouseEvent(None, Quartz.kCGEventLeftMouseDown, point, Quartz.kCGMouseButtonLeft)
        elif buttons & 0x02:
            evt = Quartz.CGEventCreateMouseEvent(None, Quartz.kCGEventRightMouseDown, point, Quartz.kCGMouseButtonRight)
        else:
            evt = Quartz.CGEventCreateMouseEvent(None, Quartz.kCGEventMouseMoved, point, Quartz.kCGMouseButtonLeft)
        Quartz.CGEventPost(Quartz.kCGHIDEventTap, evt)

    async def stop(self) -> None:
        pass


class HIDInjectorStub:
    """Stub injector for platforms where injection isn't available."""

    async def start(self) -> bool:
        log.info("HID injector: stub (no injection on this platform)")
        return True

    def inject_keyboard(self, report: bytes) -> None:
        pass

    def inject_mouse(self, report: bytes) -> None:
        pass

    async def stop(self) -> None:
        pass


def _build_hid_to_evdev_map() -> dict[int, int]:
    """Build a minimal HID-to-evdev keycode map."""
    from evdev import ecodes
    # HID usage ID → evdev keycode (subset — full map in keycodes.py)
    m: dict[int, int] = {}
    # Letters a-z: HID 0x04-0x1D → evdev KEY_A-KEY_Z
    for i, key in enumerate(range(ecodes.KEY_A, ecodes.KEY_Z + 1)):
        m[0x04 + i] = key
    # Digits 1-0: HID 0x1E-0x27
    digit_keys = [ecodes.KEY_1, ecodes.KEY_2, ecodes.KEY_3, ecodes.KEY_4,
                  ecodes.KEY_5, ecodes.KEY_6, ecodes.KEY_7, ecodes.KEY_8,
                  ecodes.KEY_9, ecodes.KEY_0]
    for i, key in enumerate(digit_keys):
        m[0x1E + i] = key
    # Common keys
    m[0x28] = ecodes.KEY_ENTER
    m[0x29] = ecodes.KEY_ESC
    m[0x2A] = ecodes.KEY_BACKSPACE
    m[0x2B] = ecodes.KEY_TAB
    m[0x2C] = ecodes.KEY_SPACE
    m[0x4F] = ecodes.KEY_RIGHT
    m[0x50] = ecodes.KEY_LEFT
    m[0x51] = ecodes.KEY_DOWN
    m[0x52] = ecodes.KEY_UP
    return m


def _hid_to_vk(hid_code: int) -> int:
    """Map HID usage ID to Windows Virtual Key code."""
    # Letters a-z: HID 0x04-0x1D → VK 0x41-0x5A
    if 0x04 <= hid_code <= 0x1D:
        return 0x41 + (hid_code - 0x04)
    # Digits 1-9,0: HID 0x1E-0x27 → VK 0x31-0x39,0x30
    if 0x1E <= hid_code <= 0x26:
        return 0x31 + (hid_code - 0x1E)
    if hid_code == 0x27:
        return 0x30
    m = {
        0x28: 0x0D, 0x29: 0x1B, 0x2A: 0x08, 0x2B: 0x09,  # Enter, Esc, Backspace, Tab
        0x2C: 0x20,  # Space
        0x2D: 0xBD, 0x2E: 0xBB, 0x2F: 0xDB, 0x30: 0xDD,  # -, =, [, ]
        0x31: 0xDC, 0x33: 0xBA, 0x34: 0xDE, 0x35: 0xC0,  # \, ;, ', `
        0x36: 0xBC, 0x37: 0xBE, 0x38: 0xBF,  # , . /
        0x39: 0x14,  # CapsLock
        0x3A: 0x70, 0x3B: 0x71, 0x3C: 0x72, 0x3D: 0x73,  # F1-F4
        0x3E: 0x74, 0x3F: 0x75, 0x40: 0x76, 0x41: 0x77,  # F5-F8
        0x42: 0x78, 0x43: 0x79, 0x44: 0x7A, 0x45: 0x7B,  # F9-F12
        0x46: 0x2C, 0x47: 0x91, 0x48: 0x13,  # PrtSc, ScrollLock, Pause
        0x49: 0x2D, 0x4A: 0x24, 0x4B: 0x21,  # Insert, Home, PageUp
        0x4C: 0x2E, 0x4D: 0x23, 0x4E: 0x22,  # Delete, End, PageDown
        0x4F: 0x27, 0x50: 0x25, 0x51: 0x28, 0x52: 0x26,  # Right, Left, Down, Up
    }
    return m.get(hid_code, 0)


def _hid_to_mac_keycode(hid_code: int) -> int | None:
    """Map HID usage ID to macOS virtual keycode."""
    # Letters a-z
    mac_letters = [0, 11, 8, 2, 14, 3, 5, 4, 34, 38, 40, 37, 46, 45, 31, 35, 12, 15, 1, 17, 32, 9, 13, 7, 16, 6]
    if 0x04 <= hid_code <= 0x1D:
        return mac_letters[hid_code - 0x04]
    # Digits 1-0
    mac_digits = [18, 19, 20, 21, 23, 22, 26, 28, 25, 29]
    if 0x1E <= hid_code <= 0x27:
        return mac_digits[hid_code - 0x1E]
    m = {
        0x28: 36, 0x29: 53, 0x2A: 51, 0x2B: 48,  # Return, Esc, Delete, Tab
        0x2C: 49,  # Space
        0x4F: 124, 0x50: 123, 0x51: 125, 0x52: 126,  # Arrow keys
    }
    return m.get(hid_code)


# ── Audio backend ───────────────────────────────────────────────────────────

class AudioBackendWindows:
    """
    Windows audio backend.

    Windows doesn't have PipeWire. Options:
    - If VB-CABLE or similar virtual audio cable is installed, use it
    - Otherwise, the machine's default audio output just works
    - For audio routing: the controller routes via VBAN over the network
    """

    def __init__(self, node_name: str) -> None:
        self._name = node_name

    async def start(self) -> str | None:
        # Check for VB-CABLE
        try:
            import subprocess
            result = subprocess.run(
                ["powershell", "-Command",
                 "Get-AudioDevice -List | Where-Object { $_.Name -like '*CABLE*' } | Select-Object -First 1 -ExpandProperty Name"],
                capture_output=True, text=True, timeout=5,
            )
            if result.returncode == 0 and result.stdout.strip():
                name = result.stdout.strip()
                log.info("Windows audio: found virtual cable '%s'", name)
                return name
        except Exception:
            pass
        log.info("Windows audio: using default output (no virtual cable detected)")
        return None

    async def stop(self) -> None:
        pass


class AudioBackendMacOS:
    """macOS audio backend. Uses BlackHole or Soundflower if available."""

    def __init__(self, node_name: str) -> None:
        self._name = node_name

    async def start(self) -> str | None:
        import subprocess
        try:
            result = subprocess.run(
                ["system_profiler", "SPAudioDataType"],
                capture_output=True, text=True, timeout=5,
            )
            for line in result.stdout.splitlines():
                if "BlackHole" in line or "Soundflower" in line:
                    name = line.strip().rstrip(":")
                    log.info("macOS audio: found virtual device '%s'", name)
                    return name
        except Exception:
            pass
        log.info("macOS audio: using default output")
        return None

    async def stop(self) -> None:
        pass


class AudioBackendLinux:
    """Create a virtual PipeWire/PulseAudio sink for audio routing."""

    def __init__(self, node_name: str) -> None:
        self._name = node_name
        self._sink_name = f"ozma-{node_name}"
        self._module_id: int | None = None

    async def start(self) -> str | None:
        """Create a null sink. Returns the sink name or None."""
        import subprocess
        try:
            result = subprocess.run(
                ["pactl", "load-module", "module-null-sink",
                 f"sink_name={self._sink_name}",
                 f"sink_properties=device.description=Ozma-{self._name}"],
                capture_output=True, text=True, timeout=5,
            )
            if result.returncode == 0:
                self._module_id = int(result.stdout.strip())
                log.info("Audio sink created: %s (module %d)", self._sink_name, self._module_id)
                return self._sink_name
        except Exception as e:
            log.warning("Failed to create audio sink: %s", e)
        return None

    async def stop(self) -> None:
        if self._module_id is not None:
            import subprocess
            try:
                subprocess.run(
                    ["pactl", "unload-module", str(self._module_id)],
                    capture_output=True, timeout=5,
                )
            except Exception:
                pass


# ── Doorbell event handler ───────────────────────────────────────────────────

def _parse_host(url: str) -> str:
    """Extract hostname from a URL string."""
    try:
        from urllib.parse import urlparse as _up
        return _up(url).hostname or "localhost"
    except Exception:
        return "localhost"


# VBAN frame header: 4-byte magic + 24 bytes of fields = 28 bytes total
_VBAN_MAGIC = b"VBAN"
_VBAN_HEADER_SIZE = 28
_VBAN_SAMPLE_RATES = [6000, 12000, 24000, 48000, 96000, 192000, 384000,
                      8000, 16000, 32000, 64000, 128000, 256000, 512000,
                      11025, 22050, 44100, 88200, 176400, 352800,
                      44100 * 8, 0, 0, 0, 0]
_DOORBELL_MIC_PORT = 6982    # controller listens here for mic VBAN from the agent


def _vban_encode_header(
    stream_name: str, counter: int,
    sample_rate: int = 48000, channels: int = 2, samples: int = 256,
) -> bytes:
    """Pack a 28-byte VBAN frame header."""
    try:
        sr_idx = _VBAN_SAMPLE_RATES.index(sample_rate)
    except ValueError:
        sr_idx = 3  # default 48000
    name = stream_name.encode("ascii", errors="replace")[:16].ljust(16, b"\x00")
    return (
        _VBAN_MAGIC
        + bytes([sr_idx, samples - 1, channels - 1, 0x01])
        + name
        + counter.to_bytes(4, "little")
    )


class DoorbellEventHandler:
    """
    Subscribes to the controller's WebSocket event stream and handles
    doorbell events directly on the machine where the user is sitting.

    doorbell.ringing:  show native OS notification (notify-send / osascript / toast)
    doorbell.answered: start VBANSender (PipeWire mic → UDP → controller:6982)
                       The controller's VBANToBackchannelBridge receives it and
                       forwards it to the camera RTSP backchannel.
    doorbell.dismissed / doorbell.expired: stop VBANSender

    Overlay: the browser overlay (dashboard/OzmaConsole) handles the visual.
    For compositing-capable displays (Wayland/X11 managed by screen_manager),
    the doorbell.ringing event in the controller's event queue is picked up by
    screen_manager and rendered as an overlay widget on connected display devices.
    A native on-screen overlay (wlr-layer-shell / X11 override-redirect) is a
    future enhancement.
    """

    def __init__(self, controller_url: str, controller_host: str) -> None:
        self._ctrl_url = controller_url.rstrip("/")
        self._ctrl_host = controller_host
        self._vban_task: asyncio.Task | None = None
        self._vban_stop = asyncio.Event()

    async def run(self) -> None:
        """Reconnecting WebSocket subscriber loop."""
        ws_proto = "wss" if self._ctrl_url.startswith("https") else "ws"
        base = self._ctrl_url.replace("http://", "").replace("https://", "")
        ws_url = f"{ws_proto}://{base}/api/v1/events"
        while True:
            try:
                import aiohttp
                async with aiohttp.ClientSession() as session:
                    async with session.ws_connect(ws_url, heartbeat=30) as ws:
                        log.info("Doorbell: subscribed to %s", ws_url)
                        async for msg in ws:
                            if msg.type == aiohttp.WSMsgType.TEXT:
                                try:
                                    evt = json.loads(msg.data)
                                    await self._handle(evt)
                                except Exception as exc:
                                    log.debug("Doorbell event error: %s", exc)
            except Exception as exc:
                log.debug("Doorbell WS disconnected (%s), retrying in 10s", exc)
            await asyncio.sleep(10)

    async def _handle(self, evt: dict) -> None:
        t = evt.get("type", "")
        if t == "doorbell.ringing":
            camera = evt.get("camera", "camera")
            person = evt.get("person", "")
            msg = f"{person} at your door" if person else f"Doorbell — {camera}"
            await self._notify(msg)
        elif t == "doorbell.answered":
            session_id = evt.get("id", "")
            await self._start_mic_vban(session_id)
        elif t in ("doorbell.dismissed", "doorbell.expired"):
            await self._stop_mic_vban()

    async def _notify(self, message: str) -> None:
        """Show a native notification. Best-effort — never raises."""
        system = platform.system()
        try:
            if system == "Linux":
                await asyncio.create_subprocess_exec(
                    "notify-send", "--urgency=critical",
                    "--icon=camera", "Doorbell", message,
                    stderr=asyncio.subprocess.DEVNULL,
                )
            elif system == "Darwin":
                script = f'display notification "{message}" with title "Doorbell" sound name "Funk"'
                await asyncio.create_subprocess_exec(
                    "osascript", "-e", script,
                    stderr=asyncio.subprocess.DEVNULL,
                )
            elif system == "Windows":
                # Windows 10+: use PowerShell toast notification
                ps = (
                    f"[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType=WindowsRuntime] | Out-Null;"
                    f"$t = [Windows.UI.Notifications.ToastTemplateType]::ToastText02;"
                    f"$x = [Windows.UI.Notifications.ToastNotificationManager]::GetTemplateContent($t);"
                    f"$x.GetElementsByTagName('text')[0].AppendChild($x.CreateTextNode('Doorbell')) | Out-Null;"
                    f"$x.GetElementsByTagName('text')[1].AppendChild($x.CreateTextNode('{message}')) | Out-Null;"
                    f"[Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('Ozma').Show([Windows.UI.Notifications.ToastNotification]::new($x))"
                )
                await asyncio.create_subprocess_exec(
                    "powershell", "-WindowStyle", "Hidden", "-Command", ps,
                    stderr=asyncio.subprocess.DEVNULL,
                )
        except Exception as exc:
            log.debug("Doorbell notify failed: %s", exc)

    async def _start_mic_vban(self, session_id: str) -> None:
        """Start sending PipeWire mic audio as VBAN to the controller."""
        await self._stop_mic_vban()
        self._vban_stop.clear()
        self._vban_task = asyncio.create_task(
            self._vban_sender_loop(), name=f"doorbell-mic-{session_id}"
        )
        log.info("Doorbell mic VBAN started → %s:%d",
                 self._ctrl_host, _DOORBELL_MIC_PORT)

    async def _stop_mic_vban(self) -> None:
        self._vban_stop.set()
        if self._vban_task:
            self._vban_task.cancel()
            try:
                await self._vban_task
            except asyncio.CancelledError:
                pass
            self._vban_task = None

    async def _vban_sender_loop(self) -> None:
        """
        Capture from PipeWire default mic via pw-cat and send as VBAN UDP
        frames to the controller for forwarding to the camera backchannel.
        """
        cmd = [
            "pw-cat", "--capture",
            "--format", "s16",
            "--rate", "48000",
            "--channels", "2",
            "-",
        ]
        try:
            proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.DEVNULL,
            )
        except FileNotFoundError:
            log.warning("pw-cat not found — doorbell mic VBAN unavailable "
                        "(is PipeWire installed?)")
            return

        sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        frame_bytes = 256 * 2 * 2   # 256 samples * 2 channels * 2 bytes (s16)
        counter = 0
        try:
            while not self._vban_stop.is_set():
                assert proc.stdout is not None
                chunk = await proc.stdout.read(frame_bytes)
                if not chunk:
                    break
                if len(chunk) < frame_bytes:
                    chunk = chunk + b"\x00" * (frame_bytes - len(chunk))
                header = _vban_encode_header(
                    "doorbell-mic", counter,
                    sample_rate=48000, channels=2, samples=256,
                )
                sock.sendto(header + chunk, (self._ctrl_host, _DOORBELL_MIC_PORT))
                counter = (counter + 1) & 0xFFFF_FFFF
        except asyncio.CancelledError:
            pass
        except Exception as exc:
            log.debug("Doorbell mic VBAN loop ended: %s", exc)
        finally:
            sock.close()
            if proc.returncode is None:
                proc.terminate()
                try:
                    await asyncio.wait_for(proc.wait(), timeout=2.0)
                except asyncio.TimeoutError:
                    proc.kill()


# ── Desktop Soft Node ───────────────────────────────────────────────────────

class DesktopSoftNode:
    """
    A soft node that runs on any desktop/laptop/server.

    Receives HID packets from the Controller and injects them into the
    local input system. Creates a virtual audio sink for audio routing.
    Announces via mDNS so the Controller discovers it automatically.
    """

    def __init__(self, name: str, host: str = "0.0.0.0", port: int = 7331,
                 controller_url: str = "", api_port: int = 7382,
                 capture_fps: int = 15) -> None:
        self._name = name
        self._host = host
        self._port = port
        self._api_port = api_port
        self._controller_url = controller_url
        self._stop_event = asyncio.Event()
        self._hid: HIDInjectorLinux | HIDInjectorStub | None = None
        self._audio: AudioBackendLinux | None = None
        self._audio_sink: str | None = None
        self._play_proc: asyncio.subprocess.Process | None = None
        self._screen: ScreenCaptureBackend | None = None
        self._capture_fps = capture_fps

    async def run(self) -> None:
        # Platform-specific HID injector
        system = platform.system()
        if system == "Linux":
            self._hid = HIDInjectorLinux()
        elif system == "Windows":
            self._hid = HIDInjectorWindows()
        elif system == "Darwin":
            self._hid = HIDInjectorMacOS()
        else:
            self._hid = HIDInjectorStub()

        ok = await self._hid.start()
        if not ok:
            self._hid = HIDInjectorStub()
            await self._hid.start()

        # Audio backend
        if system == "Linux":
            self._audio = AudioBackendLinux(self._name)
        elif system == "Windows":
            self._audio = AudioBackendWindows(self._name)
        elif system == "Darwin":
            self._audio = AudioBackendMacOS(self._name)
        if self._audio:
            self._audio_sink = await self._audio.start()

        # Screen capture
        if self._capture_fps > 0:
            capture_dir = str(Path(os.environ.get('TEMP', '/tmp')) / f"ozma-agent-{self._name}")
            self._screen = ScreenCaptureBackend(
                output_dir=capture_dir, fps=self._capture_fps,
            )
            screen_ok = await self._screen.start()
            if screen_ok:
                log.info("Screen capture: %s", self._screen.backend)
            else:
                log.info("Screen capture not available (display may not be accessible)")
        else:
            log.info("Screen capture disabled (--no-capture)")

        # HTTP server for HLS stream + status
        await self._start_http()

        # mDNS announcement
        await self._announce()

        # Direct registration with controller (if URL provided)
        if self._controller_url:
            asyncio.create_task(self._direct_register(), name=f"register-{self._name}")

        # Doorbell event handler (overlay + two-way audio)
        if self._controller_url:
            self._doorbell = DoorbellEventHandler(
                controller_url=self._controller_url,
                controller_host=_parse_host(self._controller_url),
            )
            asyncio.create_task(
                self._doorbell.run(), name="doorbell-events"
            )

        # UDP server for HID packets
        await self._serve()

    async def _start_http(self) -> None:
        """Start HTTP server for HLS stream and status."""
        app = web.Application()

        async def status_handler(_: web.Request) -> web.Response:
            return web.json_response({
                "name": self._name,
                "audio_sink": self._audio_sink,
                "screen": self._screen.to_dict() if self._screen else None,
            })

        async def snapshot_handler(_: web.Request) -> web.Response:
            if not self._screen:
                return web.json_response({"error": "no screen capture"}, status=503)
            data = await self._screen.snapshot()
            if data:
                return web.Response(body=data, content_type="image/jpeg")
            return web.json_response({"error": "capture failed"}, status=503)

        async def health_handler(_: web.Request) -> web.Response:
            return web.json_response({"ok": True})

        async def audio_nodes_handler(_: web.Request) -> web.Response:
            """List PipeWire sources and sinks on this node."""
            pw_nodes = await self._get_pw_nodes()
            default_sink = next((n["name"] for n in pw_nodes if n.get("default") and "Sink" in n["media_class"]), "")
            default_source = next((n["name"] for n in pw_nodes if n.get("default") and "Source" in n["media_class"]), "")
            return web.json_response({
                "nodes": pw_nodes,
                "default_sink": default_sink,
                "default_source": default_source,
            })

        async def sweep_handler(request: web.Request) -> web.Response:
            """Run a room correction sweep on this node's PipeWire."""
            body = await request.json()
            source = body.get("source", "")
            sink = body.get("sink", "")
            if not source or not sink:
                return web.json_response({"ok": False, "error": "source and sink required"}, status=400)
            try:
                rc = _get_rc()
                profile = await rc.run_sweep(
                    source=source, sink=sink,
                    phone_model=body.get("phone_model", "generic"),
                    target_curve=body.get("target_curve", "harman"),
                    room_name=body.get("room_name", ""),
                    node_id=f"{self._name}._ozma._udp.local.",
                )
                if not profile:
                    return web.json_response({"ok": False, "error": "Sweep failed"})
                return web.json_response({"ok": True, "profile": profile.to_dict()})
            except ImportError as e:
                return web.json_response({"ok": False, "error": f"Missing dependency: {e}"})

        # Shared RoomCorrectionManager for this node (lazy init)
        _rc_instance = None
        def _get_rc():
            nonlocal _rc_instance
            if _rc_instance is None:
                ctrl_dir = str(Path(__file__).parent.parent / "controller")
                if ctrl_dir not in sys.path:
                    sys.path.insert(0, ctrl_dir)
                from room_correction import RoomCorrectionManager
                _rc_instance = RoomCorrectionManager()
            return _rc_instance

        async def apply_handler(request: web.Request) -> web.Response:
            body = await request.json()
            profile_id = body.get("profile_id", "")
            rc = _get_rc()
            ok = await rc.apply_correction(profile_id)
            if not ok:
                return web.json_response({"ok": False, "error": "Profile not found or apply failed"}, status=404)
            return web.json_response({"ok": True, "profile_id": profile_id})

        async def remove_handler(_: web.Request) -> web.Response:
            rc = _get_rc()
            await rc.remove_correction()
            return web.json_response({"ok": True})

        async def play_handler(request: web.Request) -> web.Response:
            """Play a reference track through a PipeWire sink via pw-play."""
            body = await request.json()
            track = body.get("track", "")
            sink = body.get("sink", "")
            if not track or not sink:
                return web.json_response({"ok": False, "error": "track and sink required"}, status=400)
            # Sanitise track name — only alphanumeric, underscore, dash
            import re as _re
            if not _re.match(r'^[\w-]+$', track):
                return web.json_response({"ok": False, "error": "invalid track name"}, status=400)
            track_dir = Path(__file__).parent.parent / "controller" / "static" / "demo_tracks"
            # Try .flac then .wav
            track_path = None
            for ext in (".flac", ".wav"):
                p = track_dir / f"{track}{ext}"
                if p.exists():
                    track_path = p
                    break
            if not track_path:
                return web.json_response({"ok": False, "error": f"Track not found: {track}"}, status=404)
            self._play_proc = await asyncio.create_subprocess_exec(
                "pw-play", "--target", sink, str(track_path),
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.DEVNULL,
            )
            await self._play_proc.wait()
            self._play_proc = None
            return web.json_response({"ok": True})

        async def stop_handler(_: web.Request) -> web.Response:
            if self._play_proc and self._play_proc.returncode is None:
                self._play_proc.terminate()
            return web.json_response({"ok": True})

        # UI hints endpoint — report window state from inside the OS
        _ui_hints = None
        def _get_ui_hints():
            nonlocal _ui_hints
            if _ui_hints is None:
                from ui_hints import UIHintProvider
                _ui_hints = UIHintProvider()
            return _ui_hints

        async def ui_hints_handler(request: web.Request) -> web.Response:
            """
            Report UI state from inside the OS — windows, focused control, text.

            The controller uses this instead of OCR when the agent is available.
            Query params:
              level=1  windows only (fast)
              level=2  windows + focused control (default)
              level=3  windows + full accessibility tree of focused window
            """
            level = int(request.query.get("level", "2"))
            try:
                hints = _get_ui_hints().get_hints(level=level)
                return web.json_response(hints.to_dict())
            except Exception as e:
                return web.json_response({"error": str(e)}, status=500)

        async def ui_windows_handler(_: web.Request) -> web.Response:
            """Just window titles and positions — fastest hint."""
            try:
                hints = _get_ui_hints().get_hints(level=1)
                return web.json_response({
                    "windows": [w.to_dict() for w in hints.windows],
                    "focused": hints.focused_window.to_dict() if hints.focused_window else None,
                })
            except Exception as e:
                return web.json_response({"error": str(e)}, status=500)

        app.router.add_get("/ui/hints", ui_hints_handler)
        app.router.add_get("/ui/windows", ui_windows_handler)

        app.router.add_get("/health", health_handler)
        app.router.add_get("/status", status_handler)
        app.router.add_get("/snapshot", snapshot_handler)
        app.router.add_get("/audio/nodes", audio_nodes_handler)
        app.router.add_post("/audio/sweep", sweep_handler)
        app.router.add_post("/audio/apply", apply_handler)
        app.router.add_post("/audio/remove", remove_handler)
        app.router.add_post("/audio/stop", stop_handler)
        app.router.add_post("/audio/play", play_handler)

        # Serve HLS segments as static files
        capture_dir = Path(str(Path(os.environ.get('TEMP', '/tmp')) / f"ozma-agent-{self._name}"))
        if capture_dir.exists():
            app.router.add_static("/stream/", capture_dir)

        runner = web.AppRunner(app)
        await runner.setup()
        site = web.TCPSite(runner, "0.0.0.0", self._api_port)
        await site.start()
        log.info("HTTP API on port %d (stream + status)", self._api_port)

    async def stop(self) -> None:
        self._stop_event.set()
        if self._screen:
            await self._screen.stop()
        if self._hid:
            await self._hid.stop()
        if self._audio:
            await self._audio.stop()

    async def _announce(self) -> None:
        from zeroconf import ServiceInfo, IPVersion
        from zeroconf.asyncio import AsyncZeroconf

        local_ip = self._resolve_local_ip()
        service_type = "_ozma._udp.local."
        service_name = f"{self._name}.{service_type}"

        properties: dict[str, str] = {
            "proto": str(PROTO_VERSION),
            "role": "compute",
            "hw": f"desktop-{platform.system().lower()}",
            "fw": "1.0.0",
            "cap": "softnode,screen",
            "api_port": str(self._api_port),
            "stream_port": str(self._api_port),
            "stream_path": "/stream/stream.m3u8",
        }
        if self._audio_sink:
            properties["audio_type"] = "pipewire"
            properties["audio_sink"] = self._audio_sink
        if self._screen and self._screen.active:
            properties["cap"] = "softnode,screen,capture"

        self._info = ServiceInfo(
            service_type, service_name,
            addresses=[socket.inet_aton(local_ip)],
            port=self._port,
            properties=properties,
        )
        self._azc = AsyncZeroconf(interfaces=["127.0.0.1"], ip_version=IPVersion.V4Only)
        await self._azc.async_register_service(self._info)
        log.info("mDNS announced: %s @ %s:%d", self._name, local_ip, self._port)

    async def _get_pw_nodes(self) -> list[dict]:
        """List PipeWire audio sources and sinks via pw-dump."""
        try:
            proc = await asyncio.create_subprocess_exec(
                "pw-dump", "-N",
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.DEVNULL,
            )
            stdout, _ = await asyncio.wait_for(proc.communicate(), timeout=5)
            if not stdout:
                return []
            import json as _json
            objects = _json.loads(stdout)
            nodes = []

            # Find default sink/source names from metadata
            default_sink = ""
            default_source = ""
            for obj in objects:
                if obj.get("type") == "PipeWire:Interface:Metadata":
                    for entry in obj.get("metadata", []):
                        key = entry.get("key", "")
                        val = entry.get("value", {})
                        name = val.get("name", "") if isinstance(val, dict) else ""
                        if key == "default.audio.sink":
                            default_sink = name
                        elif key == "default.audio.source":
                            default_source = name

            for obj in objects:
                if obj.get("type") != "PipeWire:Interface:Node":
                    continue
                info = obj.get("info", {})
                props = info.get("props", {})
                media_class = props.get("media.class", "")
                if not media_class or "Audio" not in media_class:
                    continue
                name = props.get("node.name", "")
                desc = props.get("node.description", props.get("node.nick", name))
                is_default = (name == default_sink and "Sink" in media_class) or \
                             (name == default_source and "Source" in media_class)
                nodes.append({
                    "id": obj.get("id", 0),
                    "name": name,
                    "description": desc,
                    "media_class": media_class,
                    "default": is_default,
                })
            return nodes
        except Exception:
            return []

    async def _direct_register(self) -> None:
        """Register directly with the controller via HTTP."""
        import urllib.request

        local_ip = self._resolve_local_ip()
        service_type = "_ozma._udp.local."
        node_id = f"{self._name}.{service_type}"

        body = {
            "id": node_id,
            "host": local_ip,
            "port": self._port,
            "proto": str(PROTO_VERSION),
            "role": "compute",
            "hw": f"desktop-{platform.system().lower()}",
            "fw": "1.0.0",
            "cap": "softnode,screen",
            "api_port": str(self._api_port),
            "stream_port": str(self._api_port),
            "stream_path": "/stream/stream.m3u8",
            "audio_type": "pipewire" if self._audio_sink else "",
            "audio_sink": self._audio_sink or "",
        }

        url = f"{self._controller_url.rstrip('/')}/api/v1/nodes/register"
        await asyncio.sleep(3)
        for attempt in range(10):
            try:
                data = json.dumps(body).encode()
                req = urllib.request.Request(
                    url, data=data,
                    headers={"Content-Type": "application/json"},
                    method="POST",
                )
                loop = asyncio.get_running_loop()
                def _post():
                    with urllib.request.urlopen(req, timeout=3) as r:
                        return json.loads(r.read())
                result = await loop.run_in_executor(None, _post)
                if result.get("ok"):
                    log.info("Registered with controller at %s", self._controller_url)
                    asyncio.create_task(
                        self._re_register_loop(body, url),
                        name=f"re-register-{self._name}",
                    )
                    return
            except Exception:
                await asyncio.sleep(2)
        log.debug("Direct registration failed after 10 attempts")

    async def _re_register_loop(self, body: dict, url: str) -> None:
        """
        Periodically re-register with the controller.

        Handles controller restarts — the node re-appears automatically
        without user intervention. The controller's health check keeps
        the node alive between re-registrations.
        """
        import urllib.request
        while True:
            await asyncio.sleep(60)
            try:
                data = json.dumps(body).encode()
                req = urllib.request.Request(
                    url, data=data,
                    headers={"Content-Type": "application/json"},
                    method="POST",
                )
                loop = asyncio.get_running_loop()
                await loop.run_in_executor(None, lambda: urllib.request.urlopen(req, timeout=5).read())
            except Exception:
                pass

    async def _serve(self) -> None:
        loop = asyncio.get_running_loop()
        transport, protocol = await loop.create_datagram_endpoint(
            lambda: _UDPProtocol(self._on_packet),
            local_addr=(self._host, self._port),
        )
        log.info("Desktop soft node '%s' listening on UDP %s:%d", self._name, self._host, self._port)

        try:
            await self._stop_event.wait()
        finally:
            transport.close()
            if hasattr(self, '_azc'):
                await self._azc.async_unregister_service(self._info)
                await self._azc.async_close()

    def _on_packet(self, data: bytes, addr: tuple) -> None:
        if len(data) < 2:
            return
        pkt_type = data[0]
        payload = data[1:]

        if pkt_type == 0x01 and len(payload) >= 8 and self._hid:
            self._hid.inject_keyboard(payload[:8])
        elif pkt_type == 0x02 and len(payload) >= 6 and self._hid:
            self._hid.inject_mouse(payload[:6])

    @staticmethod
    def _resolve_local_ip() -> str:
        try:
            s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            s.connect(("8.8.8.8", 80))
            ip = s.getsockname()[0]
            s.close()
            return ip
        except Exception:
            return "127.0.0.1"


class _UDPProtocol(asyncio.DatagramProtocol):
    def __init__(self, callback):
        self._callback = callback

    def datagram_received(self, data: bytes, addr: tuple) -> None:
        self._callback(data, addr)


# ── CLI entry point ─────────────────────────────────────────────────────────

def main() -> None:
    import argparse
    p = argparse.ArgumentParser(
        description="Ozma Agent — make any machine part of your ozma mesh",
        prog="ozma-agent",
    )
    sub = p.add_subparsers(dest="command")

    # `ozma-agent run` (default if no subcommand)
    run_p = sub.add_parser("run", help="Run the agent (default)")
    for sp in [p, run_p]:
        sp.add_argument("--name", default=platform.node(), help="Machine name (default: hostname)")
        sp.add_argument("--port", type=int, default=7331, help="UDP listen port")
        sp.add_argument("--api-port", type=int, default=7382, help="HTTP API port")
        sp.add_argument("--controller", default="", help="Controller URL")
        sp.add_argument("--fps", type=int, default=15, help="Screen capture FPS")
        sp.add_argument("--no-capture", action="store_true", help="Disable screen capture")
        sp.add_argument("--debug", action="store_true")
        # Seat options (always multi-seat capable, default 1 seat)
        sp.add_argument("--seats", type=int, default=None,
                        help="Number of seats (default: 1, or from persisted config)")
        sp.add_argument("--seat-config", default=None,
                        help="Path to seat configuration JSON file")
        sp.add_argument("--seat-profile", default="workstation",
                        choices=["gaming", "workstation", "media", "kiosk"],
                        help="Default seat profile (default: workstation)")
        sp.add_argument("--single-node", action="store_true",
                        help="Legacy single-node mode (no SeatManager)")

    # `ozma-agent install`
    inst_p = sub.add_parser("install", help="Install as background service (auto-start on boot)")
    inst_p.add_argument("--name", default=platform.node(), help="Machine name")
    inst_p.add_argument("--controller", required=True, help="Controller URL")

    # `ozma-agent uninstall`
    sub.add_parser("uninstall", help="Remove background service")

    # `ozma-agent status`
    sub.add_parser("status", help="Check service status")

    args = p.parse_args()

    # Handle service commands
    if args.command == "install":
        from service import install_service
        logging.basicConfig(level=logging.INFO, format="%(message)s")
        ok = install_service(args.name, args.controller)
        print("Installed" if ok else "Install failed")
        sys.exit(0 if ok else 1)
    elif args.command == "uninstall":
        from service import uninstall_service
        logging.basicConfig(level=logging.INFO, format="%(message)s")
        ok = uninstall_service()
        print("Uninstalled" if ok else "Uninstall failed")
        sys.exit(0 if ok else 1)
    elif args.command == "status":
        from service import service_status
        s = service_status()
        print(f"Platform: {s['platform']}")
        print(f"Installed: {'yes' if s['installed'] else 'no'}")
        print(f"Running: {'yes' if s['running'] else 'no'}")
        sys.exit(0)

    logging.basicConfig(
        level=logging.DEBUG if args.debug else logging.INFO,
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
        datefmt="%H:%M:%S",
    )

    single_node = getattr(args, "single_node", False)

    if single_node:
        # Legacy single-node mode (no SeatManager, no dynamic seats)
        node = DesktopSoftNode(
            name=args.name, port=args.port,
            api_port=args.api_port,
            controller_url=args.controller,
            capture_fps=0 if args.no_capture else args.fps,
        )

        loop = asyncio.new_event_loop()

        def _on_signal():
            loop.call_soon_threadsafe(node._stop_event.set)

        if sys.platform != "win32":
            for sig in (signal.SIGINT, signal.SIGTERM):
                loop.add_signal_handler(sig, _on_signal)

        try:
            loop.run_until_complete(node.run())
        except KeyboardInterrupt:
            pass
        finally:
            loop.run_until_complete(node.stop())
            loop.close()
    else:
        # Default: always use SeatManager (supports dynamic seat config from controller)
        from multiseat import SeatManager

        # Determine initial seat count: CLI flag > persisted config > 1
        seat_count = getattr(args, "seats", None)
        if seat_count is None:
            persisted = SeatManager.load_persisted_config()
            if persisted:
                seat_count = persisted.get("seats", 1)
                log.info("Using persisted seat count: %d", seat_count)
            else:
                seat_count = 1

        manager = SeatManager(
            controller_url=args.controller,
            base_udp_port=args.port,
            base_api_port=args.api_port,
            seat_count=seat_count,
            seat_config_path=getattr(args, "seat_config", None),
            profile_name=getattr(args, "seat_profile", "workstation"),
            machine_name=args.name,
        )

        loop = asyncio.new_event_loop()

        def _on_signal():
            loop.call_soon_threadsafe(manager._stop_event.set)

        if sys.platform != "win32":
            for sig in (signal.SIGINT, signal.SIGTERM):
                loop.add_signal_handler(sig, _on_signal)

        try:
            loop.run_until_complete(manager.start())
        except KeyboardInterrupt:
            pass
        finally:
            loop.run_until_complete(manager.stop())
            loop.close()


if __name__ == "__main__":
    main()
