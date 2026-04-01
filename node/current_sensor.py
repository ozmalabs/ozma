# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
USB output current measurement for ozma nodes.

Reads the current draw on the node's USB output to the target machine
using an INA219 or INA226 I2C current/power monitor.  This enables:

  - Monitoring target machine USB power draw
  - Detecting if the target is powered on (current > threshold)
  - Alerting on overcurrent conditions
  - Historical current data for the dashboard

Hardware:
  - INA219 or INA226 breakout on the USB output VBUS line
  - I2C connected to the SBC (typically bus 1)
  - Shunt resistor: 0.1 ohm (default)

HTTP API:
  GET /current          → {"mA": 245.3, "mW": 1234.5, "V": 5.03, "available": true}
  GET /current/history  → {"samples": [[timestamp_ms, mA], ...], "interval_ms": 1000}
"""

from __future__ import annotations

import asyncio
import logging
import time
from collections import deque
from typing import Any

from aiohttp import web

log = logging.getLogger("ozma.node.current")

# INA219 defaults
DEFAULT_I2C_BUS = 1
DEFAULT_I2C_ADDR = 0x40
DEFAULT_SHUNT_OHMS = 0.1
HISTORY_SIZE = 300       # Keep 5 minutes at 1s intervals
SAMPLE_INTERVAL = 1.0    # seconds

try:
    import smbus2
    _I2C_AVAILABLE = True
except ImportError:
    _I2C_AVAILABLE = False

# INA219 registers
_REG_CONFIG = 0x00
_REG_SHUNT_VOLTAGE = 0x01
_REG_BUS_VOLTAGE = 0x02
_REG_POWER = 0x03
_REG_CURRENT = 0x04
_REG_CALIBRATION = 0x05


class CurrentSensor:
    """
    Reads USB output current via INA219 I2C sensor.

    Falls back to stub mode if smbus2 is not available or the sensor
    is not detected.
    """

    def __init__(
        self,
        i2c_bus: int = DEFAULT_I2C_BUS,
        i2c_addr: int = DEFAULT_I2C_ADDR,
        shunt_ohms: float = DEFAULT_SHUNT_OHMS,
    ) -> None:
        self._bus_num = i2c_bus
        self._addr = i2c_addr
        self._shunt_ohms = shunt_ohms
        self._bus = None
        self._available = False
        self._current_lsb = 0.0  # A per LSB
        self._task: asyncio.Task | None = None
        self._history: deque[tuple[float, float]] = deque(maxlen=HISTORY_SIZE)

        # Latest readings
        self.current_mA: float = 0.0
        self.voltage_V: float = 0.0
        self.power_mW: float = 0.0

    async def start(self) -> bool:
        if not _I2C_AVAILABLE:
            log.info("smbus2 not available — current sensor in stub mode")
            return False

        try:
            self._bus = smbus2.SMBus(self._bus_num)
            # Configure INA219: 32V range, 320mA max, continuous
            # Calibration for 0.1 ohm shunt, 3.2A max
            max_current = 3.2  # A
            self._current_lsb = max_current / 32768.0
            cal = int(0.04096 / (self._current_lsb * self._shunt_ohms))
            self._bus.write_word_data(self._addr, _REG_CALIBRATION, _swap16(cal))
            # Config: bus 32V, shunt 320mV, 12-bit, continuous
            config = 0x399F
            self._bus.write_word_data(self._addr, _REG_CONFIG, _swap16(config))
            self._available = True
            log.info("Current sensor ready on I2C bus %d addr 0x%02X",
                     self._bus_num, self._addr)
        except Exception as e:
            log.info("Current sensor not detected: %s", e)
            return False

        self._task = asyncio.create_task(self._sample_loop(), name="current-sample")
        return True

    async def stop(self) -> None:
        if self._task:
            self._task.cancel()
            try:
                await self._task
            except asyncio.CancelledError:
                pass
        if self._bus:
            try:
                self._bus.close()
            except Exception:
                pass

    @property
    def available(self) -> bool:
        return self._available

    def read(self) -> dict[str, Any]:
        return {
            "mA": round(self.current_mA, 1),
            "mW": round(self.power_mW, 1),
            "V": round(self.voltage_V, 3),
            "available": self._available,
        }

    def history(self) -> dict[str, Any]:
        return {
            "samples": list(self._history),
            "interval_ms": int(SAMPLE_INTERVAL * 1000),
        }

    async def _sample_loop(self) -> None:
        while True:
            try:
                await asyncio.sleep(SAMPLE_INTERVAL)
                self._read_sensor()
                self._history.append((time.time() * 1000, self.current_mA))
            except asyncio.CancelledError:
                return
            except Exception:
                pass

    def _read_sensor(self) -> None:
        if not self._bus or not self._available:
            return
        try:
            # Bus voltage (mV)
            raw = _swap16(self._bus.read_word_data(self._addr, _REG_BUS_VOLTAGE))
            self.voltage_V = (raw >> 3) * 0.004  # 4mV per LSB, shift out status bits

            # Current (mA)
            raw = _swap16(self._bus.read_word_data(self._addr, _REG_CURRENT))
            if raw > 32767:
                raw -= 65536
            self.current_mA = raw * self._current_lsb * 1000.0

            # Power (mW)
            raw = _swap16(self._bus.read_word_data(self._addr, _REG_POWER))
            self.power_mW = raw * self._current_lsb * 20.0 * 1000.0
        except Exception:
            pass


def _swap16(val: int) -> int:
    """Swap bytes in a 16-bit word (I2C byte order)."""
    return ((val & 0xFF) << 8) | ((val >> 8) & 0xFF)


# ── HTTP route registration ──────────────────────────────────────────────────

def register_current_routes(app: web.Application, sensor: CurrentSensor) -> None:
    async def get_current(_: web.Request) -> web.Response:
        return web.json_response(sensor.read())

    async def get_history(_: web.Request) -> web.Response:
        return web.json_response(sensor.history())

    app.router.add_get("/current", get_current)
    app.router.add_get("/current/history", get_history)
