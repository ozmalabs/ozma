# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
#!/usr/bin/env python3
"""
Ozma Desktop Soft Node — make any PC appear as an ozma node.

No hardware required. Install on any Linux, macOS, or Windows machine
and it appears on the ozma Controller as a manageable node — just like
a hardware node plugged in via USB, but over the network.

What it provides:
  - HID injection: receives keyboard/mouse packets from the Controller
    and injects them into the local input system (uinput on Linux,
    CGEvent on macOS, SendInput on Windows)
  - Audio routing: creates a virtual audio device that the Controller
    can route to/from (PipeWire/PulseAudio on Linux, CoreAudio on macOS,
    WASAPI loopback on Windows)
  - Display capture: captures the screen for streaming/OCR via the
    universal stream router (PipeWire screen capture, Wayland portal,
    or X11 grab)
  - mDNS announcement: appears as a standard ozma node on the network

This is the "Ozma Soft Node" product — free tier allows 1 device,
Pro tier allows 5, Team/Business unlimited.

Usage:
  pip install ozma-softnode
  ozma-softnode --name my-desktop
  ozma-softnode --name my-desktop --controller http://10.0.0.1:7380
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

log = logging.getLogger("ozma.softnode.desktop")

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


class HIDInjectorStub:
    """Stub injector for platforms where injection isn't available yet."""

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


# ── Audio backend ───────────────────────────────────────────────────────────

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
        else:
            self._hid = HIDInjectorStub()
            log.info("HID injection on %s not yet implemented — stub mode", system)

        await self._hid.start()

        # Audio backend
        if system == "Linux":
            self._audio = AudioBackendLinux(self._name)
            self._audio_sink = await self._audio.start()

        # Screen capture
        if self._capture_fps > 0:
            capture_dir = f"/tmp/ozma-softnode-{self._name}"
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
        capture_dir = Path(f"/tmp/ozma-softnode-{self._name}")
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
    p = argparse.ArgumentParser(description="Ozma Desktop Soft Node")
    p.add_argument("--name", default=platform.node(), help="Node name (default: hostname)")
    p.add_argument("--port", type=int, default=7331, help="UDP listen port")
    p.add_argument("--api-port", type=int, default=7382, help="HTTP API port (stream + status)")
    p.add_argument("--controller", default="", help="Controller URL (e.g., http://10.0.0.1:7380)")
    p.add_argument("--fps", type=int, default=15, help="Screen capture FPS")
    p.add_argument("--no-capture", action="store_true", help="Disable screen capture")
    p.add_argument("--debug", action="store_true")
    args = p.parse_args()

    logging.basicConfig(
        level=logging.DEBUG if args.debug else logging.INFO,
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
        datefmt="%H:%M:%S",
    )

    node = DesktopSoftNode(
        name=args.name, port=args.port,
        api_port=args.api_port,
        controller_url=args.controller,
        capture_fps=0 if args.no_capture else args.fps,
    )

    loop = asyncio.new_event_loop()

    def _on_signal():
        loop.call_soon_threadsafe(node._stop_event.set)

    for sig in (signal.SIGINT, signal.SIGTERM):
        loop.add_signal_handler(sig, _on_signal)

    try:
        loop.run_until_complete(node.run())
    except KeyboardInterrupt:
        pass
    finally:
        loop.run_until_complete(node.stop())
        loop.close()


if __name__ == "__main__":
    main()
