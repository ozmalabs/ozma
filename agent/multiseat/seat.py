# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Individual seat — one user's station within a multi-seat PC.

A Seat is a self-contained ozma node. It has its own:
  - Display (one physical monitor or virtual display)
  - Input devices (keyboard + mouse, grouped by USB hub)
  - Audio sink (PipeWire virtual null sink)
  - UDP listener (HID packets from the controller)
  - HTTP API (status, snapshot, HLS stream)
  - Controller registration (appears as an independent node)

The controller sees each seat as an independent machine. All multi-seat
complexity lives here in the agent.
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
import platform
import shutil
import socket
import struct
from pathlib import Path
from typing import Any

from aiohttp import web

log = logging.getLogger("ozma.agent.multiseat.seat")

PROTO_VERSION = 1


class Seat:
    """
    A single seat in a multi-seat PC.

    Each seat runs as an independent ozma node with its own UDP listener,
    HTTP API, screen capture, audio sink, and HID injector.
    """

    def __init__(
        self,
        name: str,
        seat_index: int,
        display_index: int,
        udp_port: int,
        api_port: int,
        input_devices: list[str] | None = None,
        audio_sink: str | None = None,
        capture_fps: int = 15,
        capture_width: int = 1920,
        capture_height: int = 1080,
        encoder_args: list[str] | None = None,
    ) -> None:
        self.name = name
        self.seat_index = seat_index
        self.display_index = display_index
        self.udp_port = udp_port
        self.api_port = api_port
        self.input_devices = input_devices or []
        self.audio_sink = audio_sink
        self.capture_fps = capture_fps
        self.capture_width = capture_width
        self.capture_height = capture_height
        self.encoder_args = encoder_args or []

        # Set during start()
        self.display: Any = None  # DisplayInfo, set by SeatManager
        self._stop_event = asyncio.Event()
        self._hid_injector: Any = None
        self._screen_proc: asyncio.subprocess.Process | None = None
        self._transport: asyncio.DatagramTransport | None = None
        self._output_dir = Path(f"/tmp/ozma-seat-{seat_index}")
        self._runner: web.AppRunner | None = None
        self._webrtc: Any = None  # SeatWebRTCHandler, set if aiortc available

        # HID state tracking
        self._prev_modifier: int = 0
        self._prev_keys: set[int] = set()
        self._prev_buttons: int = 0

    async def start(self, controller_url: str = "") -> None:
        """
        Start this seat: HID injector, screen capture, UDP listener, HTTP API.

        Args:
            controller_url: Controller URL for registration (empty = mDNS only)
        """
        log.info("Starting seat %s (index=%d, display=%d, udp=%d, http=%d)",
                 self.name, self.seat_index, self.display_index,
                 self.udp_port, self.api_port)

        # Prepare output directory for screen capture
        self._output_dir.mkdir(parents=True, exist_ok=True)

        # Initialize WebRTC handler (graceful if aiortc not installed)
        print(f"[SEAT {self.name}] init webrtc...", flush=True)
        self._init_webrtc()

        # Start HID injector (per-seat uinput devices)
        print(f"[SEAT {self.name}] start HID injector...", flush=True)
        try:
            await self._start_hid_injector()
        except Exception as e:
            print(f"[SEAT {self.name}] HID injector failed: {e}", flush=True)
            log.warning("Seat %s: HID injector failed: %s", self.name, e)

        # Start screen capture
        if self.capture_fps > 0:
            print(f"[SEAT {self.name}] start screen capture...", flush=True)
            try:
                await self._start_screen_capture()
            except Exception as e:
                print(f"[SEAT {self.name}] screen capture failed: {e}", flush=True)
                log.warning("Seat %s: screen capture failed: %s", self.name, e)

        # Start HTTP API
        print(f"[SEAT {self.name}] start HTTP API on port {self.api_port}...", flush=True)
        try:
            await self._start_http()
        except Exception as e:
            print(f"[SEAT {self.name}] HTTP API failed: {e}", flush=True)
            log.warning("Seat %s: HTTP API failed: %s", self.name, e)

        print(f"[SEAT {self.name}] HTTP API running", flush=True)

        # Register with controller
        if controller_url:
            asyncio.create_task(
                self._register(controller_url),
                name=f"register-{self.name}",
            )

        print(f"[SEAT {self.name}] starting UDP listener on port {self.udp_port}...", flush=True)
        # Start UDP listener (blocks until stopped)
        await self._serve()

    async def stop(self) -> None:
        """Clean shutdown of this seat."""
        log.info("Stopping seat %s", self.name)
        self._stop_event.set()

        # Stop WebRTC peer connections
        if self._webrtc:
            await self._webrtc.cleanup()
            self._webrtc = None

        # Stop screen capture
        if self._screen_proc and self._screen_proc.returncode is None:
            self._screen_proc.terminate()
            try:
                await asyncio.wait_for(self._screen_proc.wait(), timeout=5.0)
            except asyncio.TimeoutError:
                self._screen_proc.kill()
            self._screen_proc = None

        # Stop HID injector
        if self._hid_injector:
            await self._hid_injector.stop()
            self._hid_injector = None

        # Stop HTTP server
        if self._runner:
            await self._runner.cleanup()
            self._runner = None

        # Close UDP transport
        if self._transport:
            self._transport.close()
            self._transport = None

    def _init_webrtc(self) -> None:
        """Initialize WebRTC handler if aiortc is available."""
        try:
            from .webrtc_seat import SeatWebRTCHandler
            handler = SeatWebRTCHandler(self)
            if handler.available:
                self._webrtc = handler
                log.info("Seat %s: WebRTC available", self.name)
            else:
                log.debug("Seat %s: WebRTC unavailable (aiortc not installed)",
                          self.name)
        except Exception:
            # aiortc/av/PyAV can crash on import if ffmpeg libs are missing
            log.debug("Seat %s: WebRTC not available", self.name)

    async def _start_hid_injector(self) -> None:
        """Create per-seat virtual input devices via uinput."""
        system = platform.system()
        if system == "Linux":
            self._hid_injector = _SeatHIDInjectorLinux(
                kbd_name=f"ozma-kbd-{self.name}",
                mouse_name=f"ozma-mouse-{self.name}",
            )
        else:
            self._hid_injector = _SeatHIDInjectorStub()

        ok = await self._hid_injector.start()
        if not ok:
            log.warning("Seat %s: HID injector failed, using stub", self.name)
            self._hid_injector = _SeatHIDInjectorStub()
            await self._hid_injector.start()

    async def _start_screen_capture(self) -> None:
        """
        Start per-display screen capture via ffmpeg x11grab.

        Captures the region of the X root window corresponding to this
        seat's display, using the display's x_offset and y_offset.
        """
        if not shutil.which("ffmpeg"):
            log.warning("Seat %s: ffmpeg not found — capture disabled", self.name)
            return

        display_env = os.environ.get("DISPLAY", ":0")

        # Calculate capture region from display info
        grab_x = 0
        grab_y = 0
        width = self.capture_width
        height = self.capture_height

        if self.display:
            grab_x = self.display.x_offset
            grab_y = self.display.y_offset
            width = self.display.width
            height = self.display.height

        # ffmpeg x11grab with offset for this display
        x11_input = f"{display_env}+{grab_x},{grab_y}"

        cmd = [
            "ffmpeg", "-hide_banner", "-loglevel", "warning", "-y",
            "-f", "x11grab",
            "-framerate", str(self.capture_fps),
            "-video_size", f"{width}x{height}",
            "-i", x11_input,
        ]

        # Use allocated encoder args if available, otherwise fallback to libx264
        if self.encoder_args:
            cmd.extend(self.encoder_args)
        else:
            cmd.extend(["-c:v", "libx264", "-preset", "ultrafast",
                        "-tune", "zerolatency", "-crf", "28"])

        cmd.extend([
            "-f", "hls",
            "-hls_time", "1",
            "-hls_list_size", "4",
            "-hls_flags", "delete_segments+independent_segments",
            "-hls_segment_filename", str(self._output_dir / "seg_%05d.ts"),
            str(self._output_dir / "stream.m3u8"),
        ])

        try:
            self._screen_proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.PIPE,
            )
            await asyncio.sleep(1.0)
            if self._screen_proc.returncode is not None:
                stderr = await self._screen_proc.stderr.read()
                log.warning("Seat %s: screen capture failed: %s",
                            self.name, stderr.decode()[:200])
                self._screen_proc = None
                return

            log.info("Seat %s: screen capture active (x11grab %dx%d@%dfps at +%d+%d)",
                     self.name, width, height, self.capture_fps, grab_x, grab_y)

            asyncio.create_task(
                self._monitor_capture(), name=f"capture-mon-{self.name}"
            )
        except Exception as e:
            log.warning("Seat %s: screen capture start failed: %s", self.name, e)

    async def _monitor_capture(self) -> None:
        """Log ffmpeg stderr output for debugging."""
        if not self._screen_proc or not self._screen_proc.stderr:
            return
        try:
            async for line in self._screen_proc.stderr:
                text = line.decode(errors="replace").rstrip()
                if text:
                    log.debug("Seat %s capture: %s", self.name, text)
        except Exception:
            pass

    async def _start_http(self) -> None:
        """Start HTTP API for this seat: /status, /snapshot, /stream/."""
        app = web.Application()

        async def status_handler(_: web.Request) -> web.Response:
            return web.json_response(self.to_dict())

        async def snapshot_handler(_: web.Request) -> web.Response:
            data = await self._take_snapshot()
            if data:
                return web.Response(body=data, content_type="image/jpeg")
            return web.json_response({"error": "capture failed"}, status=503)

        async def health_handler(_: web.Request) -> web.Response:
            return web.json_response({"ok": True})

        app.router.add_get("/status", status_handler)
        app.router.add_get("/snapshot", snapshot_handler)
        app.router.add_get("/health", health_handler)

        # WebRTC routes (if aiortc is available)
        if self._webrtc:
            self._webrtc.add_routes(app)

        # Serve HLS segments
        if self._output_dir.exists():
            app.router.add_static("/stream/", self._output_dir)

        self._runner = web.AppRunner(app)
        await self._runner.setup()
        site = web.TCPSite(self._runner, "0.0.0.0", self.api_port)
        await site.start()
        log.info("Seat %s: HTTP API on port %d", self.name, self.api_port)

    async def _take_snapshot(self) -> bytes | None:
        """Capture a single JPEG frame for this seat's display region."""
        display_env = os.environ.get("DISPLAY", ":0")
        grab_x = self.display.x_offset if self.display else 0
        grab_y = self.display.y_offset if self.display else 0
        width = self.display.width if self.display else self.capture_width
        height = self.display.height if self.display else self.capture_height

        cmd = [
            "ffmpeg", "-hide_banner", "-loglevel", "error",
            "-f", "x11grab", "-framerate", "1",
            "-video_size", f"{width}x{height}",
            "-i", f"{display_env}+{grab_x},{grab_y}",
            "-frames:v", "1", "-f", "image2pipe", "-vcodec", "mjpeg", "pipe:1",
        ]
        try:
            proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.DEVNULL,
            )
            stdout, _ = await asyncio.wait_for(proc.communicate(), timeout=10)
            return stdout if stdout else None
        except Exception:
            return None

    async def _register(self, controller_url: str) -> None:
        """Register this seat as a node with the controller."""
        import urllib.request

        local_ip = self._resolve_local_ip()
        service_type = "_ozma._udp.local."
        node_id = f"{self.name}.{service_type}"

        body = {
            "id": node_id,
            "host": local_ip,
            "port": self.udp_port,
            "proto": str(PROTO_VERSION),
            "role": "compute",
            "hw": f"multiseat-{platform.system().lower()}",
            "fw": "1.0.0",
            "cap": "softnode,screen,multiseat",
            "api_port": str(self.api_port),
            "stream_port": str(self.api_port),
            "stream_path": "/stream/stream.m3u8",
            "audio_type": "pipewire" if self.audio_sink else "",
            "audio_sink": self.audio_sink or "",
            "seat_index": str(self.seat_index),
            "display_name": self.display.name if self.display else "",
        }

        url = f"{controller_url.rstrip('/')}/api/v1/nodes/register"
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
                result = await loop.run_in_executor(
                    None,
                    lambda: json.loads(urllib.request.urlopen(req, timeout=3).read()),
                )
                if result.get("ok"):
                    log.info("Seat %s: registered with controller", self.name)
                    asyncio.create_task(
                        self._re_register_loop(body, url),
                        name=f"re-register-{self.name}",
                    )
                    return
            except Exception:
                await asyncio.sleep(2)

        log.warning("Seat %s: registration failed after 10 attempts", self.name)

    async def _re_register_loop(self, body: dict, url: str) -> None:
        """Periodically re-register to handle controller restarts."""
        import urllib.request

        while not self._stop_event.is_set():
            await asyncio.sleep(60)
            if self._stop_event.is_set():
                break
            try:
                data = json.dumps(body).encode()
                req = urllib.request.Request(
                    url, data=data,
                    headers={"Content-Type": "application/json"},
                    method="POST",
                )
                loop = asyncio.get_running_loop()
                await loop.run_in_executor(
                    None,
                    lambda: urllib.request.urlopen(req, timeout=5).read(),
                )
            except Exception:
                pass

    async def _serve(self) -> None:
        """UDP datagram listener for HID packets from the controller."""
        loop = asyncio.get_running_loop()
        self._transport, _ = await loop.create_datagram_endpoint(
            lambda: _UDPProtocol(self._on_packet),
            local_addr=("0.0.0.0", self.udp_port),
        )
        log.info("Seat %s: listening on UDP port %d", self.name, self.udp_port)

        try:
            await self._stop_event.wait()
        finally:
            if self._transport:
                self._transport.close()
                self._transport = None

    def _on_packet(self, data: bytes, addr: tuple) -> None:
        """Handle incoming HID packet. Same format as DesktopSoftNode."""
        if len(data) < 2:
            return
        pkt_type = data[0]
        payload = data[1:]

        if pkt_type == 0x01 and len(payload) >= 8 and self._hid_injector:
            self._hid_injector.inject_keyboard(payload[:8])
        elif pkt_type == 0x02 and len(payload) >= 6 and self._hid_injector:
            self._hid_injector.inject_mouse(payload[:6])

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

    def to_dict(self) -> dict:
        """Serialize seat state for API responses."""
        capture_active = (
            self._screen_proc is not None
            and self._screen_proc.returncode is None
        )
        return {
            "name": self.name,
            "seat_index": self.seat_index,
            "display_index": self.display_index,
            "display_name": self.display.name if self.display else None,
            "udp_port": self.udp_port,
            "api_port": self.api_port,
            "input_devices": self.input_devices,
            "audio_sink": self.audio_sink,
            "capture": {
                "active": capture_active,
                "fps": self.capture_fps,
                "resolution": f"{self.capture_width}x{self.capture_height}",
                "stream_path": str(self._output_dir / "stream.m3u8") if capture_active else None,
                "encoder_args": self.encoder_args if self.encoder_args else None,
            },
            "webrtc": self._webrtc.to_dict() if self._webrtc else {
                "available": False, "peers": 0,
            },
        }


