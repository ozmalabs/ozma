# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
#!/usr/bin/env python3
"""
Ozma Soft Node — virtual compute node with evdev HID injection.

Announces itself via mDNS (_ozma._udp.local.), listens for tinynode HID
packets from the Controller, and injects them into a QEMU VM via evdev
(input-linux). QMP is no longer required for HID — evdev devices are
created via uinput and QEMU reads them directly.

Power control uses libvirt API (preferred) or QMP (fallback).

Usage:
  python softnode/soft_node.py --name vm1 --port 7332
  python softnode/soft_node.py --name vm2 --port 7333 --qmp /tmp/ozma-vm2.qmp

Each instance needs a distinct --port since both run on the same host.
The Controller discovers them via mDNS and routes HID to whichever port is
in the active scenario's NodeInfo.

The mDNS instance name becomes the node_id in the Controller:
  "vm1._ozma._udp.local."
"""

import argparse
import asyncio
import json
import logging
import os
import signal
import struct
import socket
import sys
from pathlib import Path

from aiohttp import web
from zeroconf import ServiceInfo
from zeroconf.asyncio import AsyncZeroconf

# Allow running from the repo root or from softnode/ directly
sys.path.insert(0, str(Path(__file__).parent))

from hid_to_qmp import KeyboardReportState, MouseReportState
from qmp_client import QMPClient
from qemu_display import QEMUDBusConsole
from dbus_display import DBusDisplayClient
from looking_glass import LookingGlassCapture
from virtual_capture import VirtualCapture
from connect_client import NodeConnectClient
from prometheus_metrics import collect_soft
from evdev_input import VirtualKeyboard, VirtualMouse, EvdevHIDTranslator

log = logging.getLogger("ozma.softnode")


# ── Encoder helpers ───────────────────────────────────────────────────────────

def _encoder_args_for(encoder: str, bitrate: str, quality: int, latency: str) -> list[str]:
    """Build ffmpeg -c:v … args for the given encoder and parameters."""
    if "nvenc" in encoder:
        preset = "p1" if latency == "realtime" else "p5"
        args = ["-c:v", encoder, "-preset", preset, "-tune", "ull", "-rc", "cbr",
                "-b:v", bitrate or "3M"]
    elif "vaapi" in encoder:
        qp = str(quality) if quality > 0 else "24"
        args = ["-c:v", encoder, "-qp", qp]
        if bitrate and quality < 0:
            args = ["-c:v", encoder, "-b:v", bitrate]
    elif "qsv" in encoder:
        preset = "veryfast" if latency == "realtime" else "medium"
        args = ["-c:v", encoder, "-preset", preset]
        if bitrate:
            args += ["-b:v", bitrate]
    elif "v4l2m2m" in encoder:
        args = ["-c:v", encoder]
        if bitrate:
            args += ["-b:v", bitrate]
    elif "libx265" in encoder:
        args = ["-c:v", "libx265", "-preset", "ultrafast",
                "-tune", "zerolatency", "-x265-params", "log-level=error"]
        if quality > 0:
            args += ["-crf", str(quality)]
        elif bitrate:
            args += ["-b:v", bitrate]
    else:  # libx264 / default
        args = ["-c:v", "libx264", "-preset", "ultrafast", "-tune", "zerolatency"]
        if quality > 0:
            args += ["-crf", str(quality)]
        elif bitrate:
            args += ["-b:v", bitrate]
        else:
            args += ["-crf", "23"]
    return args


async def _test_encoder_quick(encoder: str, vaapi_device: str | None = None) -> bool:
    """Quick test encode to verify an encoder is functional."""
    if vaapi_device and "vaapi" in encoder:
        cmd = ["ffmpeg", "-hide_banner", "-loglevel", "error",
               "-init_hw_device", f"vaapi=hw:{vaapi_device}",
               "-filter_hw_device", "hw",
               "-f", "lavfi", "-i", "color=black:size=64x64:rate=1",
               "-vf", "format=nv12,hwupload",
               "-frames:v", "1", "-c:v", encoder, "-f", "null", "-"]
    else:
        cmd = ["ffmpeg", "-hide_banner", "-loglevel", "error",
               "-f", "lavfi", "-i", "color=black:size=64x64:rate=1",
               "-frames:v", "1", "-c:v", encoder, "-f", "null", "-"]
    try:
        proc = await asyncio.create_subprocess_exec(
            *cmd, stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.DEVNULL)
        try:
            await asyncio.wait_for(proc.wait(), timeout=10.0)
        except asyncio.TimeoutError:
            proc.kill()
            return False
        return proc.returncode == 0
    except Exception:
        return False

PROTO_VERSION = 1
MAX_PACKET = 64


