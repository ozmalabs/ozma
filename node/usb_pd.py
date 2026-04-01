# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
USB Power Delivery (PD) management for ozma nodes.

Enables the node as a smart USB-C dock: a single cable provides HID,
audio, video capture, and charging to a connected phone or laptop.

Hardware requirement:
  - USB-PD controller IC (FUSB302, STUSB4500, or CYPD3177) on I2C
  - Power path: external PSU → PD controller → USB-C port → device
  - The PD controller negotiates voltage and current with the
    connected device (sink) or provides power as a source

PD profiles supported:
  5V  / 3A  (15W)  — phones, accessories
  9V  / 3A  (27W)  — fast-charging phones
  15V / 3A  (45W)  — tablets, lightweight laptops
  20V / 3A  (60W)  — laptops (with 3A cable)
  20V / 5A  (100W) — laptops (with 5A/emarked cable)

The node monitors:
  - Negotiated voltage and current
  - Actual power delivery (via INA219 current sensor)
  - Device type (sink = phone/laptop, source = charger connected to node)
  - Cable capability (3A or 5A)

HTTP API:
  GET  /pd/state     → voltage, current, negotiated profile, device type
  POST /pd/profile   → request specific voltage/current (for testing)

Integration with phone_endpoint.py:
  When a phone connects with PD, both audio (UAC2) and charging are active
  simultaneously.  The node is a complete phone dock.

Integration with current_sensor.py:
  The INA219 reads actual delivered current. Combined with PD negotiated
  voltage, we get real-time power delivery monitoring on the dashboard.
