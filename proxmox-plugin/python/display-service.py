#!/usr/bin/python3
# SPDX-License-Identifier: GPL-2.0-or-later
"""
Ozma Proxmox display service — per-VM display capture + input.

Started by systemd as ozma-display@VMID.service. Provides:
  1. D-Bus p2p framebuffer capture via QMP add_client (RegisterListener)
  2. KVMFR shared memory frame capture (Looking Glass format)
  3. QMP input injection (input-send-event)
  4. HTTP API for the controller to pull frames and send input
  5. Auto-registration with the ozma controller

Display capture priority:
  1. D-Bus RegisterListener (real-time push, ~30fps)
  2. KVMFR shared memory (Looking Glass compatible)
  3. QMP screendump (fallback, ~2fps)
"""

from __future__ import annotations

import asyncio
import io
import json
import logging
import os
import struct
import sys
import time
from pathlib import Path

# Add ozma modules to path
_lib_dir = Path(__file__).parent.parent / "lib" / "ozma-proxmox"
if _lib_dir.exists():
    sys.path.insert(0, str(_lib_dir))
sys.path.insert(0, str(Path(__file__).parent))

from aiohttp import web

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s %(levelname)s %(name)s: %(message)s",
    datefmt="%H:%M:%S",
)
log = logging.getLogger("ozma.proxmox.display")