class SoftNode:
    def __init__(
        self,
        name: str,
        host: str,
        port: int,
        qmp_path: str = "",
        vnc_host: str | None = None,
        vnc_port: int | None = None,
        vnc_socket: str | None = None,    # VNC unix socket path (overrides host/port)
        capture_device: str | None = None,  # V4L2 device path (skip VNC capture)
        audio_sink: str | None = None,    # PipeWire null sink name for this node
        api_port: int = 0,                # HTTP port for power/status API (0 = auto)
        qmp_input_path: str = "",         # Dedicated input QMP socket (recommended)
        power_backend: "PowerBackend | None" = None,  # libvirt/qmp power control
        vm_guest_ip: str | None = None,   # VM's actual guest IP (advertised via mDNS vm_ip TXT)
    ) -> None:
        self._name = name
        self._host = host
        self._port = port
        self._qmp: QMPClient | None = QMPClient(qmp_path, input_socket_path=qmp_input_path) if qmp_path else None
        self._power = power_backend
        self._vnc_host = vnc_host
        self._vnc_port = vnc_port
        self._vnc_socket = vnc_socket
        self._capture_device = capture_device
        self._audio_sink = audio_sink
        self._api_port = api_port
        self._vm_guest_ip = vm_guest_ip
        # Async D-Bus display client (fast input + framebuffer)
        self._dbus_client: DBusDisplayClient | None = None
        # Direct evdev for input-linux (kernel-level input)
        self._evdev_input_fd: int = -1   # keyboard device FD
        self._evdev_mouse_fd: int = -1   # mouse device FD
        # evdev input (primary HID path — no QMP needed)
        self._evdev_kbd = VirtualKeyboard(name=f"ozma-kbd-{name}")
        self._evdev_mouse = VirtualMouse(name=f"ozma-mouse-{name}")
        self._evdev_translator: EvdevHIDTranslator | None = None
        # QMP HID fallback (only if evdev unavailable)
        self._kbd = KeyboardReportState()
        self._mouse = MouseReportState()
        self._use_evdev = False
        self._stop_event = asyncio.Event()
        # Codec state — modified via /codec API to trigger ffmpeg restart
        self._codec_override: dict | None = None   # None = auto-detect
        self._ffmpeg_hls_proc: asyncio.subprocess.Process | None = None  # current HLS encoder
        self._current_encoder: str = ""   # e.g. "h264_nvenc"
        self._available_encoders: list[str] = []  # probed at startup
        self._display: QEMUDBusConsole | None = None
        self._displays: list[QEMUDBusConsole] = []  # multi-monitor: all consoles
        self._virtual_capture: VirtualCapture | None = None
        if capture_device:
            # External capture device (v4l2loopback fed by the display bridge)
            # No VirtualCapture needed — the device is already producing frames
            pass
        elif vnc_host and vnc_port:
            self._virtual_capture = VirtualCapture(
                vm_name=name, vnc_host=vnc_host, vnc_port=vnc_port,
            )
        service_type = "_ozma._udp.local."
        self._node_id = f"{name}.{service_type}"
        self._connect = NodeConnectClient(
            self._node_id, node_type="soft", hid_port=port,
        )

    async def run(self) -> None:
        # Start evdev input devices (primary HID path)
        kbd_path = self._evdev_kbd.start()
        mouse_path = self._evdev_mouse.start()
        if kbd_path and mouse_path:
            self._evdev_translator = EvdevHIDTranslator(self._evdev_kbd, self._evdev_mouse)
            self._use_evdev = True
            log.info("evdev input: kbd=%s mouse=%s", kbd_path, mouse_path)
        else:
            log.warning("evdev input unavailable — falling back to QMP HID")

        # QMP is optional — only start if configured
        if self._qmp:
            await self._qmp.start()
        elif not self._use_evdev:
            log.error("No HID path available: evdev failed and no QMP configured")

        # Connect to QEMU D-Bus display (keyboard + mouse + framebuffer)
        # Check for ozma private D-Bus first (libvirt VMs provisioned by ozma)
        dbus_bus_sock = f"/run/ozma/dbus/{self._name}-bus.sock"
        # Try D-Bus p2p via QMP add_client (no bus daemon needed)
        # Uses dedicated display QMP socket (separate from input/control)
        qmp_display_sock = f"/run/ozma/qmp/{self._name}-display.sock"
        if os.path.exists(qmp_display_sock):
            # Retry a few times — VM may still be booting
            for attempt in range(5):
                self._dbus_client = DBusDisplayClient(qmp_display_sock)
                if await self._dbus_client.connect():
                    break
                await asyncio.sleep(2)
            if self._dbus_client and self._dbus_client.connected:
                log.info("D-Bus p2p display: %dx%d via QMP add_client",
                         self._dbus_client.width, self._dbus_client.height)
            else:
                self._dbus_client = None
        else:
            # Session bus — enumerate all consoles (demo VMs)
            console_indices = await QEMUDBusConsole.enumerate_consoles()
            if not console_indices:
                console_indices = [0]
            for idx in console_indices:
                console = QEMUDBusConsole(idx)
                if await console.connect():
                    self._displays.append(console)
                    log.info("D-Bus console %d: %dx%d (%s)",
                             idx, console.width, console.height, console.label)
            if self._displays:
                self._display = self._displays[0]

        if self._displays:
            log.info("QEMU D-Bus display: %d console(s) ready", len(self._displays))
        else:
            log.warning("QEMU D-Bus display not available — falling back to QMP")

        # Open evdev service devices for input-linux (if available)
        kbd_state = Path(f"/run/ozma/evdev/{self._name}.kbd")
        if kbd_state.exists():
            try:
                kbd_path = kbd_state.read_text().strip()
                mouse_state = Path(f"/run/ozma/evdev/{self._name}.mouse")
                mouse_path = mouse_state.read_text().strip() if mouse_state.exists() else ""
                self._evdev_input_fd = os.open(kbd_path, os.O_WRONLY | os.O_NONBLOCK)
                if mouse_path:
                    self._evdev_mouse_fd = os.open(mouse_path, os.O_WRONLY | os.O_NONBLOCK)
                log.info("evdev FDs: kbd=%d mouse=%d", self._evdev_input_fd, self._evdev_mouse_fd)
                log.info("evdev input-linux: kbd=%s mouse=%s", kbd_path, mouse_path)
            except Exception as e:
                log.debug("evdev input-linux unavailable: %s", e)

        # Start capture — pick the best available source
        self._hls_dir = Path(f"/tmp/ozma-stream-{self._name}")
        self._hls_dir.mkdir(parents=True, exist_ok=True)
        if self._capture_device:
            asyncio.create_task(
                self._capture_hls(self._capture_device),
                name=f"capture-hls-{self._name}",
            )
            log.info("Capture: %s → HLS", self._capture_device)
        elif self._vnc_socket:
            asyncio.create_task(
                self._capture_hls(self._vnc_socket),
                name=f"capture-hls-{self._name}",
            )
            log.info("Capture: VNC socket %s → HLS", self._vnc_socket)
        elif self._dbus_client and self._dbus_client.connected:
            # D-Bus RegisterListener → ffmpeg → H.264 HLS (real-time, 30+ fps)
            asyncio.create_task(
                self._capture_dbus_hls(),
                name=f"capture-dbus-hls-{self._name}",
            )
            log.info("Capture: D-Bus framebuffer → H.264 HLS for %s", self._name)
        elif self._qmp:
            # QMP screendump → ffmpeg → H.264 HLS (fallback, ~2fps)
            asyncio.create_task(
                self._capture_qmp_hls(),
                name=f"capture-qmp-hls-{self._name}",
            )
            log.info("Capture: QMP screendump → H.264 HLS for %s (slow fallback)", self._name)
        elif self._virtual_capture:
            device_path = await self._virtual_capture.start()
            if device_path:
                log.info("Virtual capture device: %s → %s", self._name, device_path)

        self._runner = await self._start_api()
        await self._announce()
        # Direct registration with controller to ensure all fields arrive
        # (mDNS on busy multi-interface hosts may resolve with stale data).
        # Runs in background — doesn't block startup.
        asyncio.create_task(self._direct_register(), name=f"register-{self._name}")
        # Register with Connect (if token configured). Nodes connect
        # directly — the mesh is visible from Connect even if the
        # controller is offline.
        await self._connect.start(
            capabilities=f"{'evdev' if self._use_evdev else 'qmp'},power",
            version="0.1.0",
            extra={
                "audio_type": "pipewire" if self._audio_sink else "",
                "audio_sink": self._audio_sink or "",
                "vnc_host": self._vnc_host or "",
                "vnc_port": str(self._vnc_port) if self._vnc_port else "",
                "capture_device": (self._virtual_capture.device_path
                                   if self._virtual_capture and self._virtual_capture.device_path
                                   else ""),
            },
        )
        await self._serve()

    async def stop(self) -> None:
        self._stop_event.set()
        await self._connect.stop()
        if self._virtual_capture:
            await self._virtual_capture.stop()
        self._evdev_kbd.stop()
        self._evdev_mouse.stop()

    # --- HTTP API for power control ---

    async def _start_api(self) -> web.AppRunner | None:
        """Start a lightweight HTTP server for power/status endpoints."""
        app = web.Application()

        async def health(_: web.Request) -> web.Response:
            return web.json_response({"ok": True})

        async def connection_state(_: web.Request) -> web.Response:
            return web.json_response(self._connect.state.to_dict())

        async def metrics(_: web.Request) -> web.Response:
            pw = self._power or self._qmp
            if pw:
                status = await pw.query_status()
                vm_status = status.get("status", "unknown") if status else "unknown"
                connected = pw.connected if hasattr(pw, "connected") else True
            else:
                vm_status = "unknown"
                connected = False
            text = collect_soft(
                node_name=self._name,
                connect_client=self._connect,
                qmp_connected=connected,
                vm_status=vm_status,
            )
            return web.Response(text=text, content_type="text/plain; version=0.0.4")

        async def power_state(_: web.Request) -> web.Response:
            pw = self._power or self._qmp
            if pw:
                status = await pw.query_status()
                running = status.get("status") == "running" if status else None
                connected = pw.connected if hasattr(pw, "connected") else True
            else:
                status = None
                running = None
                connected = False
            return web.json_response({
                "available": connected,
                "powered": running,
                "vm_status": status.get("status") if status else "unknown",
            })

        async def power_action(request: web.Request) -> web.Response:
            action = request.match_info["action"]
            pw = self._power or self._qmp
            if not pw:
                return web.json_response(
                    {"ok": False, "error": "No power backend configured"}, status=503
                )
            actions = {
                "on": pw.cont,             # resume / start VM
                "off": pw.system_powerdown, # ACPI power button
                "reset": pw.system_reset,
                "force-off": pw.stop if hasattr(pw, "stop") else pw.system_powerdown,
            }
            fn = actions.get(action)
            if not fn:
                return web.json_response(
                    {"ok": False, "error": f"Unknown action: {action}"}, status=400
                )
            ok = await fn()
            return web.json_response({"ok": ok, "action": action})

        # ── Display + Input via QEMU D-Bus ──────────────────────────────

        _self = self  # capture self for closures — display may connect after API starts
        import os as _os

        async def display_snapshot(_: web.Request) -> web.Response:
            """JPEG snapshot of the VM display."""
            # 1. Async D-Bus client (best — push-based framebuffer, zero I/O)
            if _self._dbus_client and _self._dbus_client.connected and _self._dbus_client.latest_frame:
                return web.Response(body=_self._dbus_client.latest_frame, content_type="image/jpeg")
            # 2. Legacy D-Bus display
            if _self._display and _self._display.connected:
                frame = await _self._display.get_frame()
                if frame:
                    return web.Response(body=frame, content_type="image/jpeg")
            # 3. QMP screendump (fallback)
            if _self._qmp and _self._qmp.connected:
                jpeg = await _screendump_jpeg(_self._qmp._ctrl)
                if jpeg:
                    return web.Response(body=jpeg, content_type="image/jpeg")
            # 4. virsh screenshot (last resort)
            jpeg = await _virsh_screenshot_jpeg(_self._name)
            if jpeg:
                return web.Response(body=jpeg, content_type="image/jpeg")
            return web.json_response({"error": "no display"}, status=503)

        async def _screendump_jpeg(ctrl) -> bytes | None:
            import io as _io
            tmp = f"/dev/shm/ozma-snap-{_os.getpid()}.png"
            try:
                ok = await ctrl.screendump(tmp)
                if ok and _os.path.exists(tmp):
                    from PIL import Image
                    img = Image.open(tmp)
                    buf = _io.BytesIO()
                    img.convert("RGB").save(buf, format="JPEG", quality=75)
                    return buf.getvalue()
            except Exception:
                pass
            finally:
                try: _os.unlink(tmp)
                except OSError: pass
            return None

        async def _virsh_screenshot_jpeg(vm_name: str) -> bytes | None:
            import io as _io
            tmp = f"/dev/shm/ozma-virsh-{_os.getpid()}.png"
            try:
                proc = await asyncio.create_subprocess_exec(
                    "virsh", "screenshot", vm_name, tmp,
                    stdout=asyncio.subprocess.DEVNULL,
                    stderr=asyncio.subprocess.DEVNULL,
                )
                await proc.communicate()
                if proc.returncode == 0 and _os.path.exists(tmp):
                    from PIL import Image
                    img = Image.open(tmp)
                    buf = _io.BytesIO()
                    img.convert("RGB").save(buf, format="JPEG", quality=75)
                    return buf.getvalue()
            except Exception:
                pass
            finally:
                try: _os.unlink(tmp)
                except OSError: pass
            return None

        async def display_mjpeg(_: web.Request) -> web.StreamResponse:
            """MJPEG stream of the VM display."""
            response = web.StreamResponse(
                status=200,
                headers={"Content-Type": "multipart/x-mixed-replace; boundary=frame"},
            )
            await response.prepare(_)
            while True:
                frame = None
                # D-Bus display
                if _self._display and _self._display.connected:
                    frame = await _self._display.get_frame()
                # QMP screendump (direct socket)
                if not frame and _self._qmp and _self._qmp.connected:
                    frame = await _screendump_jpeg(_self._qmp._ctrl)
                # virsh screenshot (fallback)
                if not frame:
                    frame = await _virsh_screenshot_jpeg(_self._name)
                if frame:
                    await response.write(
                        b"--frame\r\nContent-Type: image/jpeg\r\n\r\n" + frame + b"\r\n"
                    )
                await asyncio.sleep(1.0 / 15)  # 15 fps

        async def display_info(_: web.Request) -> web.Response:
            """Display resolution and status."""
            return web.json_response({
                "width": _self._display.width if _self._display else 0,
                "height": _self._display.height if _self._display else 0,
                "connected": _display.connected if _self._display else False,
                "type": "dbus" if _self._display and _self._display.connected else "none",
            })

        # ── evdev raw write (for input-linux) ─────────────────────────────
        def _evdev_write_key(fd: int, keycode: int, down: bool):
            """Write a key event directly to an evdev device FD."""
            import time as _t
            sec = int(_t.time())
            usec = int((_t.time() % 1) * 1000000)
            ev = struct.pack('llHHi', sec, usec, 1, keycode, 1 if down else 0)
            syn = struct.pack('llHHi', sec, usec, 0, 0, 0)
            os.write(fd, ev + syn)

        # ── evdev keycode → QMP qcode mapping ───────────────────────────
        # QMP input-send-event uses QEMU "qcode" names, not evdev numbers.
        _EVDEV_TO_QCODE = {
            1: "esc", 2: "1", 3: "2", 4: "3", 5: "4", 6: "5", 7: "6",
            8: "7", 9: "8", 10: "9", 11: "0", 12: "minus", 13: "equal",
            14: "backspace", 15: "tab", 16: "q", 17: "w", 18: "e", 19: "r",
            20: "t", 21: "y", 22: "u", 23: "i", 24: "o", 25: "p",
            26: "bracket_left", 27: "bracket_right", 28: "ret", 29: "ctrl",
            30: "a", 31: "s", 32: "d", 33: "f", 34: "g", 35: "h",
            36: "j", 37: "k", 38: "l", 39: "semicolon", 40: "apostrophe",
            41: "grave_accent", 42: "shift", 43: "backslash",
            44: "z", 45: "x", 46: "c", 47: "v", 48: "b", 49: "n",
            50: "m", 51: "comma", 52: "dot", 53: "slash", 54: "shift_r",
            56: "alt", 57: "spc", 58: "caps_lock",
            59: "f1", 60: "f2", 61: "f3", 62: "f4", 63: "f5", 64: "f6",
            65: "f7", 66: "f8", 67: "f9", 68: "f10", 87: "f11", 88: "f12",
            97: "ctrl_r", 100: "alt_r", 102: "home", 103: "up",
            104: "pgup", 105: "left", 106: "right", 107: "end",
            108: "down", 109: "pgdn", 110: "insert", 111: "delete",
            125: "meta_l", 126: "meta_r",
        }

        async def _qmp_input_key(vm_name: str, keycode: int, down: bool) -> bool:
            """Send a key event via the direct QMP socket (1-2ms latency)."""
            qcode = _EVDEV_TO_QCODE.get(keycode)
            if not qcode or not _self._qmp or not _self._qmp.connected:
                return False
            resp = await _self._qmp._ctrl.send_command({
                "execute": "input-send-event",
                "arguments": {"events": [{
                    "type": "key",
                    "data": {"down": down, "key": {"type": "qcode", "data": qcode}},
                }]},
            })
            return resp is not None and "return" in resp

        async def _qmp_input_mouse(vm_name: str, x: int, y: int,
                                    button: int = 0, down: bool | None = None) -> bool:
            """Send mouse event via the direct QMP socket."""
            if not _self._qmp or not _self._qmp.connected:
                return False
            events = [
                {"type": "abs", "data": {"axis": "x", "value": x}},
                {"type": "abs", "data": {"axis": "y", "value": y}},
            ]
            if down is not None and button >= 0:
                btn_map = {0: "left", 1: "left", 2: "right", 4: "middle"}
                events.append({
                    "type": "btn",
                    "data": {"down": down, "button": btn_map.get(button, "left")},
                })
            resp = await _self._qmp._ctrl.send_command({
                "execute": "input-send-event",
                "arguments": {"events": events},
            })
            return resp is not None and "return" in resp

        async def input_key(request: web.Request) -> web.Response:
            """Send keyboard input. Body: {"keycode": 30, "down": true}"""
            body = await request.json()
            keycode = body.get("keycode", 0)
            down = body.get("down", True)
            # evdev input-linux (kernel-level, zero overhead)
            if _self._evdev_input_fd >= 0:
                _evdev_write_key(_self._evdev_input_fd, keycode, down)
                return web.json_response({"ok": True})
            # QMP input-send-event (fallback — 1-2ms latency)
            ok = await _qmp_input_key(_self._name, keycode, down)
            if ok:
                return web.json_response({"ok": True})
            return web.json_response({"error": "no input method available"}, status=503)

        async def input_mouse(request: web.Request) -> web.Response:
            """Send mouse input. Body: {"x": 500, "y": 300, "button": 0, "action": "click"}"""
            body = await request.json()
            x = body.get("x", 0)
            y = body.get("y", 0)
            action = body.get("action", "move")
            button = body.get("button", 0)
            # QMP input-send-event (direct socket)
            if action == "move":
                await _qmp_input_mouse(_self._name, x, y)
            elif action == "press":
                await _qmp_input_mouse(_self._name, x, y, button, down=True)
            elif action == "release":
                await _qmp_input_mouse(_self._name, x, y, button, down=False)
            elif action == "click":
                await _qmp_input_mouse(_self._name, x, y, button, down=True)
                await asyncio.sleep(0.05)
                await _qmp_input_mouse(_self._name, x, y, button, down=False)
            return web.json_response({"ok": True})

        async def input_type(request: web.Request) -> web.Response:
            """Type text. Body: {"text": "hello"}"""
            has_input = ((_self._display and _self._display.connected)
                        or (_self._use_evdev and _self._evdev_kbd))
            if not has_input:
                return web.json_response({"error": "no input method"}, status=503)
            body = await request.json()
            text = body.get("text", "")
            import time as _time
            # Map characters to evdev keycodes
            CHAR_TO_EVDEV = {
                **{c: (30 + i, False) for i, c in enumerate('asdfghjkl')},
                **{c: (16 + i, False) for i, c in enumerate('qwertyuiop')},
                **{c: (44 + i, False) for i, c in enumerate('zxcvbnm')},
                **{str(i): (2 + i if i > 0 else 11, False) for i in range(10)},
                ' ': (57, False), '.': (52, False), '-': (12, False), '=': (13, False),
                ',': (51, False), '/': (53, False), ';': (39, False), "'": (40, False),
                '\\': (43, False), '[': (26, False), ']': (27, False), '`': (41, False),
                ':': (39, True), '_': (12, True), '+': (13, True), '"': (40, True),
                '<': (51, True), '>': (52, True), '?': (53, True),
                '\n': (28, False), '\t': (15, False),
            }
            use_dbus = _self._display and _self._display.connected
            for ch in text:
                lc = ch.lower()
                shift = ch.isupper() or ch in CHAR_TO_EVDEV and CHAR_TO_EVDEV.get(ch, (0, False))[1]
                keycode, need_shift = CHAR_TO_EVDEV.get(lc, CHAR_TO_EVDEV.get(ch, (0, False)))
                if keycode:
                    if use_dbus:
                        if shift or need_shift:
                            _self._display.key_press(42)
                        _self._display.key_tap(keycode)
                        if shift or need_shift:
                            _self._display.key_release(42)
                    elif _self._use_evdev and _self._evdev_kbd:
                        if shift or need_shift:
                            _self._evdev_kbd.key_event(42, True)
                        _self._evdev_kbd.key_event(keycode, True)
                        _time.sleep(0.01)
                        _self._evdev_kbd.key_event(keycode, False)
                        if shift or need_shift:
                            _self._evdev_kbd.key_event(42, False)
                    _time.sleep(0.02)
            return web.json_response({"ok": True})

        async def input_ws(request: web.Request) -> web.WebSocketResponse:
            """WebSocket for real-time input from dashboard."""
            ws = web.WebSocketResponse()
            await ws.prepare(request)
            import json as _json
            async for msg in ws:
                if msg.type != web.WSMsgType.TEXT:
                    continue
                data = _json.loads(msg.data)
                if not _self._display or not _self._display.connected:
                    continue
                t = data.get("type", "")
                if t == "key":
                    kc = data.get("keycode", 0)
                    if data.get("down", True):
                        _self._display.key_press(kc)
                    else:
                        _self._display.key_release(kc)
                elif t == "pointer":
                    _self._display.mouse_move(data.get("x", 0), data.get("y", 0))
                    btn = data.get("buttons", -1)
                    if btn == 1:
                        _self._display.mouse_press(0)
                    elif btn == 0 and data.get("was_pressed"):
                        _self._display.mouse_release(0)
                elif t == "click":
                    _self._display.mouse_click(data.get("x", 0), data.get("y", 0), data.get("button", 0))
            return ws

        app.router.add_get("/display/snapshot", display_snapshot)
        app.router.add_get("/display/mjpeg", display_mjpeg)
        app.router.add_get("/display/info", display_info)
        app.router.add_post("/input/key", input_key)
        app.router.add_post("/input/mouse", input_mouse)
        app.router.add_post("/input/type", input_type)
        app.router.add_get("/input/ws", input_ws)

        async def display_ws(request: web.Request) -> web.WebSocketResponse:
            """WebSocket JPEG stream — real-time framebuffer, zero buffering."""
            ws = web.WebSocketResponse()
            await ws.prepare(request)
            dc = _self._dbus_client
            target_fps = 30
            interval = 1.0 / target_fps
            last_frame = None
            try:
                while not ws.closed:
                    if dc and dc.connected and dc.latest_frame:
                        frame = dc.latest_frame
                        if frame is not last_frame:
                            await ws.send_bytes(frame)
                            last_frame = frame
                    await asyncio.sleep(interval)
            except (asyncio.CancelledError, ConnectionResetError):
                pass
            return ws

        app.router.add_get("/display/ws", display_ws)

        # WebRTC sessions
        _gst_stream = None
        _webrtc_pcs: set = set()

        async def webrtc_offer(request: web.Request) -> web.Response:
            """WebRTC signaling: H.264 via aiortc."""
            nonlocal _gst_stream

            body = await request.json()

            if not _self._dbus_client or not _self._dbus_client.connected:
                return web.json_response({"error": "no display"}, status=503)

            import aiortc.codecs.h264 as _h264mod
            _h264mod.DEFAULT_BITRATE = 4_000_000  # 4Mbps default
            _h264mod.MAX_BITRATE = 50_000_000     # 50Mbps ceiling for 4K60
            from aiortc import RTCPeerConnection, RTCSessionDescription
            from webrtc_stream import FramebufferVideoTrack
            from webrtc_audio import PulseAudioTrack

            offer = RTCSessionDescription(sdp=body["sdp"], type=body["type"])
            pc = RTCPeerConnection()
            _webrtc_pcs.add(pc)

            @pc.on("connectionstatechange")
            async def on_state():
                if pc.connectionState in ("failed", "closed"):
                    _webrtc_pcs.discard(pc)
                    await pc.close()

            # Video track from D-Bus framebuffer
            track = FramebufferVideoTrack(_self._dbus_client, fps=30)
            sender = pc.addTrack(track)
            # Store sender for bitrate control
            _self._webrtc_sender = sender

            # Audio track from PulseAudio monitor (VM audio output)
            audio_sink = _self._audio_sink or f"ozma-{_self._name}"
            # Find the monitor source for the sink the VM plays to
            try:
                import subprocess as _sp
                result = _sp.run(["pactl", "list", "short", "sink-inputs"],
                                capture_output=True, text=True, timeout=3)
                # Default: capture from the default sink monitor
                monitor = "default.monitor"
                # If we can find the VM's specific sink, use its monitor
                for line in result.stdout.splitlines():
                    # sink-input lines don't easily map to sink names
                    pass
                audio_track = PulseAudioTrack(sink_monitor=monitor)
                pc.addTrack(audio_track)
            except Exception as e:
                log.debug("Audio track unavailable: %s", e)

            await pc.setRemoteDescription(offer)
            answer = await pc.createAnswer()
            await pc.setLocalDescription(answer)

            answer_sdp = pc.localDescription.sdp

            if answer_sdp:
                return web.json_response({"sdp": answer_sdp, "type": "answer"})
            return web.json_response({"error": "WebRTC negotiation failed"}, status=500)

        async def webrtc_bitrate(request: web.Request) -> web.Response:
            """Adjust WebRTC video bitrate. Body: {"bitrate": 4000000}"""
            body = await request.json()
            bitrate = body.get("bitrate", 4000000)
            bitrate = max(500_000, min(bitrate, 50_000_000))  # up to 50Mbps for 4K60
            # Find the H.264 encoder on the active sender
            sender = getattr(_self, '_webrtc_sender', None)
            if sender and hasattr(sender, '_encoder') and sender._encoder:
                sender._encoder.target_bitrate = bitrate
                return web.json_response({"ok": True, "bitrate": bitrate})
            # Try via codec directly
            for pc in _webrtc_pcs:
                for s in pc.getSenders():
                    if s.track and s.track.kind == "video" and hasattr(s, '_encoder'):
                        s._encoder.target_bitrate = bitrate
                        return web.json_response({"ok": True, "bitrate": bitrate})
            return web.json_response({"ok": False, "error": "no active encoder"})

        app.router.add_post("/webrtc/bitrate", webrtc_bitrate)
        app.router.add_post("/webrtc/offer", webrtc_offer)

        # Dynamic codec switching
        async def get_codec(request: web.Request) -> web.Response:
            return web.json_response({
                "current_encoder": self._current_encoder,
                "override": self._codec_override,
                "available": self._available_encoders,
            })

        async def set_codec(request: web.Request) -> web.Response:
            try:
                body = await request.json()
            except Exception:
                body = {}
            self._codec_override = body if body else None
            # Kill current ffmpeg; capture loop will restart with new config
            if self._ffmpeg_hls_proc:
                try:
                    self._ffmpeg_hls_proc.kill()
                except ProcessLookupError:
                    pass
            log.info("Codec override set: %s (ffmpeg restart requested)", body)
            return web.json_response({"ok": True, "override": self._codec_override})

        app.router.add_get("/codec", get_codec)
        app.router.add_post("/codec", set_codec)

        app.router.add_get("/health", health)
        app.router.add_get("/metrics", metrics)
        app.router.add_get("/api/v1/connection", connection_state)
        app.router.add_get("/power/state", power_state)
        app.router.add_post("/power/{action}", power_action)

        # Serve HLS stream (from capture device or virtual capture)
        stream_dir = Path(f"/tmp/ozma-stream-{self._name}")
        stream_dir.mkdir(parents=True, exist_ok=True)
        app.router.add_static("/stream/", stream_dir, show_index=False)

        runner = web.AppRunner(app)
        await runner.setup()

        # Pick a port: use configured port, or auto-assign based on UDP port
        port = self._api_port or (self._port + 50)  # e.g. 7332 → 7382
        site = web.TCPSite(runner, "0.0.0.0", port)
        await site.start()
        self._api_port = port
        log.info("HTTP API on port %d (power control)", port)
        return runner

    # --- mDNS announcement ---

    async def _announce(self) -> None:
        local_ip = self._resolve_local_ip()
        service_type = "_ozma._udp.local."
        service_name = f"{self._name}.{service_type}"

        audio_props: dict[str, str] = {}
        if self._audio_sink:
            audio_props = {
                "audio_type": "pipewire",
                "audio_sink": self._audio_sink,
            }

        self._info = ServiceInfo(
            service_type,
            service_name,
            addresses=[socket.inet_aton(local_ip)],
            port=self._port,
            properties={
                "proto": str(PROTO_VERSION),
                "role": "compute",
                "hw": "soft",
                "fw": "0.1.0",
                "cap": f"{'evdev' if self._use_evdev else 'qmp'},power",
                **({"api_port": str(self._api_port)} if self._api_port else {}),
                **({"vnc_host": self._vnc_host} if self._vnc_host else {}),
                **({"vnc_port": str(self._vnc_port)} if self._vnc_port else {}),
                **({"capture_device": self._virtual_capture.device_path}
                   if self._virtual_capture and self._virtual_capture.device_path else {}),
                **({"vm_ip": self._vm_guest_ip} if self._vm_guest_ip else {}),
                **audio_props,
            },
        )
        # On hosts with many interfaces (Docker, Podman, libvirt bridges),
        # zeroconf iterates all of them, taking minutes. Bind to the specific
        # IP we resolved to avoid this.
        from zeroconf import IPVersion
        self._azc = AsyncZeroconf(interfaces=["127.0.0.1"], ip_version=IPVersion.V4Only)
        await self._azc.async_register_service(self._info)
        log.info(
            "mDNS announced: %s @ %s:%d  (node_id: %s)",
            self._name, local_ip, self._port, service_name,
        )
        print(f"[soft-node:{self._name}] Listening on UDP {local_ip}:{self._port}")
        print(f"[soft-node:{self._name}] node_id = {service_name}")
        print(f"[soft-node:{self._name}] Activate with:")
        print(f"  curl -X POST http://localhost:7380/api/v1/scenarios/<id>/bind \\")
        print(f"       -H 'Content-Type: application/json' \\")
        print(f"       -d '{{\"node_id\": \"{service_name}\"}}'")
        print()

    async def _unannounce(self) -> None:
        if hasattr(self, "_azc") and hasattr(self, "_info"):
            await self._azc.async_unregister_service(self._info)
            await self._azc.async_close()

    async def _direct_register(self) -> None:
        """Register directly with the controller via HTTP.

        On busy hosts with many network interfaces, mDNS may resolve
        with stale/incomplete TXT records. Direct registration ensures
        all fields (especially capture_device) arrive at the controller.
        """
        import json
        import urllib.request

        service_type = "_ozma._udp.local."
        node_id = f"{self._name}.{service_type}"
        url = "http://localhost:7380/api/v1/nodes/register"

        local_ip = self._resolve_local_ip()
        body = {
            "id": node_id,
            "host": local_ip,
            "port": self._port,
            "proto": str(PROTO_VERSION),
            "role": "compute",
            "hw": "soft",
            "fw": "0.1.0",
            "cap": "qmp,power",
            "vnc_host": self._vnc_host or "",
            "vnc_port": str(self._vnc_port) if self._vnc_port else "",
            "api_port": str(self._api_port) if self._api_port else "",
            "audio_type": "pipewire" if self._audio_sink else "",
            "audio_sink": self._audio_sink or "",
            "capture_device": (self._capture_device
                               or (self._virtual_capture.device_path
                                   if self._virtual_capture and self._virtual_capture.device_path
                                   else "")),
            **({"vm_guest_ip": self._vm_guest_ip} if self._vm_guest_ip else {}),
        }
        # Multi-display outputs
        if self._displays:
            body["display_outputs"] = json.dumps([
                {"index": d.console_index, "source_type": "dbus",
                 "capture_source_id": f"{self._name}-display-{d.console_index}",
                 "width": d.width, "height": d.height}
                for d in self._displays
            ])

        await asyncio.sleep(3)  # give the controller time to start
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
                    log.info("Direct registration with controller succeeded")
                    # Keep re-registering to stay alive (container can't mDNS)
                    asyncio.create_task(
                        self._re_register_loop(body, url),
                        name=f"re-register-{self._name}",
                    )
                    return
            except Exception:
                await asyncio.sleep(2)

        log.debug("Direct registration failed after 10 attempts (controller may not be up)")

    async def _re_register_loop(self, body: dict, url: str) -> None:
        """Re-register every 30s to keep alive in containerised controllers."""
        import json
        import urllib.request
        while True:
            await asyncio.sleep(30)
            try:
                data = json.dumps(body).encode()
                req = urllib.request.Request(
                    url, data=data,
                    headers={"Content-Type": "application/json"},
                    method="POST",
                )
                loop = asyncio.get_running_loop()
                await loop.run_in_executor(
                    None, lambda: urllib.request.urlopen(req, timeout=3).read()
                )
            except Exception:
                pass

    # --- Encoder selection ---

    async def _build_encoder_args(
        self, w: int, h: int, fps: int
    ) -> tuple[list[str], list[str] | None]:
        """
        Return (encoder_args, vf_extra) for the current codec override or best auto-detected encoder.

        Probe priority: NVENC → VAAPI → QSV → V4L2M2M → libx264.
        Results are cached; override via POST /codec.
        """
        override = self._codec_override or {}
        requested = override.get("codec", "")         # e.g. "h264_nvenc", "h264_vaapi", "h265"
        bitrate = override.get("bitrate", "3M")
        quality = override.get("quality", -1)
        latency = override.get("latency_mode", "realtime")

        # If explicit encoder name given (not family), use it directly
        if requested and "_" in requested:
            self._current_encoder = requested
            return _encoder_args_for(requested, bitrate, quality, latency), None

        # Map family → preferred encoder
        family = requested or "h264"
        candidates: list[tuple[str, str | None]] = [
            # (ffmpeg_encoder, vaapi_device_or_None)
            ("h264_nvenc",   None),
            ("h264_vaapi",   "/dev/dri/renderD128"),
            ("h264_qsv",     None),
            ("h264_v4l2m2m", None),
            ("libx264",      None),
        ]
        if family == "h265":
            candidates = [
                ("hevc_nvenc", None), ("hevc_vaapi", "/dev/dri/renderD128"),
                ("hevc_qsv", None), ("hevc_v4l2m2m", None), ("libx265", None),
            ]

        # Use cached results if available
        if not self._available_encoders or override:
            self._available_encoders = []
            for enc_name, vaapi_dev in candidates:
                ok = await _test_encoder_quick(enc_name, vaapi_dev)
                if ok:
                    self._available_encoders.append(enc_name)

        chosen = self._available_encoders[0] if self._available_encoders else "libx264"
        self._current_encoder = chosen
        vaapi_dev = dict(candidates).get(chosen)
        vf_extra: list[str] | None = None
        if vaapi_dev and "vaapi" in chosen:
            vf_extra = ["-vf", f"scale={w}:{h},format=nv12,hwupload", "-pix_fmt", "nv12"]
        return _encoder_args_for(chosen, bitrate, quality, latency), vf_extra

    # --- UDP server ---

    async def _capture_hls(self, device_or_socket: str) -> None:
        """
        Capture display → HLS.

        If device_or_socket is a unix socket path, use socat to bridge
        it to TCP so ffmpeg can read VNC from it. If it's a /dev/video*,
        use v4l2 input.
        """
        hls_dir = self._hls_dir

        if device_or_socket.startswith("/dev/"):
            # V4L2 capture device
            cmd = [
                "ffmpeg", "-hide_banner", "-loglevel", "warning", "-y",
                "-f", "v4l2", "-i", device_or_socket,
            ]
        elif device_or_socket.endswith(".sock") or os.path.exists(device_or_socket):
            # VNC unix socket — bridge to TCP with socat, then ffmpeg reads VNC
            socat_port = 15931
            socat_proc = await asyncio.create_subprocess_exec(
                "socat", f"TCP-LISTEN:{socat_port},reuseaddr,fork",
                f"UNIX-CONNECT:{device_or_socket}",
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.DEVNULL,
            )
            await asyncio.sleep(0.5)
            cmd = [
                "ffmpeg", "-hide_banner", "-loglevel", "warning", "-y",
                "-i", f"vnc://127.0.0.1:{socat_port}",
            ]
            log.info("VNC socket bridged to TCP :%d via socat", socat_port)
        else:
            log.warning("Unknown capture source: %s", device_or_socket)
            return

        cmd += [
            "-c:v", "libx264", "-preset", "ultrafast", "-tune", "zerolatency",
            "-crf", "28", "-r", "15",
            "-f", "hls",
            "-hls_time", "1",
            "-hls_list_size", "4",
            "-hls_flags", "delete_segments+independent_segments",
            "-hls_segment_filename", str(hls_dir / "seg_%05d.ts"),
            str(hls_dir / "stream.m3u8"),
        ]
        try:
            proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.PIPE,
            )
            log.info("HLS capture → %s", hls_dir / "stream.m3u8")
            await proc.wait()
        except Exception as e:
            log.warning("HLS capture failed: %s", e)

    async def _capture_dbus_hls(self) -> None:
        """
        Capture display via D-Bus RegisterListener → ffmpeg → H.264 HLS.

        Reads raw BGRA pixels from the D-Bus framebuffer (pushed by QEMU
        at display refresh rate) and pipes them to ffmpeg for H.264 encoding.
        """
        dc = self._dbus_client
        hls_dir = self._hls_dir
        target_fps = 30

        # Wait for first frame to know dimensions
        for _ in range(30):
            if dc.width and dc.height and dc.latest_frame:
                break
            await asyncio.sleep(0.5)
        if not dc.width or not dc.height:
            log.warning("D-Bus display: no frames received, falling back to QMP")
            await self._capture_qmp_hls()
            return

        w, h = dc.width, dc.height
        log.info("D-Bus capture: %dx%d @ %dfps target", w, h, target_fps)

        # Detect available encoders (probe once per session; re-probe if override changes)
        enc, vf_extra = await self._build_encoder_args(w, h, target_fps)

        cmd = [
            "ffmpeg", "-hide_banner", "-loglevel", "warning", "-y",
            "-f", "rawvideo", "-pix_fmt", "bgra", "-s", f"{w}x{h}",
            "-r", str(target_fps), "-i", "-",
            *enc,
            *(vf_extra or ["-pix_fmt", "yuv420p"]),
            "-g", str(target_fps),
            "-f", "hls",
            "-hls_time", "0.5",
            "-hls_list_size", "3",
            "-hls_flags", "delete_segments+independent_segments+split_by_time",
            "-hls_segment_type", "mpegts",
            "-hls_segment_filename", str(hls_dir / "seg_%05d.ts"),
            str(hls_dir / "stream.m3u8"),
        ]

        proc = await asyncio.create_subprocess_exec(
            *cmd,
            stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.DEVNULL,
            stderr=asyncio.subprocess.PIPE,
        )
        self._ffmpeg_hls_proc = proc
        log.info("D-Bus → HLS started (pid=%d, %dx%d, enc=%s, %dfps)",
                 proc.pid, w, h, self._current_encoder, target_fps)

        # Register with go2rtc
        try:
            import urllib.request
            hls_url = f"http://127.0.0.1:{self._api_port}/stream/stream.m3u8"
            go2rtc_url = f"http://localhost:1984/api/streams?dst={self._name}&src=ffmpeg:{hls_url}"
            urllib.request.urlopen(urllib.request.Request(go2rtc_url, method="PUT"), timeout=3)
            log.info("Registered with go2rtc: %s", self._name)
        except Exception:
            pass

        frame_interval = 1.0 / target_fps
        frames = 0
        last_fb = None
        loop = asyncio.get_event_loop()

        try:
            while not self._stop_event.is_set() and proc.returncode is None:
                t0 = loop.time()

                # Get raw framebuffer from D-Bus client
                fb = dc._framebuffer
                if fb and fb is not last_fb and len(fb) >= w * h * 4:
                    try:
                        proc.stdin.write(bytes(fb[:w * h * 4]))
                        await proc.stdin.drain()
                        frames += 1
                        last_fb = fb
                    except (BrokenPipeError, ConnectionResetError):
                        break

                elapsed = loop.time() - t0
                if elapsed < frame_interval:
                    await asyncio.sleep(frame_interval - elapsed)

        except asyncio.CancelledError:
            pass
        except Exception as e:
            log.warning("D-Bus HLS capture error: %s", e)
        finally:
            if proc.stdin and not proc.stdin.is_closing():
                proc.stdin.close()
            try:
                await asyncio.wait_for(proc.wait(), timeout=5)
            except asyncio.TimeoutError:
                proc.kill()
            log.info("D-Bus HLS capture stopped (%d frames)", frames)

    async def _capture_qmp_hls(self) -> None:
        """
        Capture display via QMP screendump → ffmpeg → H.264 HLS.

        Grabs PPM frames from the QEMU display via the direct QMP socket,
        pipes them to ffmpeg which encodes H.264 and outputs HLS segments.
        """
        ctrl = self._qmp._ctrl if self._qmp else None
        if not ctrl:
            return

        # Wait for QMP to connect
        for _ in range(60):
            if ctrl.connected:
                break
            await asyncio.sleep(1)
        if not ctrl.connected:
            log.warning("QMP not connected — cannot start HLS capture")
            return

        hls_dir = self._hls_dir
        frame_dir = Path("/run/ozma/frames")
        frame_dir.mkdir(parents=True, exist_ok=True)
        tmp_path = str(frame_dir / f"{self._name}.ppm")
        target_fps = 15

        # Detect / use encoder (shared with D-Bus path)
        encoder_args, _vf = await self._build_encoder_args(1920, 1080, target_fps)

        # Check for PipeWire audio sink for this VM
        pw_sink = f"ozma-{self._name}"
        has_audio = self._audio_sink or True  # Try PipeWire capture by default

        # ffmpeg: PPM stdin (video) + PipeWire (audio) → H.264+AAC → HLS
        # go2rtc reads the HLS for WebRTC output
        cmd = [
            "ffmpeg", "-hide_banner", "-loglevel", "warning", "-y",
            "-f", "image2pipe", "-framerate", str(target_fps),
            "-i", "-",
        ]

        # Add PipeWire audio capture if available
        if has_audio:
            cmd += [
                "-f", "pulse",
                "-i", f"{pw_sink}.monitor",
            ]

        cmd += [
            # Video encoding
            *encoder_args,
            "-r", str(target_fps),
            "-pix_fmt", "yuv420p",
            "-g", str(target_fps),
        ]

        if has_audio:
            cmd += [
                # Audio encoding
                "-c:a", "aac", "-b:a", "128k", "-ac", "2",
            ]

        cmd += [
            "-f", "hls",
            "-hls_time", "0.5",
            "-hls_list_size", "3",
            "-hls_flags", "delete_segments+independent_segments+split_by_time",
            "-hls_segment_type", "mpegts",
            "-hls_segment_filename", str(hls_dir / "seg_%05d.ts"),
            str(hls_dir / "stream.m3u8"),
        ]

        proc = await asyncio.create_subprocess_exec(
            *cmd,
            stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.DEVNULL,
            stderr=asyncio.subprocess.PIPE,
        )
        self._ffmpeg_hls_proc = proc
        log.info("QMP → HLS started for %s (pid=%d, enc=%s, %dfps)",
                 self._name, proc.pid, self._current_encoder, target_fps)

        # Register with go2rtc for WebRTC output (if go2rtc is available)
        try:
            import urllib.request
            hls_url = f"http://127.0.0.1:{self._api_port}/stream/stream.m3u8"
            go2rtc_url = f"http://localhost:1984/api/streams?dst={self._name}&src=ffmpeg:{hls_url}"
            urllib.request.urlopen(urllib.request.Request(go2rtc_url, method="PUT"), timeout=3)
            log.info("Registered with go2rtc for WebRTC: %s", self._name)
        except Exception as e:
            log.debug("go2rtc registration failed (WebRTC unavailable): %s", e)

        frame_interval = 1.0 / target_fps
        frames = 0
        loop = asyncio.get_event_loop()
        try:
            while not self._stop_event.is_set() and proc.returncode is None:
                t0 = loop.time()

                # QMP screendump writes PPM to /run/ozma/frames/
                resp = await ctrl.send_command({
                    "execute": "screendump",
                    "arguments": {"filename": tmp_path, "format": "ppm"},
                })
                if resp and "return" in resp:
                    try:
                        data = await loop.run_in_executor(None, lambda: Path(tmp_path).read_bytes())
                        proc.stdin.write(data)
                        await proc.stdin.drain()
                        frames += 1
                    except (BrokenPipeError, ConnectionResetError):
                        break
                    except FileNotFoundError:
                        pass

                elapsed = loop.time() - t0
                if elapsed < frame_interval:
                    await asyncio.sleep(frame_interval - elapsed)

        except asyncio.CancelledError:
            pass
        except Exception as e:
            log.warning("QMP HLS capture error: %s", e)
        finally:
            if proc.stdin and not proc.stdin.is_closing():
                proc.stdin.close()
            try:
                await asyncio.wait_for(proc.wait(), timeout=5)
            except asyncio.TimeoutError:
                proc.kill()
            try:
                os.unlink(tmp_path)
            except OSError:
                pass
            log.info("QMP HLS capture stopped for %s (%d frames)", self._name, frames)

    async def _serve(self) -> None:
        loop = asyncio.get_running_loop()

        # asyncio UDP via create_datagram_endpoint
        transport, protocol = await loop.create_datagram_endpoint(
            lambda: _UDPProtocol(self._on_packet),
            local_addr=(self._host, self._port),
        )
        log.info("UDP server running on %s:%d", self._host, self._port)

        try:
            await self._stop_event.wait()
        finally:
            transport.close()
            await self._unannounce()
            if self._qmp:
                await self._qmp.stop()
            if self._runner:
                await self._runner.cleanup()
            log.info("Soft node '%s' stopped", self._name)

    def _on_packet(self, data: bytes, addr: tuple) -> None:
        if not data:
            return
        self._connect.record_hid_packet()
        ptype = data[0]
        payload = data[1:]

        if self._use_evdev and self._evdev_translator:
            # Primary path: evdev → QEMU input-linux (zero latency, no QMP)
            if ptype == 0x01:  # keyboard
                self._evdev_translator.handle_keyboard(payload)
            elif ptype == 0x02:  # mouse (absolute)
                self._evdev_translator.handle_mouse(payload)
            elif ptype == 0x05:  # mouse (relative — gaming)
                self._evdev_translator.handle_mouse_relative(payload)
            else:
                log.debug("Unknown packet type 0x%02X from %s", ptype, addr)
        elif self._qmp:
            # Fallback: QMP input events
            if ptype == 0x01:
                events = self._kbd.diff(payload)
                if events:
                    asyncio.create_task(
                        self._qmp.send_input_events(events),
                        name="qmp-kbd",
                    )
            elif ptype == 0x02:
                events = self._mouse.decode(payload)
                if events:
                    asyncio.create_task(
                        self._qmp.send_input_events(events),
                        name="qmp-mouse",
                    )
            else:
                log.debug("Unknown packet type 0x%02X from %s", ptype, addr)

    # --- Helpers ---

    @staticmethod
    def _resolve_local_ip() -> str:
        try:
            s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            s.connect(("8.8.8.8", 80))
            ip = s.getsockname()[0]
            s.close()
            return ip
        except OSError:
            return "127.0.0.1"


