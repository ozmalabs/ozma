# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Concrete screen drivers for specific device types.

Each driver implements the ScreenDriver protocol for its tier:
  - FramePushDriver subclasses: Stream Deck, OLED, web, e-ink
  - NativeRenderDriver subclasses: ESP32, Android, RPi touchscreen
  - ConstrainedDriver subclasses: X-Touch LCD, 7-segment, character LCD

Device-specific protocol handling is encapsulated here.
The screen_manager.py dispatches to whichever driver is registered.
"""

from __future__ import annotations

import asyncio
import json
import logging
import struct
from typing import Any

from screen_manager import FramePushDriver, NativeRenderDriver, ConstrainedDriver

log = logging.getLogger("ozma.screen_drivers")


# ── Tier 1: Frame Push Drivers ───────────────────────────────────────────────

class StreamDeckKeyDriver(FramePushDriver):
    """Push rendered frames to individual Stream Deck keys."""

    def __init__(self, deck: Any, key_index: int) -> None:
        self._deck = deck
        self._key_index = key_index

        async def _push(frame: bytes) -> None:
            try:
                from StreamDeck.ImageHelpers import PILHelper
                from PIL import Image
                import io
                img = Image.open(io.BytesIO(frame))
                native = PILHelper.to_native_format(self._deck, img)
                with self._deck:
                    self._deck.set_key_image(self._key_index, native)
            except Exception:
                pass

        super().__init__(push_fn=_push)


class NodeOLEDDriver(FramePushDriver):
    """Push frames to a node's OLED display via HTTP."""

    def __init__(self, host: str, port: int) -> None:
        self._host = host
        self._port = port

        async def _push(frame: bytes) -> None:
            import urllib.request
            try:
                loop = asyncio.get_running_loop()
                req = urllib.request.Request(
                    f"http://{self._host}:{self._port}/oled/image",
                    data=frame, headers={"Content-Type": "image/png"}, method="POST",
                )
                await loop.run_in_executor(None, lambda: urllib.request.urlopen(req, timeout=2))
            except Exception:
                pass

        super().__init__(push_fn=_push)


class WebScreenDriver(FramePushDriver):
    """
    Serves rendered frames via HTTP for web dashboard widgets.

    The dashboard fetches /api/v1/screens/{id}/frame to get the latest PNG.
    This driver just stores the latest frame in memory.
    """

    def __init__(self) -> None:
        self._latest_frame: bytes = b""
        super().__init__(push_fn=self._store)

    async def _store(self, frame: bytes) -> None:
        self._latest_frame = frame

    @property
    def latest_frame(self) -> bytes:
        return self._latest_frame


class EInkDriver(FramePushDriver):
    """
    Push frames to e-ink/e-paper displays.

    E-ink is slow to refresh (~1-15s) but uses zero power when static.
    Good for persistent status displays that don't need real-time updates.
    Typically connected via SPI on a node's SBC.
    """

    def __init__(self, host: str, port: int) -> None:
        self._host = host
        self._port = port
        self._last_push = 0.0
        self._min_interval = 60.0  # Don't refresh more than once per minute

        async def _push(frame: bytes) -> None:
            import time
            now = time.monotonic()
            if now - self._last_push < self._min_interval:
                return
            self._last_push = now
            import urllib.request
            try:
                loop = asyncio.get_running_loop()
                req = urllib.request.Request(
                    f"http://{self._host}:{self._port}/eink/image",
                    data=frame, headers={"Content-Type": "image/png"}, method="POST",
                )
                await loop.run_in_executor(None, lambda: urllib.request.urlopen(req, timeout=10))
            except Exception:
                pass

        super().__init__(push_fn=_push)


class HDMIOverlayDriver(FramePushDriver):
    """
    Inject an OSD overlay onto a captured HDMI feed.

    Renders a status panel as a transparent PNG overlay, composited
    on top of the active capture feed by the controller's compositor.
    """

    def __init__(self, capture_source_id: str) -> None:
        self._source_id = capture_source_id
        self._latest_overlay: bytes = b""

        async def _push(frame: bytes) -> None:
            self._latest_overlay = frame

        super().__init__(push_fn=_push)

    @property
    def overlay_frame(self) -> bytes:
        return self._latest_overlay


# ── Tier 2: Native Render Drivers ────────────────────────────────────────────

class ESP32Driver(NativeRenderDriver):
    """ESP32 with TFT display — connects via WebSocket on port 7391."""

    def __init__(self, device_id: str) -> None:
        super().__init__(device_id)
        # The WebSocket connection is set by the screen_server when the device connects


class AndroidAppDriver(NativeRenderDriver):
    """
    Android ozma app — connects via WebSocket.

    Same protocol as ESP32 but supports richer widget set including
    touch interaction (tap to switch scenario, swipe to change view).
    """

    def __init__(self, device_id: str) -> None:
        super().__init__(device_id)
        self._supports_touch = True