# ── Per-seat HID injectors ──────────────────────────────────────────────────

class _SeatHIDInjectorLinux:
    """
    Per-seat HID injector using uinput.

    Creates uniquely named virtual keyboard and mouse for this seat
    (e.g. "ozma-kbd-seat-0", "ozma-mouse-seat-0").
    """

    def __init__(self, kbd_name: str, mouse_name: str) -> None:
        self._kbd_name = kbd_name
        self._mouse_name = mouse_name
        self._kbd_dev: Any = None
        self._mouse_dev: Any = None
        self._prev_modifier: int = 0
        self._prev_keys: set[int] = set()

    async def start(self) -> bool:
        try:
            import evdev
            from evdev import UInput, AbsInfo, ecodes

            # Virtual keyboard
            kbd_cap = {ecodes.EV_KEY: list(range(1, 249))}
            self._kbd_dev = UInput(
                kbd_cap, name=self._kbd_name,
                vendor=0x1209, product=0x0001, version=1,
            )
            log.info("Seat keyboard: %s → %s", self._kbd_name, self._kbd_dev.device.path)

            # Virtual mouse (absolute + relative)
            mouse_cap = {
                ecodes.EV_KEY: [
                    ecodes.BTN_LEFT, ecodes.BTN_RIGHT, ecodes.BTN_MIDDLE,
                    ecodes.BTN_SIDE, ecodes.BTN_EXTRA,
                ],
                ecodes.EV_ABS: [
                    (ecodes.ABS_X, AbsInfo(0, 0, 32767, 0, 0, 0)),
                    (ecodes.ABS_Y, AbsInfo(0, 0, 32767, 0, 0, 0)),
                ],
                ecodes.EV_REL: [
                    ecodes.REL_X, ecodes.REL_Y,
                    ecodes.REL_WHEEL, ecodes.REL_HWHEEL,
                ],
            }
            self._mouse_dev = UInput(
                mouse_cap, name=self._mouse_name,
                vendor=0x1209, product=0x0002, version=1,
            )
            log.info("Seat mouse: %s → %s", self._mouse_name, self._mouse_dev.device.path)

            return True
        except Exception as e:
            log.warning("Seat HID injector failed: %s", e)
            return False

    def inject_keyboard(self, report: bytes) -> None:
        """Inject 8-byte HID keyboard boot report with proper state tracking."""
        if not self._kbd_dev:
            return
        from evdev import ecodes

        modifier = report[0]
        keys = {k for k in report[2:8] if k != 0}

        # HID modifier bit -> evdev keycode
        MOD_MAP = {
            0x01: 29,   # Left Ctrl
            0x02: 42,   # Left Shift
            0x04: 56,   # Left Alt
            0x08: 125,  # Left Meta
            0x10: 97,   # Right Ctrl
            0x20: 54,   # Right Shift
            0x40: 100,  # Right Alt
            0x80: 126,  # Right Meta
        }

        # Modifier diff
        for bit, evcode in MOD_MAP.items():
            was = bool(self._prev_modifier & bit)
            now = bool(modifier & bit)
            if now and not was:
                self._kbd_dev.write(ecodes.EV_KEY, evcode, 1)
            elif was and not now:
                self._kbd_dev.write(ecodes.EV_KEY, evcode, 0)

        # Key diff
        for hid_code in self._prev_keys - keys:
            evcode = _HID_TO_EVDEV.get(hid_code)
            if evcode:
                self._kbd_dev.write(ecodes.EV_KEY, evcode, 0)

        for hid_code in keys - self._prev_keys:
            evcode = _HID_TO_EVDEV.get(hid_code)
            if evcode:
                self._kbd_dev.write(ecodes.EV_KEY, evcode, 1)

        self._kbd_dev.syn()
        self._prev_modifier = modifier
        self._prev_keys = keys

    def inject_mouse(self, report: bytes) -> None:
        """Inject 6-byte HID mouse report (absolute coordinates)."""
        if not self._mouse_dev:
            return
        from evdev import ecodes

        buttons = report[0]
        x = report[1] | (report[2] << 8)
        y = report[3] | (report[4] << 8)
        scroll = struct.unpack("b", bytes([report[5]]))[0] if len(report) > 5 else 0

        self._mouse_dev.write(ecodes.EV_ABS, ecodes.ABS_X, x)
        self._mouse_dev.write(ecodes.EV_ABS, ecodes.ABS_Y, y)

        self._mouse_dev.write(ecodes.EV_KEY, ecodes.BTN_LEFT, 1 if buttons & 1 else 0)
        self._mouse_dev.write(ecodes.EV_KEY, ecodes.BTN_RIGHT, 1 if buttons & 2 else 0)
        self._mouse_dev.write(ecodes.EV_KEY, ecodes.BTN_MIDDLE, 1 if buttons & 4 else 0)

        if scroll:
            self._mouse_dev.write(ecodes.EV_REL, ecodes.REL_WHEEL, scroll)

        self._mouse_dev.syn()

    async def stop(self) -> None:
        if self._kbd_dev:
            try:
                self._kbd_dev.close()
            except Exception:
                pass
            self._kbd_dev = None
        if self._mouse_dev:
            try:
                self._mouse_dev.close()
            except Exception:
                pass
            self._mouse_dev = None


