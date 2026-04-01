#!/usr/bin/python3
# SPDX-License-Identifier: GPL-2.0-or-later
"""
Ozma Proxmox display service — per-VM display capture + input.

Started by systemd as ozma-display@VMID.service. Provides:
  1. KVMFR shared memory frame capture (Looking Glass format)
  2. D-Bus keyboard + mouse input injection
  3. HTTP API for the controller to pull frames and send input
  4. Auto-registration with the ozma controller

For VMs with emulated GPU: the QEMU kvmfr display backend writes
frames to SHM. This service reads them.

For VMs with GPU passthrough: the guest-side Looking Glass Host
writes frames to the same SHM region.

Either way, this service reads frames and serves them to the controller.
"""

from __future__ import annotations

import asyncio
import io
import json
import logging
import os
import sys
import time
from pathlib import Path

# Add ozma softnode to path for the Looking Glass reader
sys.path.insert(0, str(Path(__file__).parent.parent.parent / "lib" / "ozma-proxmox"))
sys.path.insert(0, str(Path(__file__).parent))

from aiohttp import web

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s %(levelname)s %(name)s: %(message)s",
    datefmt="%H:%M:%S",
)
log = logging.getLogger("ozma.proxmox.display")


class VMDisplayService:
    """Display capture + input for a single VM."""

    def __init__(self, vmid: int) -> None:
        self.vmid = vmid
        self.name = f"vm{vmid}"
        self.shm_path = f"/dev/shm/ozma-vm{vmid}"
        self.qmp_path = f"/var/run/ozma/vm{vmid}-ctrl.qmp"
        self.api_port = 7390 + vmid
        self.hid_port = 7340 + vmid

        self._width = 0
        self._height = 0
        self._latest_frame: bytes | None = None
        self._frame_count = 0
        self._display_type = "none"  # kvmfr, dbus, or none

    async def start(self) -> None:
        """Start display capture and HTTP API."""
        # Try KVMFR first (works for both emulated and passthrough GPU)
        if os.path.exists(self.shm_path):
            try:
                from looking_glass import LookingGlassCapture
                self._lg = LookingGlassCapture(self.name, shm_path=self.shm_path)
                if await self._lg.start():
                    self._display_type = "kvmfr"
                    log.info("VM %d: KVMFR display from %s", self.vmid, self.shm_path)
                    asyncio.create_task(self._kvmfr_capture_loop(), name=f"kvmfr-{self.vmid}")
            except ImportError:
                log.debug("looking_glass module not available")

        # Fallback: D-Bus display
        if self._display_type == "none":
            try:
                from qemu_display import QEMUDBusConsole
                consoles = await QEMUDBusConsole.enumerate_consoles()
                if consoles:
                    self._dbus_console = QEMUDBusConsole(consoles[0])
                    if await self._dbus_console.connect():
                        self._display_type = "dbus"
                        self._width = self._dbus_console.width
                        self._height = self._dbus_console.height
                        log.info("VM %d: D-Bus display %dx%d", self.vmid,
                                 self._width, self._height)
            except ImportError:
                pass

        if self._display_type == "none":
            log.warning("VM %d: no display source available", self.vmid)

        # Start HTTP API
        await self._start_api()

    async def _kvmfr_capture_loop(self) -> None:
        """Continuously read frames from KVMFR shared memory."""
        while True:
            try:
                frame = await self._lg.get_frame_jpeg()
                if frame:
                    self._latest_frame = frame
                    self._frame_count += 1
                    if self._frame_count <= 3:
                        log.info("VM %d: KVMFR frame %d, %d bytes",
                                 self.vmid, self._frame_count, len(frame))
            except Exception as e:
                log.debug("KVMFR read error: %s", e)
            await asyncio.sleep(1.0 / 30)  # 30 fps

    async def _start_api(self) -> None:
        """HTTP API for controller to access display + input."""
        app = web.Application()

        async def health(_):
            return web.json_response({"ok": True, "vmid": self.vmid})

        async def display_info(_):
            return web.json_response({
                "vmid": self.vmid,
                "width": self._width,
                "height": self._height,
                "type": self._display_type,
                "frame_count": self._frame_count,
            })

        async def snapshot(_):
            if self._latest_frame:
                return web.Response(body=self._latest_frame, content_type="image/jpeg")
            # Fallback: QMP screendump
            if self._display_type == "dbus":
                frame = await self._dbus_console.capture_frame_qmp(self.qmp_path)
                if frame:
                    return web.Response(body=frame, content_type="image/jpeg")
            return web.json_response({"error": "no frame"}, status=503)

        async def mjpeg(request):
            response = web.StreamResponse(headers={
                "Content-Type": "multipart/x-mixed-replace; boundary=frame",
            })
            await response.prepare(request)
            while True:
                if self._latest_frame:
                    await response.write(
                        b"--frame\r\nContent-Type: image/jpeg\r\n\r\n"
                        + self._latest_frame + b"\r\n"
                    )
                await asyncio.sleep(1.0 / 15)

        async def input_key(request):
            body = await request.json()
            keycode = body.get("keycode", 0)
            down = body.get("down", True)
            if self._display_type == "dbus" and hasattr(self, '_dbus_console'):
                if down:
                    self._dbus_console.key_press(keycode)
                else:
                    self._dbus_console.key_release(keycode)
            return web.json_response({"ok": True})

        async def input_mouse(request):
            body = await request.json()
            x, y = body.get("x", 0), body.get("y", 0)
            action = body.get("action", "move")
            button = body.get("button", 0)
            if self._display_type == "dbus" and hasattr(self, '_dbus_console'):
                if action == "move":
                    self._dbus_console.mouse_move(x, y)
                elif action == "click":
                    self._dbus_console.mouse_click(x, y, button)
                elif action == "press":
                    self._dbus_console.mouse_move(x, y)
                    self._dbus_console.mouse_press(button)
                elif action == "release":
                    self._dbus_console.mouse_release(button)
            return web.json_response({"ok": True})

        app.router.add_get("/health", health)
        app.router.add_get("/display/info", display_info)
        app.router.add_get("/display/snapshot", snapshot)
        app.router.add_get("/display/mjpeg", mjpeg)
        app.router.add_post("/input/key", input_key)
        app.router.add_post("/input/mouse", input_mouse)

        runner = web.AppRunner(app)
        await runner.setup()
        site = web.TCPSite(runner, "0.0.0.0", self.api_port)
        await site.start()
        log.info("VM %d: HTTP API on port %d", self.vmid, self.api_port)


async def main():
    if len(sys.argv) < 2:
        print(f"Usage: {sys.argv[0]} VMID", file=sys.stderr)
        sys.exit(1)

    vmid = int(sys.argv[1])
    service = VMDisplayService(vmid)
    await service.start()

    # Run until killed
    stop = asyncio.Event()
    import signal
    for sig in (signal.SIGTERM, signal.SIGINT):
        asyncio.get_event_loop().add_signal_handler(sig, stop.set)
    await stop.wait()


if __name__ == "__main__":
    asyncio.run(main())
