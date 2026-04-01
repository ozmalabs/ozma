# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Ozma Node — full hardware KVM node.

Combines:
  • USB HID gadget output   (keyboard + mouse → /dev/hidg0, /dev/hidg1)
  • V4L2 video capture      (HDMI grabber → HW-encoded HLS stream)
  • Audio capture           (HDMI/line audio → AAC in HLS)
  • mDNS announcement       (_ozma._udp.local.)
  • UDP HID receiver        (from controller, same wire format as tinynode)
  • HTTP stream server      (serves HLS at /stream/stream.m3u8)

mDNS TXT fields advertised:
  proto=1
  role=compute
  hw=<platform>          e.g. rpi4, milkv-duos, x86, ...
  fw=<version>
  cap=hid,video,audio    capabilities present
  stream_port=<port>     HTTP port serving HLS (default 7380)
  stream_path=/stream/stream.m3u8

Usage:
  python3 -m node.node --name mynode --hid-udp-port 7331

  With explicit device overrides:
  python3 -m node.node \\
      --capture-device /dev/video0 \\
      --audio-device hw:3,0 \\
      --kbd /dev/hidg0 \\
      --mouse /dev/hidg1
"""

from __future__ import annotations

import argparse
import asyncio
import logging
import platform
import signal
import socket
import sys
from pathlib import Path

from aiohttp import web
from zeroconf import ServiceInfo
from zeroconf.asyncio import AsyncZeroconf

sys.path.insert(0, str(Path(__file__).parent))
from hw_detect import detect_encoder, detect_capture_devices, best_capture_device, CaptureDevice
from usb_hid import USBHIDGadget
from usb_audio import USBAudioGadget
from capture import MediaCapture
from power import PowerController, register_power_routes
from current_sensor import CurrentSensor, register_current_routes
from rpa import NodeRPA
from rgb_leds import RGBController, register_rgb_routes
from phone_endpoint import PhoneEndpoint, register_phone_routes
from usb_pd import USBPDController, register_pd_routes
from serial_capture import NodeSerialCapture, register_serial_routes
from expansion_sensors import ExpansionSensorManager, register_sensor_routes
from connect_client import NodeConnectClient
from prometheus_metrics import collect_all as collect_prometheus
from self_management import NodeSelfManager

log = logging.getLogger("ozma.node")

PROTO_VERSION = 1
STREAM_DIR = Path("/tmp/ozma-stream")
STREAM_PATH = "/stream/stream.m3u8"


# ---------------------------------------------------------------------------
# Platform detection

def detect_platform() -> str:
    """Best-effort hardware platform string."""
    # Raspberry Pi
    try:
        model = Path("/proc/device-tree/model").read_text(errors="ignore").rstrip("\x00")
        if model:
            if "Raspberry Pi" in model:
                return "rpi-" + model.split("Raspberry Pi")[1].strip().split()[0].lower()
            return model[:32].lower().replace(" ", "-")
    except OSError:
        pass
    # Milk-V / RISC-V SoCs
    try:
        cpuinfo = Path("/proc/cpuinfo").read_text()
        if "sg2000" in cpuinfo.lower() or "cv1800" in cpuinfo.lower():
            return "milkv-duos"
    except OSError:
        pass
    return platform.machine()  # x86_64, aarch64, riscv64, ...


# ---------------------------------------------------------------------------
# HTTP stream server (aiohttp)

def _read_usb_info() -> dict:
    """
    Returns USB device info from this node's perspective:
      gadget  — the composite USB gadget this node presents to a target host
      attached — USB devices physically attached to this node's host ports
    """
    # ── Gadget (presented by this node) ──────────────────────────────────────
    gadget_dir = Path("/sys/kernel/config/usb_gadget/ozma")
    gadget: dict = {"active": False, "udc": None, "functions": []}
    if gadget_dir.exists():
        try:
            udc = (gadget_dir / "UDC").read_text().strip()
            gadget["active"] = bool(udc)
            gadget["udc"] = udc or None
        except OSError:
            pass
        funcs_dir = gadget_dir / "functions"
        if funcs_dir.exists():
            for fn in sorted(funcs_dir.iterdir()):
                func: dict = {"name": fn.name}
                kind = fn.name.split(".")[0]
                if kind == "hid":
                    try:
                        proto = int((fn / "protocol").read_text().strip())
                        func["subtype"] = "keyboard" if proto == 1 else "mouse" if proto == 2 else f"hid-{proto}"
                    except OSError:
                        func["subtype"] = "hid"
                elif kind == "uac2":
                    func["subtype"] = "audio-uac2"
                    try:
                        func["rate"] = int((fn / "p_srate").read_text().strip())
                        func["channels"] = bin(int((fn / "p_chmask").read_text().strip())).count("1")
                    except OSError:
                        pass
                elif kind == "mass_storage":
                    func["subtype"] = "mass-storage"
                else:
                    func["subtype"] = kind
                gadget["functions"].append(func)

    # ── Attached (plugged into this node's USB host ports) ───────────────────
    attached: list[dict] = []
    usb_root = Path("/sys/bus/usb/devices")
    if usb_root.exists():
        for entry in sorted(usb_root.iterdir()):
            name = entry.name
            # Skip virtual root hub entries and per-interface entries
            if name.startswith("usb") or ":" in name:
                continue

            def _r(field: str, base: Path = entry) -> str | None:
                try:
                    return (base / field).read_text().strip()
                except OSError:
                    return None

            vid = _r("idVendor")
            if not vid:
                continue
            attached.append({
                "busid":        name,
                "vid":          vid,
                "pid":          _r("idProduct"),
                "manufacturer": _r("manufacturer"),
                "product":      _r("product"),
                "speed":        _r("speed"),      # Mbps: "1.5","12","480","5000"
                "class":        _r("bDeviceClass"),
            })

    return {"gadget": gadget, "attached": attached}


def build_http_app(stream_dir: Path) -> web.Application:
    app = web.Application()
    stream_dir.mkdir(parents=True, exist_ok=True)
    app.router.add_static("/stream", str(stream_dir), show_index=False)

    async def health(_: web.Request) -> web.Response:
        return web.json_response({"ok": True})

    async def usb(_: web.Request) -> web.Response:
        return web.json_response(_read_usb_info())

    app.router.add_get("/health", health)
    app.router.add_get("/usb", usb)
    return app


# ---------------------------------------------------------------------------
# Main node

class OzmaNode:
    def __init__(
        self,
        name: str,
        hid_udp_port: int = 7331,
        http_port: int = 7382,
        kbd_path: str = "/dev/hidg0",
        mouse_path: str = "/dev/hidg1",
        capture_device: CaptureDevice | None = None,
        audio_device: str | None = None,
        stream_dir: Path = STREAM_DIR,
        no_hid: bool = False,
        no_video: bool = False,
        no_audio_gadget: bool = False,
        no_power: bool = False,
        power_pin: int = 17,
        reset_pin: int = 27,
        led_pin: int = 22,
        no_current: bool = False,
        no_rgb: bool = False,
        rgb_leds: int = 30,
        register_url: str | None = None,
        register_host: str | None = None,
    ) -> None:
        self._name = name
        self._hid_udp_port = hid_udp_port
        self._http_port = http_port
        self._kbd_path = kbd_path
        self._mouse_path = mouse_path
        self._capture_device = capture_device
        self._audio_device = audio_device
        self._stream_dir = stream_dir
        self._no_hid = no_hid
        self._no_video = no_video
        self._no_audio_gadget = no_audio_gadget
        self._no_power = no_power
        self._power_pin = power_pin
        self._reset_pin = reset_pin
        self._led_pin = led_pin
        self._no_current = no_current
        self._no_rgb = no_rgb
        self._rgb_leds = rgb_leds
        self._register_url = register_url  # bypass mDNS for dev/QEMU SLIRP environments
        self._register_host = register_host  # override host sent to controller (SLIRP port-forward)

        self._gadget: USBHIDGadget | None = None
        self._audio_gadget: USBAudioGadget | None = None
        self._capture: MediaCapture | None = None
        self._power: PowerController | None = None
        self._current: CurrentSensor | None = None
        self._rgb: RGBController | None = None
        self._phone: PhoneEndpoint | None = None
        self._pd: USBPDController | None = None
        self._serial: NodeSerialCapture | None = None
        self._expansion: ExpansionSensorManager | None = None
        self._self_mgr = NodeSelfManager()
        self._azc: AsyncZeroconf | None = None
        self._service_info: ServiceInfo | None = None
        self._stop_event = asyncio.Event()
        service_type = "_ozma._udp.local."
        self._connect = NodeConnectClient(
            f"{name}.{service_type}", node_type="hardware",
            hid_port=hid_udp_port,
        )

    async def run(self) -> None:
        hw = detect_platform()
        caps: list[str] = []

        # --- USB HID gadget ---
        if not self._no_hid:
            try:
                self._gadget = await USBHIDGadget.open(self._kbd_path, self._mouse_path)
                caps.append("hid")
                log.info("USB HID gadget ready")
            except Exception as e:
                log.warning("USB HID gadget unavailable: %s", e)

        # --- Video + audio capture ---
        if not self._no_video:
            cap_dev = self._capture_device
            if cap_dev is None:
                devices = detect_capture_devices()
                cap_dev = best_capture_device(devices)
                if cap_dev:
                    log.info("Auto-selected capture device: %s (%s)", cap_dev.path, cap_dev.name)
                else:
                    log.warning("No V4L2 capture device found — video disabled")

            if cap_dev:
                try:
                    encoder = detect_encoder(prefer_hevc=True)
                    log.info("Video encoder: %s", encoder.name)

                    audio = self._audio_device or cap_dev.audio_device
                    self._capture = MediaCapture(
                        cap_dev, encoder, self._stream_dir, audio_device=audio
                    )

                    # USB audio gadget: route captured audio to host via UAC2
                    if audio and not self._no_audio_gadget:
                        try:
                            self._audio_gadget = await USBAudioGadget.open()
                            if self._audio_gadget.playback_device:
                                self._capture.add_uac2_output(
                                    self._audio_gadget.playback_device
                                )
                                caps.append("uac2-audio")
                                log.info(
                                    "USB audio gadget active → %s",
                                    self._audio_gadget.playback_device,
                                )
                        except Exception as e:
                            log.warning("USB audio gadget unavailable: %s", e)

                    await self._capture.start()
                    caps.append("video")
                    if audio:
                        caps.append("audio")
                except Exception as e:
                    log.error("Failed to start video capture: %s", e)

        # --- Power/reset control ---
        if not self._no_power:
            self._power = PowerController(
                power_pin=self._power_pin,
                reset_pin=self._reset_pin,
                led_pin=self._led_pin,
            )
            if await self._power.start():
                caps.append("power")

        # --- Current sensor ---
        if not self._no_current:
            self._current = CurrentSensor()
            if await self._current.start():
                caps.append("current")

        # --- RGB LEDs ---
        if not self._no_rgb:
            self._rgb = RGBController(led_count=self._rgb_leds)
            if await self._rgb.start():
                caps.append("rgb")

        # --- USB Power Delivery ---
        self._pd = USBPDController()
        if await self._pd.start():
            caps.append("usb-pd")

        # --- Expansion sensors (Enterprise Node I2C header) ---
        self._expansion = ExpansionSensorManager()
        if await self._expansion.start():
            caps.append("sensors")

        # --- RPA engine (OCR + automation) ---
        cap_path = self._capture_device or "/dev/video0"
        if isinstance(cap_path, CaptureDevice):
            cap_path = cap_path.path
        self._rpa = NodeRPA(
            capture_device=cap_path,
            kbd_device=self._kbd_path,
            mouse_device=self._mouse_path,
        )
        caps.append("rpa")

        # --- Serial console capture ---
        self._serial = NodeSerialCapture()
        if await self._serial.start():
            caps.append("serial")

        # --- Phone USB endpoint (auto-detects phone connection) ---
        self._phone = PhoneEndpoint()
        caps.append("phone")  # Always advertise capability; connection is dynamic

        # --- HTTP server (always; serves HLS when video active, /usb and /health always) ---
        app = build_http_app(self._stream_dir)
        if self._power:
            register_power_routes(app, self._power)
        if self._current:
            register_current_routes(app, self._current)
        if self._rgb:
            register_rgb_routes(app, self._rgb)
        register_phone_routes(app, self._phone)
        if self._serial and self._serial.connected:
            register_serial_routes(app, self._serial)
        if self._expansion and self._expansion.available:
            register_sensor_routes(app, self._expansion)
        if self._pd:
            register_pd_routes(app, self._pd)

        # --- RPA API routes ---
        _rpa = self._rpa
        async def rpa_read_screen(req: web.Request) -> web.Response:
            mode = req.query.get("mode", "auto")
            state = await _rpa.read_screen(mode=mode)
            return web.json_response({
                "full_text": state.full_text,
                "text_regions": [
                    {"text": r.text, "x": r.x, "y": r.y,
                     "width": r.width, "height": r.height,
                     "confidence": r.confidence}
                    for r in state.text_regions
                ],
            })
        async def rpa_key(req: web.Request) -> web.Response:
            body = await req.json()
            await _rpa.key(body["key"], modifier=body.get("modifier", 0))
            return web.json_response({"ok": True})
        async def rpa_click(req: web.Request) -> web.Response:
            body = await req.json()
            await _rpa.click(body["x"], body["y"], button=body.get("button", 1))
            return web.json_response({"ok": True})
        async def rpa_type(req: web.Request) -> web.Response:
            body = await req.json()
            await _rpa.type_text(body["text"], delay=body.get("delay", 0.03))
            return web.json_response({"ok": True})
        async def rpa_wait_for_text(req: web.Request) -> web.Response:
            body = await req.json()
            state = await _rpa.wait_for_text(
                body["text"], timeout=body.get("timeout", 60),
                interval=body.get("interval", 1.0),
                mode=body.get("mode", "auto"),
            )
            if state:
                return web.json_response({"found": True, "full_text": state.full_text})
            return web.json_response({"found": False})
        async def rpa_run_script(req: web.Request) -> web.Response:
            body = await req.json()
            asyncio.create_task(_rpa.run_script(body["script"]), name="rpa-script")
            return web.json_response({"ok": True, "status": "running"})
        async def rpa_enter_bios(req: web.Request) -> web.Response:
            body = await req.json() if req.can_read_body else {}
            key = body.get("key", "delete")
            ok = await _rpa.enter_bios(key=key)
            return web.json_response({"ok": ok})
        async def rpa_set_boot_usb(req: web.Request) -> web.Response:
            ok = await _rpa.set_boot_usb()
            return web.json_response({"ok": ok})
        async def rpa_screenshot(req: web.Request) -> web.Response:
            frame = await _rpa.grab_frame()
            if frame is None:
                return web.json_response({"error": "capture failed"}, status=500)
            import io
            buf = io.BytesIO()
            frame.save(buf, format="JPEG", quality=80)
            return web.Response(body=buf.getvalue(), content_type="image/jpeg")

        app.router.add_get("/api/v1/rpa/screen", rpa_read_screen)
        app.router.add_post("/api/v1/rpa/key", rpa_key)
        app.router.add_post("/api/v1/rpa/click", rpa_click)
        app.router.add_post("/api/v1/rpa/type", rpa_type)
        app.router.add_post("/api/v1/rpa/wait_for_text", rpa_wait_for_text)
        app.router.add_post("/api/v1/rpa/script", rpa_run_script)
        app.router.add_post("/api/v1/rpa/enter_bios", rpa_enter_bios)
        app.router.add_post("/api/v1/rpa/set_boot_usb", rpa_set_boot_usb)
        app.router.add_get("/api/v1/rpa/screenshot", rpa_screenshot)

        # Connection state + Prometheus metrics (all subsystems)
        _node = self
        async def connection_state(_: web.Request) -> web.Response:
            return web.json_response(_node._connect.state.to_dict())
        async def metrics(_: web.Request) -> web.Response:
            text = collect_prometheus(
                node_name=_node._name,
                connect_client=_node._connect,
                self_manager=_node._self_mgr,
                current_sensor=_node._current,
                pd_controller=_node._pd,
                power_controller=_node._power,
                rgb_controller=_node._rgb,
                capture=_node._capture,
                audio_gadget=_node._audio_gadget,
                phone_endpoint=_node._phone,
                expansion_sensors=_node._expansion,
                serial_capture=_node._serial,
            )
            return web.Response(text=text, content_type="text/plain; version=0.0.4")
        app.router.add_get("/api/v1/connection", connection_state)
        app.router.add_get("/metrics", metrics)

        # Start self-management monitoring
        await _node._self_mgr.start()

        runner = web.AppRunner(app)
        await runner.setup()
        site = web.TCPSite(runner, "0.0.0.0", self._http_port)
        await site.start()
        await self._phone.start()
        log.info("HTTP API on port %d", self._http_port)
        if "video" in caps:
            log.info("HLS stream at %s", STREAM_PATH)

        # --- mDNS ---
        local_ip = self._local_ip()
        txt: dict[str, str] = {
            "proto": str(PROTO_VERSION),
            "role": "compute",
            "hw": hw,
            "fw": "1.0.0",
            "cap": ",".join(caps),
            "api_port": str(self._http_port),
        }
        if "video" in caps:
            txt["stream_port"] = str(self._http_port)
            txt["stream_path"] = STREAM_PATH
        if "rgb" in caps:
            txt["rgb_leds"] = str(self._rgb_leds)
        if "serial" in caps and self._serial:
            txt["serial_port"] = self._serial.port
        if "sensors" in caps and self._expansion:
            txt["sensors"] = ",".join(self._expansion.sensor_types)

        service_type = "_ozma._udp.local."
        self._service_info = ServiceInfo(
            service_type,
            f"{self._name}.{service_type}",
            addresses=[socket.inet_aton(local_ip)],
            port=self._hid_udp_port,
            properties=txt,
        )
        self._azc = AsyncZeroconf()
        await self._azc.async_register_service(self._service_info)
        log.info("mDNS announced: %s @ %s  caps=%s", self._name, local_ip, caps)

        # --- Direct registration (dev/QEMU: bypasses mDNS multicast) ---
        if self._register_url:
            await self._direct_register(local_ip, txt)

        # --- Connect registration (nodes connect directly to the mesh) ---
        await self._connect.start(
            capabilities=",".join(caps),
            version="1.0.0",
            extra={k: v for k, v in txt.items() if k not in ("proto", "role", "cap")},
        )

        # --- UDP HID listener ---
        loop = asyncio.get_running_loop()
        transport, _ = await loop.create_datagram_endpoint(
            lambda: _HIDProtocol(self._on_hid_packet),
            local_addr=("0.0.0.0", self._hid_udp_port),
        )
        log.info("Listening for HID packets on UDP port %d", self._hid_udp_port)

        try:
            await self._stop_event.wait()
        finally:
            transport.close()
            if self._capture:
                await self._capture.stop()
            if self._gadget:
                await self._gadget.close()
            if self._audio_gadget:
                await self._audio_gadget.close()
            if self._power:
                await self._power.stop()
            if self._current:
                await self._current.stop()
            if self._rgb:
                await self._rgb.stop()
            if self._expansion:
                await self._expansion.stop()
            if self._serial:
                await self._serial.stop()
            if self._phone:
                await self._phone.stop()
            if self._pd:
                await self._pd.stop()
            if self._azc and self._service_info:
                await self._azc.async_unregister_service(self._service_info)
                await self._azc.async_close()
            await runner.cleanup()

    async def stop(self) -> None:
        self._stop_event.set()

    def _on_hid_packet(self, data: bytes, addr: tuple) -> None:
        if not data or not self._gadget:
            return
        self._connect.record_hid_packet()
        ptype = data[0]
        payload = data[1:]
        if ptype == 0x01 and len(payload) == 8:
            asyncio.create_task(self._gadget.write_keyboard(payload), name="hid-kbd")
        elif ptype == 0x02 and len(payload) == 6:
            asyncio.create_task(self._gadget.write_mouse(payload), name="hid-mouse")

    async def _direct_register(self, local_ip: str, txt: dict[str, str]) -> None:
        """
        POST registration data directly to a controller URL.
        Used in QEMU/SLIRP environments where mDNS multicast can't cross the
        SLIRP boundary.  The controller's /api/v1/nodes/register endpoint
        accepts the same fields as a mDNS TXT record.
        """
        from aiohttp import ClientSession, ClientError, ClientTimeout

        payload = {
            "id": f"{self._name}._ozma._udp.local.",
            "host": self._register_host or local_ip,
            "port": self._hid_udp_port,
            **txt,
        }
        url = f"{self._register_url.rstrip('/')}/api/v1/nodes/register"
        try:
            async with ClientSession(timeout=ClientTimeout(total=5)) as session:
                async with session.post(url, json=payload) as resp:
                    if resp.status == 200:
                        log.info("Direct registration OK → %s", url)
                    else:
                        body = await resp.text()
                        log.warning("Direct registration failed (%d): %s", resp.status, body)
        except ClientError as e:
            log.warning("Direct registration error (will retry on reconnect): %s", e)

    @staticmethod
    def _local_ip() -> str:
        try:
            s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            s.connect(("8.8.8.8", 80))
            ip = s.getsockname()[0]
            s.close()
            return ip
        except OSError:
            return "127.0.0.1"


class _HIDProtocol(asyncio.DatagramProtocol):
    def __init__(self, callback) -> None:
        self._cb = callback

    def datagram_received(self, data: bytes, addr: tuple) -> None:
        self._cb(data, addr)

    def error_received(self, exc: Exception) -> None:
        log.warning("UDP error: %s", exc)


# ---------------------------------------------------------------------------
# Controller-side integration: tell StreamManager about stream_url nodes
# This runs in the node process to patch the controller's discovery if co-located,
# but the real path is via mDNS TXT → controller discovery.py reading stream_port.

def _integrate_controller_stream(state_module_path: str | None = None) -> None:
    """No-op placeholder — integration happens in controller/discovery.py."""


# ---------------------------------------------------------------------------
# Entry point

def main() -> None:
    p = argparse.ArgumentParser(description="Ozma hardware KVM node")
    p.add_argument("--name", default=socket.gethostname(), help="Node name (default: hostname)")
    p.add_argument("--hid-udp-port", type=int, default=7331, help="UDP port for HID packets")
    p.add_argument("--http-port", type=int, default=7382, help="HTTP port for HLS stream")
    p.add_argument("--kbd", default="/dev/hidg0", help="Keyboard gadget device")
    p.add_argument("--mouse", default="/dev/hidg1", help="Mouse gadget device")
    p.add_argument("--capture-device", default=None, help="V4L2 capture device (auto-detect if omitted)")
    p.add_argument("--audio-device", default=None, help="ALSA audio device, e.g. hw:3,0")
    p.add_argument("--stream-dir", default=str(STREAM_DIR), help="HLS output directory")
    p.add_argument("--no-hid", action="store_true", help="Disable USB HID output")
    p.add_argument("--no-video", action="store_true", help="Disable video capture")
    p.add_argument("--no-audio-gadget", action="store_true", help="Disable USB audio gadget (UAC2)")
    p.add_argument("--no-power", action="store_true", help="Disable power/reset control")
    p.add_argument("--power-pin", type=int, default=17, help="GPIO pin for power relay (BCM, default 17)")
    p.add_argument("--reset-pin", type=int, default=27, help="GPIO pin for reset relay (BCM, default 27)")
    p.add_argument("--led-pin", type=int, default=22, help="GPIO pin for power LED sense (BCM, default 22)")
    p.add_argument("--no-current", action="store_true", help="Disable USB current measurement")
    p.add_argument("--no-rgb", action="store_true", help="Disable RGB LED output")
    p.add_argument("--rgb-leds", type=int, default=30, help="Number of RGB LEDs (default 30)")
    p.add_argument("--register-host", default=None, metavar="HOST",
                   help="Override host reported to controller (for SLIRP: use 'localhost')")
    p.add_argument("--register-url", default=None, metavar="URL",
                   help="POST registration directly to controller URL (dev/QEMU: bypasses mDNS)")
    p.add_argument("--debug", action="store_true")
    args = p.parse_args()

    logging.basicConfig(
        level=logging.DEBUG if args.debug else logging.INFO,
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
        datefmt="%H:%M:%S",
    )

    cap_device = None
    if args.capture_device:
        from hw_detect import CaptureDevice
        cap_device = CaptureDevice(
            path=args.capture_device,
            name=args.capture_device,
            formats=["MJPG", "YUYV"],
            max_width=1920,
            max_height=1080,
        )

    node = OzmaNode(
        name=args.name,
        hid_udp_port=args.hid_udp_port,
        http_port=args.http_port,
        kbd_path=args.kbd,
        mouse_path=args.mouse,
        capture_device=cap_device,
        audio_device=args.audio_device,
        stream_dir=Path(args.stream_dir),
        no_hid=args.no_hid,
        no_video=args.no_video,
        no_audio_gadget=args.no_audio_gadget,
        no_power=args.no_power,
        power_pin=args.power_pin,
        reset_pin=args.reset_pin,
        led_pin=args.led_pin,
        no_current=args.no_current,
        no_rgb=args.no_rgb,
        rgb_leds=args.rgb_leds,
        register_url=args.register_url,
        register_host=args.register_host,
    )

    async def run() -> None:
        loop = asyncio.get_running_loop()
        stop = asyncio.Event()
        for sig in (signal.SIGINT, signal.SIGTERM):
            loop.add_signal_handler(sig, stop.set)
        task = asyncio.create_task(node.run())
        await stop.wait()
        await node.stop()
        await task

    asyncio.run(run())


if __name__ == "__main__":
    main()