class RPiTouchscreenDriver(NativeRenderDriver):
    """
    Raspberry Pi with official 7" touchscreen.

    Runs a web browser in kiosk mode displaying a locally-served page.
    Data updates stream via WebSocket. The page renders using the same
    widget library as the Node.js renderer but in the browser.
    """

    def __init__(self, device_id: str, host: str = "localhost") -> None:
        super().__init__(device_id)
        self._host = host


class CorsairLCDDriver(NativeRenderDriver):
    """
    Corsair iCUE LCD (AIO cooler, keyboard).

    Corsair LCDs accept frame data via USB HID. For the native tier,
    we push a UI definition and the device renders locally (if using
    custom firmware). For stock firmware, falls back to frame push.

    480x480 on AIO coolers. Various sizes on keyboards.
    """

    def __init__(self, device_id: str, hid_path: str = "") -> None:
        super().__init__(device_id)
        self._hid_path = hid_path


class NZXTKrakenDriver(NativeRenderDriver):
    """
    NZXT Kraken AIO LCD.

    Similar to Corsair — USB HID protocol for frame data.
    320x320 on newer models.
    """

    def __init__(self, device_id: str) -> None:
        super().__init__(device_id)


class LEDMatrixDriver(NativeRenderDriver):
    """
    LED matrix displays (MAX7219, HUB75).

    Connected via serial or UDP to an ESP32/Arduino driving the matrix.
    Low resolution (8x8 to 64x64) but high visibility.
    Renders simple metrics: numbers, bars, scrolling text.
    """

    def __init__(self, device_id: str, host: str = "", protocol: str = "udp") -> None:
        super().__init__(device_id)
        self._host = host
        self._protocol = protocol


# ── Tier 3: Constrained Drivers ──────────────────────────────────────────────

class XTouchLCDDriver(ConstrainedDriver):
    """
    Behringer X-Touch scribble strip (14 chars, colour, invert).

    Maps metrics to the LCD: top line = scenario name, bottom line = value.
    Colour follows scenario colour (mapped to nearest LCD colour).
    """

    def __init__(self, midi_io: Any) -> None:
        self._midi = midi_io

        async def _push(values: dict[str, Any]) -> None:
            if not self._midi:
                return
            from midi import Color, _hex_to_lcd_color
            top = str(values.get("scenario_name", ""))[:7]
            bottom = str(values.get("_primary_metric", ""))[:7]
            color = _hex_to_lcd_color(values.get("scenario_color"))
            text = f"{top:^7}{bottom:^7}"
            self._midi.lcd_update(text, color)

        super().__init__(push_fn=_push)


class SevenSegmentDriver(ConstrainedDriver):
    """
    7-segment displays (HID or serial).

    Shows a single numeric value. Maps the primary metric to the display.
    """

    def __init__(self, push_fn: Any = None) -> None:
        super().__init__(push_fn=push_fn)


class CharacterLCDDriver(ConstrainedDriver):
    """
    Character LCD (HD44780, 16x2 or 20x4).

    Connected via I2C on a node's SBC. Maps metrics to lines.
    Line 1: scenario name, Line 2: primary metric value.
    """

    def __init__(self, host: str, port: int, lines: int = 2, cols: int = 16) -> None:
        self._host = host
        self._port = port
        self._lines = lines
        self._cols = cols

        async def _push(values: dict[str, Any]) -> None:
            import urllib.request
            display_lines = []
            display_lines.append(str(values.get("scenario_name", "Ozma"))[:cols])
            if "cpu_temp" in values:
                display_lines.append(f"CPU:{values['cpu_temp']:.0f}C")
            elif "_primary_metric" in values:
                display_lines.append(str(values["_primary_metric"])[:cols])

            try:
                loop = asyncio.get_running_loop()
                data = json.dumps({"lines": display_lines}).encode()
                req = urllib.request.Request(
                    f"http://{host}:{port}/lcd/text",
                    data=data, headers={"Content-Type": "application/json"}, method="POST",
                )
                await loop.run_in_executor(None, lambda: urllib.request.urlopen(req, timeout=2))
            except Exception:
                pass

        super().__init__(push_fn=_push)


# ── Driver registry ──────────────────────────────────────────────────────────

DRIVER_TYPES: dict[str, type] = {
    # Tier 1
    "streamdeck_key": StreamDeckKeyDriver,
    "oled": NodeOLEDDriver,
    "web": WebScreenDriver,
    "eink": EInkDriver,
    "hdmi_overlay": HDMIOverlayDriver,
    # Tier 2
    "esp32": ESP32Driver,
    "android": AndroidAppDriver,
    "rpi_touch": RPiTouchscreenDriver,
    "corsair_lcd": CorsairLCDDriver,
    "nzxt_lcd": NZXTKrakenDriver,
    "led_matrix": LEDMatrixDriver,
    # Tier 3
    "xtouch_lcd": XTouchLCDDriver,
    "seven_seg": SevenSegmentDriver,
    "char_lcd": CharacterLCDDriver,
}