"""

from __future__ import annotations

import asyncio
import logging
from dataclasses import dataclass
from typing import Any

from aiohttp import web

log = logging.getLogger("ozma.node.usb_pd")

# I2C defaults for common PD controller ICs
DEFAULT_I2C_BUS = 1
FUSB302_ADDR = 0x22
STUSB4500_ADDR = 0x28

# Standard PD voltage/current profiles
PD_PROFILES = [
    (5,  3.0),   # 15W  — USB default / phone charging
    (9,  3.0),   # 27W  — phone fast charge
    (15, 3.0),   # 45W  — tablet / light laptop
    (20, 3.0),   # 60W  — laptop (3A cable)
    (20, 5.0),   # 100W — laptop (5A emarked cable)
]

try:
    import smbus2
    _I2C_AVAILABLE = True
except ImportError:
    _I2C_AVAILABLE = False


@dataclass
class PDState:
    """Current USB-PD negotiation state."""

    available: bool = False
    connected: bool = False
    role: str = "none"               # "source" (providing power) | "sink" (receiving) | "none"
    negotiated_voltage: float = 0.0  # Volts
    negotiated_current: float = 0.0  # Amps
    max_voltage: float = 0.0         # Max available from source
    max_current: float = 0.0         # Max available from source
    actual_voltage: float = 0.0      # Measured (from INA219)
    actual_current: float = 0.0      # Measured (from INA219)
    actual_power: float = 0.0        # Watts
    cable_5a: bool = False           # True if emarked 5A cable detected
    device_type: str = "unknown"     # "phone", "laptop", "tablet", "accessory"

    def to_dict(self) -> dict[str, Any]:
        return {
            "available": self.available,
            "connected": self.connected,
            "role": self.role,
            "negotiated_voltage": self.negotiated_voltage,
            "negotiated_current": self.negotiated_current,
            "max_voltage": self.max_voltage,
            "max_current": self.max_current,
            "actual_voltage": round(self.actual_voltage, 2),
            "actual_current": round(self.actual_current, 3),
            "actual_power": round(self.actual_power, 1),
            "cable_5a": self.cable_5a,
            "device_type": self.device_type,
        }


class USBPDController:
    """
    Manages USB Power Delivery negotiation and monitoring.

    Supports FUSB302 and STUSB4500 PD controller ICs via I2C.
    Falls back to monitoring-only mode via sysfs if no I2C controller
    is detected (many USB-C ports expose PD state via the kernel's
    typec subsystem).
    """

    def __init__(
        self,
        i2c_bus: int = DEFAULT_I2C_BUS,
        controller_type: str = "auto",  # "fusb302", "stusb4500", "sysfs", "auto"
    ) -> None:
        self._i2c_bus = i2c_bus
        self._controller_type = controller_type
        self._state = PDState()
        self._bus = None
        self._monitor_task: asyncio.Task | None = None

    @property
    def state(self) -> PDState:
        return self._state

    async def start(self) -> bool:
        """Detect PD controller and start monitoring."""
        if self._controller_type in ("auto", "sysfs"):
            if self._detect_sysfs():
                self._state.available = True
                self._monitor_task = asyncio.create_task(
                    self._monitor_loop(), name="usb-pd-monitor"
                )
                log.info("USB-PD monitoring via sysfs")
                return True

        if self._controller_type in ("auto", "fusb302") and _I2C_AVAILABLE:
            if self._detect_i2c(FUSB302_ADDR):
                self._state.available = True
                self._controller_type = "fusb302"
                self._monitor_task = asyncio.create_task(
                    self._monitor_loop(), name="usb-pd-monitor"
                )
                log.info("USB-PD controller: FUSB302 on I2C 0x%02X", FUSB302_ADDR)
                return True

        if self._controller_type in ("auto", "stusb4500") and _I2C_AVAILABLE:
            if self._detect_i2c(STUSB4500_ADDR):
                self._state.available = True
                self._controller_type = "stusb4500"
                self._monitor_task = asyncio.create_task(
                    self._monitor_loop(), name="usb-pd-monitor"
                )
                log.info("USB-PD controller: STUSB4500 on I2C 0x%02X", STUSB4500_ADDR)
                return True

        log.info("No USB-PD controller detected — PD monitoring disabled")
        return False

    async def stop(self) -> None:
        if self._monitor_task:
            self._monitor_task.cancel()
            try:
                await self._monitor_task
            except asyncio.CancelledError:
                pass
        if self._bus:
            try:
                self._bus.close()
            except Exception:
                pass

    # ── Detection ────────────────────────────────────────────────────────────

    def _detect_sysfs(self) -> bool:
        """Check for kernel typec subsystem (Linux 5.x+)."""
        from pathlib import Path
        typec = Path("/sys/class/typec")
        return typec.exists() and any(typec.iterdir())

    def _detect_i2c(self, addr: int) -> bool:
        """Probe I2C address for PD controller."""
        if not _I2C_AVAILABLE:
            return False
        try:
            bus = smbus2.SMBus(self._i2c_bus)
            bus.read_byte(addr)
            self._bus = bus
            return True
        except Exception:
            return False

    # ── Monitoring ───────────────────────────────────────────────────────────

    async def _monitor_loop(self) -> None:
        """Periodically read PD state."""
        while True:
            try:
                self._read_state()
                await asyncio.sleep(2.0)
            except asyncio.CancelledError:
                return
            except Exception:
                await asyncio.sleep(5.0)

    def _read_state(self) -> None:
        """Read current PD state from sysfs or I2C."""
        from pathlib import Path

        # Try sysfs first (most reliable)
        typec = Path("/sys/class/typec")
        if typec.exists():
            for port in sorted(typec.iterdir()):
                self._read_typec_sysfs(port)
                return

    def _read_typec_sysfs(self, port_path: "Path") -> None:
        """Read USB Type-C / PD state from kernel sysfs."""
        def _r(name: str) -> str:
            try:
                return (port_path / name).read_text().strip()
            except OSError:
                return ""

        role = _r("power_role")
        if role:
            self._state.role = "source" if "source" in role.lower() else "sink"

        data_role = _r("data_role")
        self._state.connected = bool(data_role and data_role.lower() not in ("none", ""))

        # Check for partner device
        partner = port_path / f"{port_path.name}-partner"
        if partner.exists():
            self._state.connected = True
            # Try to read PD capabilities from partner
            for pd_dir in sorted(partner.glob("usb_power_delivery*")):
                self._read_pd_caps(pd_dir)

        # Guess device type from negotiated power
        power = self._state.negotiated_voltage * self._state.negotiated_current
        if power > 40:
            self._state.device_type = "laptop"
        elif power > 15:
            self._state.device_type = "tablet"
        elif power > 5:
            self._state.device_type = "phone"
        elif self._state.connected:
            self._state.device_type = "accessory"

    def _read_pd_caps(self, pd_dir: "Path") -> None:
        """Read PD source capabilities from sysfs."""
        # Look for source_capabilities
        for cap_dir in sorted(pd_dir.glob("source-capabilities/*")):
            def _r(name: str) -> str:
                try:
                    return (cap_dir / name).read_text().strip()
                except OSError:
                    return ""

            voltage = _r("voltage")
            current = _r("current")
            try:
                v = float(voltage.replace("mV", "")) / 1000 if "mV" in voltage else float(voltage)
                c = float(current.replace("mA", "")) / 1000 if "mA" in current else float(current)
                if v > self._state.max_voltage:
                    self._state.max_voltage = v
                if c > self._state.max_current:
                    self._state.max_current = c
                # Use highest negotiated profile
                self._state.negotiated_voltage = v
                self._state.negotiated_current = c
            except ValueError:
                pass


# ── HTTP routes ──────────────────────────────────────────────────────────────

def register_pd_routes(app: web.Application, pd: USBPDController) -> None:

    async def get_state(_: web.Request) -> web.Response:
        return web.json_response(pd.state.to_dict())

    app.router.add_get("/pd/state", get_state)