class VMDisplayService:
    """Display capture + input for a single Proxmox VM."""

    def __init__(self, vmid: int) -> None:
        self.vmid = vmid
        self.name = f"vm{vmid}"
        self.shm_path = f"/dev/shm/ozma-vm{vmid}"
        # Ozma's dedicated QMP socket (added by the Perl hook)
        self.qmp_path = f"/var/run/ozma/vm{vmid}-ctrl.qmp"
        # Fallback: Proxmox native QMP (exclusive — may conflict)
        self.qmp_path_proxmox = f"/var/run/qemu-server/{vmid}.qmp"
        self.api_port = 7390 + vmid
        self.hid_port = 7340 + vmid

        self._width = 0
        self._height = 0
        self._latest_frame: bytes | None = None
        self._frame_count = 0
        self._display_type = "none"
        self._dbus_client = None

    async def start(self) -> None:
        """Start display capture and HTTP API."""
        # Try D-Bus p2p via QMP add_client (real-time framebuffer push)
        qmp = self.qmp_path if os.path.exists(self.qmp_path) else self.qmp_path_proxmox
        if os.path.exists(qmp):
            self.qmp_path = qmp  # use whichever exists
            try:
                from dbus_display import DBusDisplayClient
                self._dbus_client = DBusDisplayClient(self.qmp_path)
                if await self._dbus_client.connect():
                    self._display_type = "dbus-p2p"
                    self._width = self._dbus_client.width
                    self._height = self._dbus_client.height
                    log.info("VM %d: D-Bus p2p display %dx%d (RegisterListener)",
                             self.vmid, self._width, self._height)
                else:
                    self._dbus_client = None
            except Exception as e:
                log.debug("D-Bus p2p failed: %s", e)
                self._dbus_client = None

        # Fallback: KVMFR shared memory
        if self._display_type == "none" and os.path.exists(self.shm_path):
            try:
                from looking_glass import LookingGlassCapture
                self._lg = LookingGlassCapture(self.name, shm_path=self.shm_path)
                if await self._lg.start():
                    self._display_type = "kvmfr"
                    log.info("VM %d: KVMFR display from %s", self.vmid, self.shm_path)
                    asyncio.create_task(self._kvmfr_capture_loop(), name=f"kvmfr-{self.vmid}")
            except ImportError:
                log.debug("looking_glass module not available")

        # Fallback: QMP screendump
        if self._display_type == "none" and os.path.exists(self.qmp_path):
            self._display_type = "qmp-screendump"
            log.info("VM %d: QMP screendump fallback", self.vmid)

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
            except Exception as e:
                log.debug("KVMFR read error: %s", e)
            await asyncio.sleep(1.0 / 30)

    async def _qmp_screendump(self) -> bytes | None:
        """Capture via QMP screendump (slow fallback)."""
        tmp = f"/dev/shm/ozma-snap-{self.vmid}.ppm"
        try:
            proc = await asyncio.create_subprocess_exec(
                "qm", "monitor", str(self.vmid),
                stdin=asyncio.subprocess.PIPE,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.DEVNULL,
            )
            stdout, _ = await asyncio.wait_for(
                proc.communicate(f"screendump {tmp}\n".encode()), timeout=5
            )
            if os.path.exists(tmp):
                from PIL import Image
                img = Image.open(tmp)
                self._width = img.width
                self._height = img.height
                buf = io.BytesIO()
                img.convert("RGB").save(buf, format="JPEG", quality=75)
                return buf.getvalue()
        except Exception as e:
            log.debug("QMP screendump failed: %s", e)
        finally:
            try:
                os.unlink(tmp)
            except OSError:
                pass
        return None

    async def _start_api(self) -> None:
        """HTTP API for controller to access display + input."""
        app = web.Application()

        async def health(_):
            return web.json_response({"ok": True, "vmid": self.vmid})

        async def display_info(_):
            dc = self._dbus_client
            return web.json_response({
                "vmid": self.vmid,
                "width": dc.width if dc else self._width,
                "height": dc.height if dc else self._height,
                "type": self._display_type,
                "frame_count": dc.frame_count if dc else self._frame_count,
            })

        async def snapshot(_):
            # D-Bus p2p (real-time)
            if self._dbus_client and self._dbus_client.connected and self._dbus_client.latest_frame:
                return web.Response(body=self._dbus_client.latest_frame, content_type="image/jpeg")
            # KVMFR
            if self._latest_frame:
                return web.Response(body=self._latest_frame, content_type="image/jpeg")
            # QMP screendump
            frame = await self._qmp_screendump()
            if frame:
                return web.Response(body=frame, content_type="image/jpeg")
            return web.json_response({"error": "no frame"}, status=503)

        async def mjpeg(request):
            response = web.StreamResponse(headers={
                "Content-Type": "multipart/x-mixed-replace; boundary=frame",
            })
            await response.prepare(request)
            while True:
                frame = None
                if self._dbus_client and self._dbus_client.connected:
                    frame = self._dbus_client.latest_frame
                elif self._latest_frame:
                    frame = self._latest_frame
                if frame:
                    await response.write(
                        b"--frame\r\nContent-Type: image/jpeg\r\n\r\n"
                        + frame + b"\r\n"
                    )
                await asyncio.sleep(1.0 / 15)

        async def input_key(request):
            body = await request.json()
            keycode = body.get("keycode", 0)
            down = body.get("down", True)
            # D-Bus p2p input (sub-ms)
            if self._dbus_client and self._dbus_client.connected:
                if down:
                    await self._dbus_client.key_press(keycode)
                else:
                    await self._dbus_client.key_release(keycode)
                return web.json_response({"ok": True})
            # QMP input-send-event fallback
            qcode = _EVDEV_TO_QCODE.get(keycode)
            if qcode:
                cmd = json.dumps({
                    "execute": "input-send-event",
                    "arguments": {"events": [{
                        "type": "key",
                        "data": {"down": down, "key": {"type": "qcode", "data": qcode}},
                    }]},
                })
                proc = await asyncio.create_subprocess_exec(
                    "qm", "monitor", str(self.vmid),
                    stdin=asyncio.subprocess.PIPE,
                    stdout=asyncio.subprocess.DEVNULL,
                    stderr=asyncio.subprocess.DEVNULL,
                )
                await proc.communicate(f"{cmd}\n".encode())
            return web.json_response({"ok": True})

        async def input_mouse(request):
            body = await request.json()
            x, y = body.get("x", 0), body.get("y", 0)
            action = body.get("action", "move")
            button = body.get("button", 0)
            if self._dbus_client and self._dbus_client.connected:
                if action == "move":
                    await self._dbus_client.mouse_move(x, y)
                elif action == "click":
                    await self._dbus_client.mouse_click(x, y, button)
                elif action == "press":
                    await self._dbus_client.mouse_move(x, y)
                    await self._dbus_client.mouse_press(button)
                elif action == "release":
                    await self._dbus_client.mouse_release(button)
            return web.json_response({"ok": True})

        # WebRTC signaling
        async def webrtc_offer(request):
            body = await request.json()
            if not self._dbus_client or not self._dbus_client.connected:
                return web.json_response({"error": "no display"}, status=503)
            try:
                import aiortc.codecs.h264 as _h264mod
                _h264mod.DEFAULT_BITRATE = 4_000_000
                _h264mod.MAX_BITRATE = 50_000_000
                from aiortc import RTCPeerConnection, RTCSessionDescription
                from webrtc_stream import FramebufferVideoTrack

                offer = RTCSessionDescription(sdp=body["sdp"], type=body["type"])
                pc = RTCPeerConnection()
                track = FramebufferVideoTrack(self._dbus_client, fps=30)
                pc.addTrack(track)
                await pc.setRemoteDescription(offer)
                answer = await pc.createAnswer()
                await pc.setLocalDescription(answer)
                return web.json_response({
                    "sdp": pc.localDescription.sdp,
                    "type": pc.localDescription.type,
                })
            except ImportError:
                return web.json_response({"error": "aiortc not available"}, status=503)
            except Exception as e:
                return web.json_response({"error": str(e)}, status=500)

        # Serve the Ozma Console HTML
        console_dir = Path(__file__).parent / "console"
        if not console_dir.exists():
            console_dir = Path("/usr/lib/ozma-proxmox/console")

        async def console_page(_):
            html = console_dir / "index.html"
            if html.exists():
                return web.FileResponse(html)
            return web.Response(text="Console not installed", status=404)

        app.router.add_get("/health", health)
        app.router.add_get("/display/info", display_info)
        app.router.add_get("/display/snapshot", snapshot)
        app.router.add_get("/display/mjpeg", mjpeg)
        app.router.add_post("/input/key", input_key)
        app.router.add_post("/input/mouse", input_mouse)
        app.router.add_post("/webrtc/offer", webrtc_offer)
        app.router.add_get("/console/", console_page)
        app.router.add_get("/console", console_page)

        # CORS headers for cross-origin console access
        try:
            import aiohttp_cors
            cors = aiohttp_cors.setup(app, defaults={
                "*": aiohttp_cors.ResourceOptions(
                    allow_credentials=True, expose_headers="*",
                    allow_headers="*", allow_methods="*",
                ),
            })
            for route in list(app.router.routes()):
                try:
                    cors.add(route)
                except ValueError:
                    pass
        except ImportError:
            pass  # aiohttp_cors not required

        runner = web.AppRunner(app)
        await runner.setup()
        site = web.TCPSite(runner, "0.0.0.0", self.api_port)
        await site.start()
        log.info("VM %d: HTTP API on port %d", self.vmid, self.api_port)


