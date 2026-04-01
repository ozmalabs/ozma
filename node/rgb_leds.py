# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
RGB LED output for ozma nodes.

Drives WS2812B / SK6812 addressable LEDs via SPI on the node's SBC.
The controller pushes scenario colours and effects to the node on
scenario switch.

Hardware:
  - WS2812B or SK6812 LED strip connected to SPI MOSI (or GPIO via
    rpi_ws281x on Raspberry Pi)
  - Optional external power supply input for longer strips

Effects:
  - solid: all LEDs one colour
  - breathe: fade in/out
  - chase: running dot
  - wave: colour sweep left→right
  - off: all LEDs off

HTTP API:
  GET  /rgb/state       → {"led_count": 30, "color": [r,g,b], "effect": "solid", "available": true}
  POST /rgb/set         → {"color": [r,g,b]} or {"color": [r,g,b], "effect": "breathe"}
  POST /rgb/off         → turn all LEDs off
"""

from __future__ import annotations

import asyncio
import logging
import math
import time
from typing import Any

from aiohttp import web

log = logging.getLogger("ozma.node.rgb")

DEFAULT_LED_COUNT = 30
DEFAULT_SPI_DEVICE = "/dev/spidev0.0"
DEFAULT_SPI_SPEED = 6_400_000  # 6.4 MHz (WS2812 timing)

try:
    import spidev
    _SPI_AVAILABLE = True
except ImportError:
    _SPI_AVAILABLE = False


class RGBController:
    """
    Drives addressable RGB LEDs via SPI.

    Falls back to stub mode if spidev is not available or SPI fails
    to open.  In stub mode, state is tracked but no hardware output.
    """

    def __init__(
        self,
        led_count: int = DEFAULT_LED_COUNT,
        spi_bus: int = 0,
        spi_device: int = 0,
        spi_speed: int = DEFAULT_SPI_SPEED,
    ) -> None:
        self._led_count = led_count
        self._spi_bus = spi_bus
        self._spi_device = spi_device
        self._spi_speed = spi_speed
        self._spi = None
        self._available = False

        # State
        self._color: tuple[int, int, int] = (0, 0, 0)
        self._effect: str = "off"
        self._brightness: float = 1.0
        self._effect_task: asyncio.Task | None = None

    async def start(self) -> bool:
        if not _SPI_AVAILABLE:
            log.info("spidev not available — RGB LEDs in stub mode")
            return False

        try:
            self._spi = spidev.SpiDev()
            self._spi.open(self._spi_bus, self._spi_device)
            self._spi.max_speed_hz = self._spi_speed
            self._spi.mode = 0
            self._available = True
            self._set_all(0, 0, 0)  # Start off
            log.info("RGB LEDs ready: %d LEDs on SPI%d.%d",
                     self._led_count, self._spi_bus, self._spi_device)
            return True
        except Exception as e:
            log.info("SPI init failed — RGB LEDs in stub mode: %s", e)
            return False

    async def stop(self) -> None:
        if self._effect_task:
            self._effect_task.cancel()
            try:
                await self._effect_task
            except asyncio.CancelledError:
                pass
        if self._available:
            self._set_all(0, 0, 0)
        if self._spi:
            try:
                self._spi.close()
            except Exception:
                pass

    @property
    def available(self) -> bool:
        return self._available

    @property
    def led_count(self) -> int:
        return self._led_count

    async def set_color(self, r: int, g: int, b: int, effect: str = "solid") -> None:
        """Set colour and optional effect."""
        self._color = (r, g, b)
        self._effect = effect

        # Stop any running effect
        if self._effect_task and not self._effect_task.done():
            self._effect_task.cancel()
            try:
                await self._effect_task
            except asyncio.CancelledError:
                pass

        if effect == "solid":
            self._set_all(r, g, b)
        elif effect == "off":
            self._set_all(0, 0, 0)
        elif effect in ("breathe", "chase", "wave"):
            self._effect_task = asyncio.create_task(
                self._run_effect(effect, r, g, b), name=f"rgb-{effect}"
            )
        else:
            self._set_all(r, g, b)

    async def off(self) -> None:
        await self.set_color(0, 0, 0, "off")

    def state_dict(self) -> dict[str, Any]:
        return {
            "led_count": self._led_count,
            "color": list(self._color),
            "effect": self._effect,
            "brightness": self._brightness,
            "available": self._available,
        }

    # ── LED output ───────────────────────────────────────────────────────────

    def _set_all(self, r: int, g: int, b: int) -> None:
        """Set all LEDs to one colour."""
        r = int(r * self._brightness)
        g = int(g * self._brightness)
        b = int(b * self._brightness)
        data = self._encode_ws2812([(g, r, b)] * self._led_count)
        self._write(data)

    def _set_leds(self, leds: list[tuple[int, int, int]]) -> None:
        """Set individual LED colours (list of (r, g, b) tuples)."""
        grb = [
            (int(g * self._brightness), int(r * self._brightness), int(b * self._brightness))
            for r, g, b in leds
        ]
        data = self._encode_ws2812(grb)
        self._write(data)

    def _write(self, data: bytes) -> None:
        if self._spi and self._available:
            try:
                self._spi.xfer2(list(data))
            except Exception:
                pass

    @staticmethod
    def _encode_ws2812(grb_pixels: list[tuple[int, int, int]]) -> bytes:
        """
        Encode GRB pixels to WS2812 SPI bitstream.
        Each bit is encoded as a ~400ns or ~800ns pulse at ~6.4MHz SPI clock.
        Bit 1 = 0b110, Bit 0 = 0b100 (3 SPI bits per WS2812 bit).
        """
        out = bytearray()
        for g, r, b in grb_pixels:
            for byte_val in (g, r, b):
                for bit in range(7, -1, -1):
                    if byte_val & (1 << bit):
                        out.append(0b110)
                    else:
                        out.append(0b100)
        # Reset pulse (>50us low)
        out.extend(b'\x00' * 20)
        return bytes(out)

    # ── Effects ──────────────────────────────────────────────────────────────

    async def _run_effect(self, effect: str, r: int, g: int, b: int) -> None:
        try:
            match effect:
                case "breathe":
                    await self._effect_breathe(r, g, b)
                case "chase":
                    await self._effect_chase(r, g, b)
                case "wave":
                    await self._effect_wave(r, g, b)
        except asyncio.CancelledError:
            return

    async def _effect_breathe(self, r: int, g: int, b: int) -> None:
        """Fade in/out continuously."""
        t = 0.0
        while True:
            brightness = (math.sin(t * 2.0) + 1.0) / 2.0
            self._set_all(
                int(r * brightness), int(g * brightness), int(b * brightness)
            )
            await asyncio.sleep(1 / 30)
            t += 1 / 30

    async def _effect_chase(self, r: int, g: int, b: int) -> None:
        """Running dot."""
        pos = 0
        while True:
            leds = [(0, 0, 0)] * self._led_count
            # 3-LED wide dot with fade
            for offset in range(-1, 2):
                idx = (pos + offset) % self._led_count
                dim = 1.0 if offset == 0 else 0.3
                leds[idx] = (int(r * dim), int(g * dim), int(b * dim))
            self._set_leds(leds)
            await asyncio.sleep(0.05)
            pos = (pos + 1) % self._led_count

    async def _effect_wave(self, r: int, g: int, b: int) -> None:
        """Colour sweep left to right."""
        t = 0.0
        while True:
            leds = []
            for i in range(self._led_count):
                phase = (i / self._led_count + t) * math.pi * 2
                brightness = (math.sin(phase) + 1.0) / 2.0
                leds.append((
                    int(r * brightness),
                    int(g * brightness),
                    int(b * brightness),
                ))
            self._set_leds(leds)
            await asyncio.sleep(1 / 30)
            t += 0.02


# ── HTTP route registration ──────────────────────────────────────────────────

def register_rgb_routes(app: web.Application, rgb: RGBController) -> None:

    async def get_state(_: web.Request) -> web.Response:
        return web.json_response(rgb.state_dict())

    async def post_set(request: web.Request) -> web.Response:
        try:
            body = await request.json()
        except Exception:
            return web.json_response({"ok": False, "error": "Invalid JSON"}, status=400)

        color = body.get("color", [0, 0, 0])
        if not isinstance(color, list) or len(color) != 3:
            return web.json_response({"ok": False, "error": "color must be [r,g,b]"}, status=400)
        r, g, b = [max(0, min(255, int(c))) for c in color]
        effect = body.get("effect", "solid")
        await rgb.set_color(r, g, b, effect)
        return web.json_response({"ok": True})

    async def post_off(_: web.Request) -> web.Response:
        await rgb.off()
        return web.json_response({"ok": True})

    app.router.add_get("/rgb/state", get_state)
    app.router.add_post("/rgb/set", post_set)
    app.router.add_post("/rgb/off", post_off)
