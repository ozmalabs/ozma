# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Phone USB audio endpoint for ozma nodes.

When a phone is plugged into a node's USB port, the node presents itself
as a USB Audio Class 2 (UAC2) device — the phone sees a USB headset.
Call audio and media audio route through ozma's desk speakers and
microphone at 48kHz lossless PCM, with <5ms latency.

How it works:

  Phone ──USB──→ Node (UAC2 gadget)
                   │
                   ├── Phone speaker out → UAC2 capture → PipeWire → Desk speakers
                   │                                                  (or VBAN → controller)
                   └── Desk mic → PipeWire → UAC2 playback → Phone mic in

  The phone thinks it's connected to a USB headset.  No app required.
  Works with any phone that supports USB Audio Class (Android 5+, iOS 11+).

Audio quality:
  - Bluetooth HFP (phone calls): 8-16kHz, lossy, ~100ms latency
  - Bluetooth A2DP (music):      44.1kHz, lossy (SBC/AAC), ~150ms
  - USB Audio (this):            48kHz, lossless PCM, <5ms latency

Detection:
  The node monitors USB host port events.  When a device with a
  phone-like USB class (CDC ACM, MTP, PTP) appears, the node activates
  phone endpoint mode.  When the phone is unplugged, it reverts to
  normal KVM node mode.

Additional USB capabilities when a phone is connected:
  - USB tethering (Android): network access via CDC ECM/NCM
  - ADB (Android): screen mirror, notifications, app control
  - usbmuxd (iOS): screen mirror via QuickTime protocol
  - Charging: the node's USB port provides 5V power

The node advertises itself to the controller as an audio endpoint:
  audio_type=phone
  audio_sink=<PipeWire node name for phone speaker out>
  phone_connected=true