# Keycode mapping for QMP fallback
_EVDEV_TO_QCODE = {
    1: "esc", 2: "1", 3: "2", 4: "3", 5: "4", 6: "5", 7: "6",
    8: "7", 9: "8", 10: "9", 11: "0", 14: "backspace", 15: "tab",
    16: "q", 17: "w", 18: "e", 19: "r", 20: "t", 21: "y", 22: "u",
    23: "i", 24: "o", 25: "p", 28: "ret", 29: "ctrl", 30: "a", 31: "s",
    32: "d", 33: "f", 34: "g", 35: "h", 36: "j", 37: "k", 38: "l",
    42: "shift", 44: "z", 45: "x", 46: "c", 47: "v", 48: "b", 49: "n",
    50: "m", 54: "shift_r", 56: "alt", 57: "spc",
    97: "ctrl_r", 100: "alt_r", 103: "up", 105: "left", 106: "right", 108: "down",
}


async def main():
    if len(sys.argv) < 2:
        print(f"Usage: {sys.argv[0]} VMID", file=sys.stderr)
        sys.exit(1)

    vmid = int(sys.argv[1])
    service = VMDisplayService(vmid)
    await service.start()

    stop = asyncio.Event()
    import signal
    for sig in (signal.SIGTERM, signal.SIGINT):
        asyncio.get_event_loop().add_signal_handler(sig, stop.set)
    await stop.wait()


if __name__ == "__main__":
    asyncio.run(main())