class _SeatHIDInjectorStub:
    """Stub injector for platforms without uinput."""

    async def start(self) -> bool:
        log.info("HID injector: stub (no uinput)")
        return True

    def inject_keyboard(self, report: bytes) -> None:
        pass

    def inject_mouse(self, report: bytes) -> None:
        pass

    async def stop(self) -> None:
        pass


# ── UDP protocol ──────────────────────────────────────────────────────────────

class _UDPProtocol(asyncio.DatagramProtocol):
    def __init__(self, callback):
        self._callback = callback

    def datagram_received(self, data: bytes, addr: tuple) -> None:
        self._callback(data, addr)


# ── HID keycode table ─────────────────────────────────────────────────────────

# HID usage ID -> Linux evdev keycode (same table as evdev_input.py)
_HID_TO_EVDEV: dict[int, int] = {
    0x04: 30,   # a
    0x05: 48,   # b
    0x06: 46,   # c
    0x07: 32,   # d
    0x08: 18,   # e
    0x09: 33,   # f
    0x0A: 34,   # g
    0x0B: 35,   # h
    0x0C: 23,   # i
    0x0D: 36,   # j
    0x0E: 37,   # k
    0x0F: 38,   # l
    0x10: 50,   # m
    0x11: 49,   # n
    0x12: 24,   # o
    0x13: 25,   # p
    0x14: 16,   # q
    0x15: 19,   # r
    0x16: 31,   # s
    0x17: 20,   # t
    0x18: 22,   # u
    0x19: 47,   # v
    0x1A: 17,   # w
    0x1B: 45,   # x
    0x1C: 21,   # y
    0x1D: 44,   # z
    0x1E: 2,    # 1
    0x1F: 3,    # 2
    0x20: 4,    # 3
    0x21: 5,    # 4
    0x22: 6,    # 5
    0x23: 7,    # 6
    0x24: 8,    # 7
    0x25: 9,    # 8
    0x26: 10,   # 9
    0x27: 11,   # 0
    0x28: 28,   # Enter
    0x29: 1,    # Escape
    0x2A: 14,   # Backspace
    0x2B: 15,   # Tab
    0x2C: 57,   # Space
    0x2D: 12,   # -
    0x2E: 13,   # =
    0x2F: 26,   # [
    0x30: 27,   # ]
    0x31: 43,   # backslash
    0x33: 39,   # ;
    0x34: 40,   # '
    0x35: 41,   # `
    0x36: 51,   # ,
    0x37: 52,   # .
    0x38: 53,   # /
    0x39: 58,   # CapsLock
    0x3A: 59,   # F1
    0x3B: 60,   # F2
    0x3C: 61,   # F3
    0x3D: 62,   # F4
    0x3E: 63,   # F5
    0x3F: 64,   # F6
    0x40: 65,   # F7
    0x41: 66,   # F8
    0x42: 67,   # F9
    0x43: 68,   # F10
    0x44: 87,   # F11
    0x45: 88,   # F12
    0x46: 99,   # PrintScreen
    0x47: 70,   # ScrollLock
    0x48: 119,  # Pause
    0x49: 110,  # Insert
    0x4A: 102,  # Home
    0x4B: 104,  # PageUp
    0x4C: 111,  # Delete
    0x4D: 107,  # End
    0x4E: 109,  # PageDown
    0x4F: 106,  # Right
    0x50: 105,  # Left
    0x51: 108,  # Down
    0x52: 103,  # Up
    0x53: 69,   # NumLock
    0x65: 127,  # Menu/Compose
}