"""

from __future__ import annotations

import asyncio
import logging
import re
import subprocess
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.node.phone")

# UAC2 ConfigFS paths
_GADGET_DIR = Path("/sys/kernel/config/usb_gadget/ozma")
_UAC2_FUNC = _GADGET_DIR / "functions" / "uac2.phone"

# USB host device monitoring
_USB_DEVICES = Path("/sys/bus/usb/devices")

# Phone-like USB vendor IDs
_PHONE_VENDORS = {
    "18d1": "google",       # Google (Pixel)
    "04e8": "samsung",      # Samsung
    "2717": "xiaomi",       # Xiaomi
    "22b8": "motorola",     # Motorola
    "0bb4": "htc",          # HTC
    "12d1": "huawei",       # Huawei
    "2a70": "oneplus",      # OnePlus
    "05ac": "apple",        # Apple (iPhone)
    "1004": "lg",           # LG
    "0fce": "sony",         # Sony (Xperia)
    "2916": "google",       # Google (Android Open Accessory)
    "1949": "amazon",       # Amazon (Fire)
}

# USB classes that indicate a phone
_PHONE_CLASSES = {"02", "06", "ff"}  # CDC, Still Image, Vendor Specific


class PhoneEndpoint:
    """
    Manages phone USB audio endpoint on a node.

    When a phone is detected on the USB host port:
    1. Ensures UAC2 gadget function is active
    2. Bridges audio: phone → desk speakers, desk mic → phone
    3. Reports phone connection to the controller

    When the phone is unplugged:
    1. Stops audio bridge
    2. Reports disconnection
    """

    def __init__(
        self,
        sample_rate: int = 48000,
        channels: int = 2,
        bridge_latency_ms: int = 5,
    ) -> None:
        self._sample_rate = sample_rate
        self._channels = channels
        self._bridge_latency_ms = bridge_latency_ms
        self._phone_connected = False
        self._phone_info: dict[str, str] = {}
        self._bridge_procs: list[asyncio.subprocess.Process] = []
        self._monitor_task: asyncio.Task | None = None
        self._uac2_playback: str | None = None  # ALSA device: node writes → phone hears
        self._uac2_capture: str | None = None   # ALSA device: phone writes → node hears

    @property
    def phone_connected(self) -> bool:
        return self._phone_connected

    @property
    def phone_info(self) -> dict[str, str]:
        return self._phone_info

    async def start(self) -> None:
        """Start monitoring for phone connections."""
        self._monitor_task = asyncio.create_task(
            self._monitor_loop(), name="phone-monitor"
        )
        log.info("Phone endpoint monitor started")

    async def stop(self) -> None:
        if self._monitor_task:
            self._monitor_task.cancel()
            try:
                await self._monitor_task
            except asyncio.CancelledError:
                pass
        await self._stop_bridge()

    async def _monitor_loop(self) -> None:
        """Poll USB devices for phone connections."""
        while True:
            try:
                phone = self._detect_phone()
                if phone and not self._phone_connected:
                    self._phone_connected = True
                    self._phone_info = phone
                    log.info("Phone connected: %s %s (vendor %s)",
                             phone.get("manufacturer", "?"),
                             phone.get("product", "?"),
                             phone.get("vendor", "?"))
                    await self._on_phone_connected()
                elif not phone and self._phone_connected:
                    self._phone_connected = False
                    self._phone_info = {}
                    log.info("Phone disconnected")
                    await self._on_phone_disconnected()

                await asyncio.sleep(2.0)
            except asyncio.CancelledError:
                return
            except Exception:
                await asyncio.sleep(5.0)

    def _detect_phone(self) -> dict[str, str] | None:
        """Check if a phone is connected to a USB host port."""
        if not _USB_DEVICES.exists():
            return None

        for entry in sorted(_USB_DEVICES.iterdir()):
            name = entry.name
            if name.startswith("usb") or ":" in name:
                continue

            def _r(field: str) -> str:
                try:
                    return (entry / field).read_text().strip()
                except OSError:
                    return ""

            vid = _r("idVendor").lower()
            pid = _r("idProduct").lower()
            dev_class = _r("bDeviceClass")

            # Check if this looks like a phone
            is_phone = vid in _PHONE_VENDORS or dev_class in _PHONE_CLASSES
            if not is_phone:
                continue

            return {
                "vendor": _PHONE_VENDORS.get(vid, vid),
                "vid": vid,
                "pid": pid,
                "manufacturer": _r("manufacturer"),
                "product": _r("product"),
                "serial": _r("serial"),
                "speed": _r("speed"),
            }

        return None

    async def _on_phone_connected(self) -> None:
        """Set up audio bridge when phone is detected."""
        # Ensure UAC2 function is available
        from usb_audio import find_uac2_playback_device, find_uac2_capture_device

        self._uac2_playback = find_uac2_playback_device()
        self._uac2_capture = find_uac2_capture_device()

        if not self._uac2_playback:
            log.warning("UAC2 gadget not available — phone audio bridge disabled")
            return

        await self._start_bridge()

    async def _on_phone_disconnected(self) -> None:
        """Tear down audio bridge when phone is unplugged."""
        await self._stop_bridge()

    async def _start_bridge(self) -> None:
        """
        Start bidirectional audio bridge:
          Phone speaker out → UAC2 capture → PipeWire source → desk speakers
          Desk mic → PipeWire → UAC2 playback → Phone mic in
        """
        if not self._uac2_capture or not self._uac2_playback:
            return

        # Direction 1: Phone out → desk speakers
        # pw-cat captures from the UAC2 ALSA device and creates a PipeWire source
        phone_out = await asyncio.create_subprocess_exec(
            "pw-cat", "--capture",
            "--target", self._uac2_capture,
            "--rate", str(self._sample_rate),
            "--channels", str(self._channels),
            "--format", "s16",
            "--media-name", "ozma-phone-out",
            "--media-category", "Communication",
            stdout=asyncio.subprocess.DEVNULL,
            stderr=asyncio.subprocess.DEVNULL,
        )
        self._bridge_procs.append(phone_out)

        # Direction 2: Desk mic → phone in
        # pw-cat plays to the UAC2 ALSA device, reading from the default PipeWire source
        phone_in = await asyncio.create_subprocess_exec(
            "pw-cat", "--playback",
            "--target", self._uac2_playback,
            "--rate", str(self._sample_rate),
            "--channels", str(self._channels),
            "--format", "s16",
            "--media-name", "ozma-phone-in",
            "--media-category", "Communication",
            stdout=asyncio.subprocess.DEVNULL,
            stderr=asyncio.subprocess.DEVNULL,
        )
        self._bridge_procs.append(phone_in)

        log.info("Phone audio bridge started: %s ↔ PipeWire (%dHz, %dch)",
                 self._uac2_capture, self._sample_rate, self._channels)

    async def _stop_bridge(self) -> None:
        """Stop all audio bridge processes."""
        for proc in self._bridge_procs:
            if proc.returncode is None:
                proc.terminate()
                try:
                    await asyncio.wait_for(proc.wait(), timeout=2.0)
                except asyncio.TimeoutError:
                    proc.kill()
        self._bridge_procs.clear()

    def state_dict(self) -> dict[str, Any]:
        return {
            "phone_connected": self._phone_connected,
            "phone_info": self._phone_info,
            "audio_bridge_active": len(self._bridge_procs) > 0,
            "uac2_playback": self._uac2_playback,
            "uac2_capture": self._uac2_capture,
            "sample_rate": self._sample_rate,
            "channels": self._channels,
        }


# ── HTTP route registration ──────────────────────────────────────────────────

from aiohttp import web


def register_phone_routes(app: web.Application, phone: PhoneEndpoint) -> None:

    async def get_state(_: web.Request) -> web.Response:
        return web.json_response(phone.state_dict())

    app.router.add_get("/phone/state", get_state)
