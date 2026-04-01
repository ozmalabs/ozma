# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
#!/usr/bin/env python3
"""
Ozma Soft Node — QMP-backed virtual compute node.

Announces itself via mDNS (_ozma._udp.local.), listens for tinynode HID
packets from the Controller, and forwards them to a QEMU VM via QMP.

Usage:
  python softnode/soft_node.py --name vm1 --port 7332 --qmp /tmp/ozma-vm1.qmp
  python softnode/soft_node.py --name vm2 --port 7333 --qmp /tmp/ozma-vm2.qmp

Each instance needs a distinct --port since both run on the same host.
The Controller discovers them via mDNS and routes HID to whichever port is
in the active scenario's NodeInfo.

The mDNS instance name becomes the node_id in the Controller:
  "vm1._ozma._udp.local."
"""

import argparse
import asyncio
import logging
import signal
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
from looking_glass import LookingGlassCapture
from virtual_capture import VirtualCapture
from connect_client import NodeConnectClient
from prometheus_metrics import collect_soft

log = logging.getLogger("ozma.softnode")

PROTO_VERSION = 1
MAX_PACKET = 64


class SoftNode:
    def __init__(
        self,
        name: str,
        host: str,
        port: int,
        qmp_path: str,
        vnc_host: str | None = None,
        vnc_port: int | None = None,
        vnc_socket: str | None = None,    # VNC unix socket path (overrides host/port)
        capture_device: str | None = None,  # V4L2 device path (skip VNC capture)
        audio_sink: str | None = None,    # PipeWire null sink name for this node
        api_port: int = 0,                # HTTP port for power/status API (0 = auto)
        qmp_input_path: str = "",         # Dedicated input QMP socket (recommended)
    ) -> None:
        self._name = name
        self._host = host
        self._port = port
        self._qmp = QMPClient(qmp_path, input_socket_path=qmp_input_path)
        self._vnc_host = vnc_host
        self._vnc_port = vnc_port
        self._vnc_socket = vnc_socket
        self._capture_device = capture_device
        self._audio_sink = audio_sink
        self._api_port = api_port
        self._kbd = KeyboardReportState()
        self._mouse = MouseReportState()
        self._stop_event = asyncio.Event()
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
        await self._qmp.start()

        # Connect to QEMU D-Bus display (keyboard + mouse + framebuffer)
        console_indices = await QEMUDBusConsole.enumerate_consoles()
        if not console_indices:
            console_indices = [0]  # try Console_0 even if enumeration fails
        for idx in console_indices:
            console = QEMUDBusConsole(idx)
            if await console.connect():
                self._displays.append(console)
                log.info("D-Bus console %d: %dx%d (%s)",
                         idx, console.width, console.height, console.label)
        if self._displays:
            self._display = self._displays[0]  # primary for backward compat
            log.info("QEMU D-Bus display: %d console(s) ready", len(self._displays))
        else:
            log.warning("QEMU D-Bus display not available — falling back to QMP/VNC")

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
            capabilities="qmp,power",
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

    # --- HTTP API for power control ---

    async def _start_api(self) -> web.AppRunner | None:
        """Start a lightweight HTTP server for power/status endpoints."""
        app = web.Application()

        async def health(_: web.Request) -> web.Response:
            return web.json_response({"ok": True})

        async def connection_state(_: web.Request) -> web.Response:
            return web.json_response(self._connect.state.to_dict())

        async def metrics(_: web.Request) -> web.Response:
            status = await self._qmp.query_status()
            text = collect_soft(
                node_name=self._name,
                connect_client=self._connect,
                qmp_connected=self._qmp.connected,
                vm_status=status.get("status", "unknown") if status else "unknown",
            )
            return web.Response(text=text, content_type="text/plain; version=0.0.4")

        async def power_state(_: web.Request) -> web.Response:
            status = await self._qmp.query_status()
            running = status.get("status") == "running" if status else None
            return web.json_response({
                "available": self._qmp.connected,
                "powered": running,
                "vm_status": status.get("status") if status else "unknown",
            })

        async def power_action(request: web.Request) -> web.Response:
            action = request.match_info["action"]
            actions = {
                "on": self._qmp.cont,             # resume paused VM
                "off": self._qmp.system_powerdown, # ACPI power button
                "reset": self._qmp.system_reset,
                "force-off": self._qmp.stop,       # pause VM (closest to force-off)
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
            if _self._display and _self._display.connected:
                frame = await _self._display.get_frame()
                if frame:
                    return web.Response(body=frame, content_type="image/jpeg")
            # QMP screendump via the control client
            ctrl = _self._qmp._ctrl if hasattr(_self._qmp, '_ctrl') else _self._qmp
            if hasattr(ctrl, 'screendump') and ctrl.connected:
                import io as _io
                tmp = f"/dev/shm/ozma-snap-{_os.getpid()}.png"
                ok = await ctrl.screendump(tmp)
                if ok and _os.path.exists(tmp):
                    try:
                        from PIL import Image
                        img = Image.open(tmp)
                        buf = _io.BytesIO()
                        img.convert("RGB").save(buf, format="JPEG", quality=75)
                        return web.Response(body=buf.getvalue(), content_type="image/jpeg")
                    finally:
                        try:
                            _os.unlink(tmp)
                        except OSError:
                            pass
            return web.json_response({"error": "no display"}, status=503)

        async def display_mjpeg(_: web.Request) -> web.StreamResponse:
            """MJPEG stream of the VM display."""
            response = web.StreamResponse(
                status=200,
                headers={"Content-Type": "multipart/x-mixed-replace; boundary=frame"},
            )
            await response.prepare(_)
            ctrl = _self._qmp._ctrl if hasattr(_self._qmp, '_ctrl') else _self._qmp
            while True:
                if hasattr(ctrl, 'screendump') and ctrl.connected:
                    import io as _io
                    tmp = f"/dev/shm/ozma-mjpeg-{_os.getpid()}.png"
                    ok = await ctrl.screendump(tmp)
                    if ok and _os.path.exists(tmp):
                        try:
                            from PIL import Image
                            img = Image.open(tmp)
                            buf = _io.BytesIO()
                            img.convert("RGB").save(buf, format="JPEG", quality=70)
                            frame = buf.getvalue()
                        finally:
                            try: _os.unlink(tmp)
                            except OSError: pass
                    else:
                        frame = None
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

        async def input_key(request: web.Request) -> web.Response:
            """Send keyboard input. Body: {"keycode": 30, "down": true}"""
            if not _self._display or not _self._display.connected:
                return web.json_response({"error": "no display"}, status=503)
            body = await request.json()
            keycode = body.get("keycode", 0)
            down = body.get("down", True)
            if down:
                _self._display.key_press(keycode)
            else:
                _self._display.key_release(keycode)
            return web.json_response({"ok": True})

        async def input_mouse(request: web.Request) -> web.Response:
            """Send mouse input. Body: {"x": 500, "y": 300, "button": 0, "action": "click"}"""
            if not _self._display or not _self._display.connected:
                return web.json_response({"error": "no display"}, status=503)
            body = await request.json()
            x = body.get("x", 0)
            y = body.get("y", 0)
            action = body.get("action", "move")
            button = body.get("button", 0)
            if action == "move":
                _self._display.mouse_move(x, y)
            elif action == "press":
                _self._display.mouse_move(x, y)
                _self._display.mouse_press(button)
            elif action == "release":
                _self._display.mouse_release(button)
            elif action == "click":
                _self._display.mouse_click(x, y, button)
            return web.json_response({"ok": True})

        async def input_type(request: web.Request) -> web.Response:
            """Type text. Body: {"text": "hello"}"""
            if not _self._display or not _self._display.connected:
                return web.json_response({"error": "no display"}, status=503)
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
            for ch in text:
                lc = ch.lower()
                shift = ch.isupper() or ch in CHAR_TO_EVDEV and CHAR_TO_EVDEV.get(ch, (0, False))[1]
                keycode, need_shift = CHAR_TO_EVDEV.get(lc, CHAR_TO_EVDEV.get(ch, (0, False)))
                if keycode:
                    if shift or need_shift:
                        _self._display.key_press(42)  # shift
                    _self._display.key_tap(keycode)
                    if shift or need_shift:
                        _self._display.key_release(42)
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
                "cap": "qmp,power",
                **({"api_port": str(self._api_port)} if self._api_port else {}),
                **({"vnc_host": self._vnc_host} if self._vnc_host else {}),
                **({"vnc_port": str(self._vnc_port)} if self._vnc_port else {}),
                **({"capture_device": self._virtual_capture.device_path}
                   if self._virtual_capture and self._virtual_capture.device_path else {}),
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

        if ptype == 0x01:  # keyboard
            events = self._kbd.diff(payload)
            if events:
                asyncio.create_task(
                    self._qmp.send_input_events(events),
                    name="qmp-kbd",
                )
        elif ptype == 0x02:  # mouse
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
    p = argparse.ArgumentParser(description="Ozma Soft Node (QMP backend)")
    p.add_argument("--name", required=True, help="Node name, e.g. 'vm1'")
    p.add_argument("--qmp", required=True, metavar="SOCKET",
                   help="QMP control socket, e.g. /tmp/ozma-vm1-ctrl.qmp")
    p.add_argument("--qmp-input", default="", metavar="SOCKET",
                   help="Dedicated QMP input socket (recommended). If omitted, shares --qmp.")
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
    p.add_argument("--debug", action="store_true")
    args = p.parse_args()

    logging.basicConfig(
        level=logging.DEBUG if args.debug else logging.INFO,
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
        datefmt="%H:%M:%S",
    )

    node = SoftNode(args.name, args.host, args.port, args.qmp,
                    vnc_host=args.vnc_host, vnc_port=args.vnc_port,
                    vnc_socket=args.vnc_socket,
                    capture_device=args.capture_device,
                    audio_sink=args.audio_sink, api_port=args.api_port,
                    qmp_input_path=args.qmp_input)

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
