# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
QEMU D-Bus display — first-class framebuffer + input for virtual nodes.

Connects to QEMU's D-Bus display interface (org.qemu.Display1) and provides:
  - Direct framebuffer access via Console.RegisterListener
  - Native keyboard input via Keyboard.Press/Release
  - Native mouse input via Mouse.SetAbsPosition/Press/Release
  - Resolution change detection
  - Multi-console support (Console_0, Console_1, ... for multi-monitor VMs)

This replaces VNC, SPICE, QMP input-send-event, and evdev input-linux
with a single unified interface purpose-built for VM display control.

QEMU command line:
  -display dbus                              # uses session bus
  -display dbus,p2p=yes,addr=unix:path=...   # private p2p socket

Multi-monitor:
  -device virtio-gpu-pci,max_outputs=2       # two display heads
  Each head appears as Console_0, Console_1, etc. on D-Bus.

Usage:
  # Single display (default)
  display = QEMUDBusConsole()
  await display.connect()

  # Multi-monitor — enumerate all consoles
  consoles = await QEMUDBusConsole.enumerate_consoles()
  displays = [QEMUDBusConsole(i) for i in consoles]
  for d in displays:
      await d.connect()
"""

from __future__ import annotations

import asyncio
import io
import logging
import os
import struct
import time
from typing import Any, AsyncIterator

log = logging.getLogger("ozma.softnode.qemu_display")

DBUS_DEST = "org.qemu"
DBUS_DISPLAY_PATH = "/org/qemu/Display1"


class QEMUDBusConsole:
    """
    A single QEMU display console via D-Bus.

    Each console represents one display head. Multi-monitor VMs have
    Console_0, Console_1, etc. Create one instance per console.
    """

    KBD_IFACE = "org.qemu.Display1.Keyboard"
    MOUSE_IFACE = "org.qemu.Display1.Mouse"
    CONSOLE_IFACE = "org.qemu.Display1.Console"

    def __init__(self, console_index: int = 0) -> None:
        self.console_index = console_index
        self.console_path = f"{DBUS_DISPLAY_PATH}/Console_{console_index}"
        self._connected = False
        self._width = 0
        self._height = 0
        self._label = ""
        self._latest_frame: bytes | None = None

    async def connect(self) -> bool:
        """Connect to this console on QEMU's D-Bus display."""
        import shutil
        if not shutil.which("gdbus"):
            log.warning("gdbus not found — D-Bus display unavailable")
            return False

        try:
            result = await self._gdbus_call(
                "org.freedesktop.DBus.Properties", "GetAll",
                args=[self.CONSOLE_IFACE],
            )
            if result and "Width" in result:
                self._width = self._parse_variant_uint(result, "Width")
                self._height = self._parse_variant_uint(result, "Height")
                self._label = self._parse_variant_string(result, "Label")
                self._connected = True
                log.info("D-Bus console %d connected: %s %dx%d",
                         self.console_index, self._label, self._width, self._height)
                return True
        except Exception as e:
            log.debug("D-Bus console %d connect failed: %s", self.console_index, e)
        return False

    @classmethod
    async def enumerate_consoles(cls) -> list[int]:
        """Discover which Console_N paths exist on the D-Bus.

        Returns a sorted list of console indices (e.g., [0, 1] for two displays).
        """
        import shutil
        import subprocess
        if not shutil.which("gdbus"):
            return []

        try:
            result = subprocess.run(
                ["gdbus", "introspect", "--session",
                 "--dest", DBUS_DEST,
                 "--object-path", DBUS_DISPLAY_PATH],
                capture_output=True, text=True, timeout=3,
            )
            # Parse introspection XML for Console_N child nodes
            import re
            consoles = sorted(
                int(m.group(1))
                for m in re.finditer(r'node name="Console_(\d+)"', result.stdout)
            )
            if consoles:
                log.info("D-Bus display: found %d console(s): %s", len(consoles), consoles)
            return consoles
        except Exception as e:
            log.debug("D-Bus console enumeration failed: %s", e)
            return []

    @property
    def connected(self) -> bool:
        return self._connected

    @property
    def width(self) -> int:
        return self._width

    @property
    def height(self) -> int:
        return self._height

    @property
    def label(self) -> str:
        return self._label

    # ── Keyboard ──────────────────────────────────────────────────────

    def key_press(self, keycode: int) -> None:
        if not self._connected:
            return
        self._gdbus_fire(self.KBD_IFACE, "Press", f"uint32 {keycode}")

    def key_release(self, keycode: int) -> None:
        if not self._connected:
            return
        self._gdbus_fire(self.KBD_IFACE, "Release", f"uint32 {keycode}")

    def key_tap(self, keycode: int) -> None:
        self.key_press(keycode)
        time.sleep(0.02)
        self.key_release(keycode)

    # ── Mouse ─────────────────────────────────────────────────────────

    def mouse_move(self, x: int, y: int) -> None:
        if not self._connected:
            return
        self._gdbus_fire(self.MOUSE_IFACE, "SetAbsPosition", f"uint32 {x} uint32 {y}")

    def mouse_press(self, button: int = 0) -> None:
        if not self._connected:
            return
        self._gdbus_fire(self.MOUSE_IFACE, "Press", f"uint32 {button}")

    def mouse_release(self, button: int = 0) -> None:
        if not self._connected:
            return
        self._gdbus_fire(self.MOUSE_IFACE, "Release", f"uint32 {button}")

    def mouse_click(self, x: int, y: int, button: int = 0) -> None:
        self.mouse_move(x, y)
        time.sleep(0.01)
        self.mouse_press(button)
        time.sleep(0.03)
        self.mouse_release(button)

    # ── Frame capture ─────────────────────────────────────────────────

    async def get_frame(self) -> bytes | None:
        return self._latest_frame

    async def capture_frame_qmp(self, qmp_socket: str) -> bytes | None:
        """Capture via QMP screendump (interim until D-Bus listener)."""
        import json
        tmp = f"/dev/shm/ozma-frame-{os.getpid()}-{self.console_index}.png"
        try:
            r, w = await asyncio.open_unix_connection(qmp_socket)
            await r.readline()
            w.write(json.dumps({"execute": "qmp_capabilities"}).encode() + b"\n")
            await w.drain()
            await r.readline()
            w.write(json.dumps({"execute": "screendump", "arguments": {
                "filename": tmp, "format": "png", "device": f"console{self.console_index}",
            }}).encode() + b"\n")
            await w.drain()
            for _ in range(5):
                line = await asyncio.wait_for(r.readline(), timeout=2)
                resp = json.loads(line)
                if "return" in resp or "error" in resp:
                    break
            w.close()

            if os.path.exists(tmp):
                from PIL import Image
                img = Image.open(tmp)
                self._width = img.width
                self._height = img.height
                buf = io.BytesIO()
                img.convert("RGB").save(buf, format="JPEG", quality=75)
                self._latest_frame = buf.getvalue()
                return self._latest_frame
        except Exception as e:
            log.debug("Frame capture error (console %d): %s", self.console_index, e)
        finally:
            try:
                os.unlink(tmp)
            except OSError:
                pass
        return None

    # ── gdbus helpers ─────────────────────────────────────────────────

    def _gdbus_fire(self, iface: str, method: str, args: str) -> None:
        """Fire-and-forget D-Bus call via gdbus CLI. Non-blocking."""
        import subprocess
        cmd = [
            "gdbus", "call", "--session",
            "--dest", DBUS_DEST,
            "--object-path", self.console_path,
            "--method", f"{iface}.{method}",
        ] + args.split()
        try:
            subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        except Exception as e:
            log.debug("gdbus fire failed: %s", e)

    async def _gdbus_call(self, iface: str, method: str | None, args: list[str] | None = None,
                           **kw) -> str | None:
        """Blocking D-Bus call, returns stdout."""
        import subprocess
        actual_method = kw.get("method", f"{iface}.{method}")
        cmd = [
            "gdbus", "call", "--session",
            "--dest", DBUS_DEST,
            "--object-path", self.console_path,
            "--method", actual_method,
        ]
        if args:
            cmd += args
        try:
            result = subprocess.run(cmd, capture_output=True, text=True, timeout=3)
            return result.stdout.strip()
        except Exception:
            return None

    @staticmethod
    def _parse_variant_uint(text: str, key: str) -> int:
        import re
        m = re.search(rf"'{key}':\s*<uint32\s+(\d+)>", text)
        return int(m.group(1)) if m else 0

    @staticmethod
    def _parse_variant_string(text: str, key: str) -> str:
        import re
        m = re.search(rf"'{key}':\s*<'([^']*)'>" , text)
        return m.group(1) if m else ""


# Backward compatibility alias
QEMUDBusDisplay = QEMUDBusConsole