class _UDPProtocol(asyncio.DatagramProtocol):
    def __init__(self, callback) -> None:
        self._callback = callback

    def datagram_received(self, data: bytes, addr: tuple) -> None:
        self._callback(data, addr)

    def error_received(self, exc: Exception) -> None:
        log.warning("UDP error: %s", exc)


# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------

def main() -> None:
    p = argparse.ArgumentParser(description="Ozma Soft Node (evdev + libvirt)")
    p.add_argument("--name", required=True, help="Node name, e.g. 'vm1'")
    p.add_argument("--qmp", default="", metavar="SOCKET",
                   help="QMP control socket (optional — evdev is used for HID, libvirt for power)")
    p.add_argument("--qmp-input", default="", metavar="SOCKET",
                   help="Dedicated QMP input socket (legacy). If omitted, shares --qmp.")
    p.add_argument("--port", type=int, default=7332,
                   help="UDP port to listen on (default 7332; use distinct ports per instance)")
    p.add_argument("--host", default="0.0.0.0", help="UDP bind address")
    p.add_argument("--vnc-host", default=None,
                   help="VNC host for video streaming (e.g. 127.0.0.1)")
    p.add_argument("--vnc-port", type=int, default=None,
                   help="VNC port for video streaming (e.g. 5901)")
    p.add_argument("--vnc-socket", default=None,
                   help="VNC unix socket path (overrides --vnc-host/port)")
    p.add_argument("--capture-device", default=None,
                   help="V4L2 capture device path (e.g. /dev/video10)")
    p.add_argument("--audio-sink", default=None,
                   help="PipeWire null sink name for this node's audio (e.g. ozma-vm1)")
    p.add_argument("--api-port", type=int, default=0,
                   help="HTTP API port for power control (default: udp-port + 50)")
    p.add_argument("--vm-guest-ip", default=None,
                   help="VM's guest network IP (advertised via mDNS vm_ip TXT record)")
    p.add_argument("--debug", action="store_true")
    args = p.parse_args()

    logging.basicConfig(
        level=logging.DEBUG if args.debug else logging.INFO,
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
        datefmt="%H:%M:%S",
    )

    node = SoftNode(args.name, args.host, args.port,
                    qmp_path=args.qmp,
                    vnc_host=args.vnc_host, vnc_port=args.vnc_port,
                    vnc_socket=args.vnc_socket,
                    capture_device=args.capture_device,
                    audio_sink=args.audio_sink, api_port=args.api_port,
                    qmp_input_path=args.qmp_input,
                    vm_guest_ip=args.vm_guest_ip)

    async def run() -> None:
        loop = asyncio.get_running_loop()
        stop = asyncio.Event()

        def _sig(_):
            stop.set()

        loop.add_signal_handler(signal.SIGINT, _sig, None)
        loop.add_signal_handler(signal.SIGTERM, _sig, None)

        task = asyncio.create_task(node.run())
        await stop.wait()
        await node.stop()
        await task

    asyncio.run(run())


if __name__ == "__main__":
    main()
