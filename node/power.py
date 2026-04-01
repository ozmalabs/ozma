# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Power/reset control for hardware nodes — full lights-out management (LoM).

Controls the target machine's power and reset buttons via GPIO relays
on the node's SBC.  Also reads the power LED state to determine if the
target is on or off.

Hardware:
  - Power relay:  GPIO output, momentary pulse shorts the target's
                  power switch header pins.
  - Reset relay:  GPIO output, momentary pulse shorts the target's
                  reset switch header pins.
  - Power LED:    GPIO input, reads the target's power LED header.
                  HIGH = target is on.

Pulse durations:
  - Power on/off:   200ms (standard ATX momentary press)
  - Force shutdown:  5000ms (hold power button to force off)
  - Reset:          200ms

GPIO pins are configurable via constructor or CLI args.  The default
pins assume a Raspberry Pi or similar SBC with standard GPIO numbering.

Uses gpiod (libgpiod) for GPIO access — works on any Linux SBC with
a /dev/gpiochipN device.

HTTP API (registered on the node's aiohttp app):
  GET  /power/state    → {"powered": true|false, "available": true}
  POST /power/on       → pulse power relay (200ms)
  POST /power/off      → pulse power relay (200ms)
  POST /power/reset    → pulse reset relay (200ms)
  POST /power/force-off → hold power relay (5000ms)
  POST /power/cycle    → force-off, wait 2s, power on
"""

from __future__ import annotations

import asyncio
import logging
from typing import Any

from aiohttp import web

log = logging.getLogger("ozma.node.power")

# Default GPIO pins (BCM numbering, common on RPi)
DEFAULT_POWER_PIN = 17
DEFAULT_RESET_PIN = 27
DEFAULT_LED_PIN = 22
DEFAULT_GPIO_CHIP = "/dev/gpiochip0"

# Pulse durations (seconds)
PULSE_SHORT = 0.2     # Power on/off, reset
PULSE_FORCE = 5.0     # Force shutdown (hold power 5s)
CYCLE_WAIT = 2.0      # Wait between force-off and power-on

try:
    import gpiod
    _GPIOD_AVAILABLE = True
except ImportError:
    _GPIOD_AVAILABLE = False


class PowerController:
    """
    Controls target machine power/reset via GPIO relays.

    If gpiod is not available or GPIO fails to initialise, operates in
    stub mode — all actions return False, state reports unavailable.
    """

    def __init__(
        self,
        power_pin: int = DEFAULT_POWER_PIN,
        reset_pin: int = DEFAULT_RESET_PIN,
        led_pin: int = DEFAULT_LED_PIN,
        gpio_chip: str = DEFAULT_GPIO_CHIP,
    ) -> None:
        self._power_pin = power_pin
        self._reset_pin = reset_pin
        self._led_pin = led_pin
        self._gpio_chip = gpio_chip
        self._available = False
        self._chip = None
        self._power_line = None
        self._reset_line = None
        self._led_line = None
        self._lock = asyncio.Lock()

    async def start(self) -> bool:
        """Initialise GPIO lines.  Returns True if hardware is available."""
        if not _GPIOD_AVAILABLE:
            log.info("gpiod not available — power control in stub mode")
            return False

        try:
            self._chip = gpiod.Chip(self._gpio_chip)

            # Output lines (relays) — active high, default low (relay off)
            self._power_line = self._chip.get_line(self._power_pin)
            self._power_line.request(
                consumer="ozma-power",
                type=gpiod.LINE_REQ_DIR_OUT,
                default_vals=[0],
            )

            self._reset_line = self._chip.get_line(self._reset_pin)
            self._reset_line.request(
                consumer="ozma-reset",
                type=gpiod.LINE_REQ_DIR_OUT,
                default_vals=[0],
            )

            # Input line (power LED sense)
            self._led_line = self._chip.get_line(self._led_pin)
            self._led_line.request(
                consumer="ozma-led",
                type=gpiod.LINE_REQ_DIR_IN,
            )

            self._available = True
            log.info("Power control ready: power=GPIO%d reset=GPIO%d led=GPIO%d",
                     self._power_pin, self._reset_pin, self._led_pin)
            return True

        except Exception as e:
            log.warning("GPIO init failed — power control in stub mode: %s", e)
            self._available = False
            return False

    async def stop(self) -> None:
        """Release GPIO lines."""
        for line in (self._power_line, self._reset_line, self._led_line):
            if line:
                try:
                    line.release()
                except Exception:
                    pass
        if self._chip:
            try:
                self._chip.close()
            except Exception:
                pass

    @property
    def available(self) -> bool:
        return self._available

    def is_powered(self) -> bool | None:
        """Read the power LED state.  Returns None if unavailable."""
        if not self._available or not self._led_line:
            return None
        try:
            return bool(self._led_line.get_value())
        except Exception:
            return None

    async def power_on(self) -> bool:
        """Short press the power button (200ms)."""
        return await self._pulse(self._power_line, PULSE_SHORT, "power_on")

    async def power_off(self) -> bool:
        """Short press the power button (200ms) — same as power_on electrically."""
        return await self._pulse(self._power_line, PULSE_SHORT, "power_off")

    async def reset(self) -> bool:
        """Pulse the reset button (200ms)."""
        return await self._pulse(self._reset_line, PULSE_SHORT, "reset")

    async def force_off(self) -> bool:
        """Hold the power button for 5 seconds (force shutdown)."""
        return await self._pulse(self._power_line, PULSE_FORCE, "force_off")

    async def power_cycle(self) -> bool:
        """Force off, wait 2s, then power on."""
        async with self._lock:
            ok = await self._pulse_unlocked(self._power_line, PULSE_FORCE, "cycle_off")
            if not ok:
                return False
            await asyncio.sleep(CYCLE_WAIT)
            return await self._pulse_unlocked(self._power_line, PULSE_SHORT, "cycle_on")

    async def _pulse(self, line: Any, duration: float, label: str) -> bool:
        async with self._lock:
            return await self._pulse_unlocked(line, duration, label)

    async def _pulse_unlocked(self, line: Any, duration: float, label: str) -> bool:
        if not self._available or not line:
            log.debug("Power %s: unavailable (stub mode)", label)
            return False
        try:
            line.set_value(1)
            await asyncio.sleep(duration)
            line.set_value(0)
            log.info("Power %s: pulsed %.1fs", label, duration)
            return True
        except Exception as e:
            log.warning("Power %s failed: %s", label, e)
            return False

    def state_dict(self) -> dict[str, Any]:
        return {
            "available": self._available,
            "powered": self.is_powered(),
            "pins": {
                "power": self._power_pin,
                "reset": self._reset_pin,
                "led": self._led_pin,
            },
        }


# ── HTTP route registration ──────────────────────────────────────────────────

def register_power_routes(app: web.Application, power: PowerController) -> None:
    """Add power control HTTP endpoints to an aiohttp app."""

    async def get_state(_: web.Request) -> web.Response:
        return web.json_response(power.state_dict())

    async def post_action(request: web.Request) -> web.Response:
        action = request.match_info["action"]
        actions = {
            "on": power.power_on,
            "off": power.power_off,
            "reset": power.reset,
            "force-off": power.force_off,
            "cycle": power.power_cycle,
        }
        fn = actions.get(action)
        if not fn:
            return web.json_response(
                {"ok": False, "error": f"Unknown action: {action}"}, status=400
            )
        ok = await fn()
        return web.json_response({"ok": ok, "action": action})

    app.router.add_get("/power/state", get_state)
    app.router.add_post("/power/{action}", post_action)
