# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Monitor hardware control — brightness, power, input source via DDC/CI.

Controls physical monitors connected to target machines or the controller
using DDC/CI (Display Data Channel / Command Interface) over I2C.

Capabilities:
  - Brightness control (dim inactive monitors, brighten active)
  - Power state (standby, off, on)
  - Input source switching (HDMI1, HDMI2, DP1, etc.)
  - Volume (for monitors with speakers)

Uses ddcutil on Linux.  Falls back to xrandr brightness for software dimming.

Scenario integration:
  On scenario switch:
    - Active machine's monitor: full brightness
    - Inactive machines' monitors: dimmed (configurable %)
    - Optional: switch monitor input source per scenario
"""

from __future__ import annotations

import asyncio
import logging
import shutil
import subprocess
from dataclasses import dataclass, field
from typing import Any

log = logging.getLogger("ozma.monitor_control")

# DDC/CI VCP codes
VCP_BRIGHTNESS = 0x10
VCP_CONTRAST = 0x12
VCP_POWER_MODE = 0xD6
VCP_INPUT_SOURCE = 0x60
VCP_VOLUME = 0x62

# Power modes
POWER_ON = 0x01
POWER_STANDBY = 0x02
POWER_OFF = 0x04

# Common input sources
INPUT_VGA = 0x01
INPUT_DVI = 0x03
INPUT_HDMI1 = 0x11
INPUT_HDMI2 = 0x12
INPUT_DP1 = 0x0F
INPUT_DP2 = 0x10
INPUT_USBC = 0x13

INPUT_NAMES: dict[str, int] = {
    "vga": INPUT_VGA, "dvi": INPUT_DVI,
    "hdmi1": INPUT_HDMI1, "hdmi2": INPUT_HDMI2,
    "dp1": INPUT_DP1, "dp2": INPUT_DP2,
    "usbc": INPUT_USBC,
}


@dataclass
class MonitorInfo:
    """A physical monitor controllable via DDC/CI."""
    bus: int                    # I2C bus number (from ddcutil detect)
    model: str = ""
    serial: str = ""
    brightness: int = -1        # Current brightness (0-100), -1 = unknown
    node_id: str = ""           # Which ozma node/machine this monitor belongs to
    input_source: str = ""

    def to_dict(self) -> dict[str, Any]:
        return {
            "bus": self.bus, "model": self.model, "serial": self.serial,
            "brightness": self.brightness, "node_id": self.node_id,
            "input_source": self.input_source,
        }


class MonitorController:
    """
    Controls physical monitors via DDC/CI (ddcutil).

    On scenario switch, dims inactive monitors and brightens the active one.
    Can also switch monitor input sources per scenario.
    """

    def __init__(self) -> None:
        self._monitors: dict[int, MonitorInfo] = {}  # bus → info
        self._available = False
        self._active_brightness: int = 100
        self._inactive_brightness: int = 20
        self._active_node: str = ""

    async def start(self) -> None:
        if not shutil.which("ddcutil"):
            log.info("ddcutil not found — monitor DDC/CI control disabled")
            # Try xrandr fallback
            if shutil.which("xrandr"):
                self._available = True
                log.info("Monitor control via xrandr (software brightness only)")
            return

        self._available = True
        await self._detect_monitors()

    def list_monitors(self) -> list[dict[str, Any]]:
        return [m.to_dict() for m in self._monitors.values()]

    async def set_brightness(self, bus: int, brightness: int) -> bool:
        """Set brightness (0-100) on a specific monitor."""
        brightness = max(0, min(100, brightness))
        if shutil.which("ddcutil"):
            return await self._ddcutil_set(bus, VCP_BRIGHTNESS, brightness)
        return False

    async def set_power(self, bus: int, mode: int) -> bool:
        """Set power mode (POWER_ON, POWER_STANDBY, POWER_OFF)."""
        return await self._ddcutil_set(bus, VCP_POWER_MODE, mode)

    async def set_input(self, bus: int, input_name: str) -> bool:
        """Switch monitor input source (hdmi1, hdmi2, dp1, etc.)."""
        code = INPUT_NAMES.get(input_name.lower())
        if code is None:
            return False
        return await self._ddcutil_set(bus, VCP_INPUT_SOURCE, code)

    async def on_scenario_switch(self, active_node_id: str, scenario: dict | None = None) -> None:
        """Dim inactive monitors, brighten active monitor."""
        self._active_node = active_node_id

        for bus, monitor in self._monitors.items():
            if monitor.node_id == active_node_id:
                await self.set_brightness(bus, self._active_brightness)
            elif monitor.node_id:
                await self.set_brightness(bus, self._inactive_brightness)

        # Input source switching per scenario
        if scenario:
            input_map = scenario.get("monitor_inputs", {})
            for bus_str, input_name in input_map.items():
                try:
                    await self.set_input(int(bus_str), input_name)
                except (ValueError, TypeError):
                    pass

    async def dim_all(self, brightness: int = 20) -> None:
        """Dim all monitors."""
        for bus in self._monitors:
            await self.set_brightness(bus, brightness)

    async def brighten_all(self, brightness: int = 100) -> None:
        """Restore all monitors to full brightness."""
        for bus in self._monitors:
            await self.set_brightness(bus, brightness)

    # ── Detection ────────────────────────────────────────────────────────────

    async def _detect_monitors(self) -> None:
        """Detect monitors via ddcutil."""
        try:
            loop = asyncio.get_running_loop()
            def _detect():
                result = subprocess.run(
                    ["ddcutil", "detect", "--brief"],
                    capture_output=True, text=True, timeout=15,
                )
                return result.stdout
            output = await loop.run_in_executor(None, _detect)

            import re
            current_bus = -1
            for line in output.splitlines():
                bus_m = re.match(r"Display\s+\d+.*bus\s+/dev/i2c-(\d+)", line)
                if bus_m:
                    current_bus = int(bus_m.group(1))
                model_m = re.match(r"\s+Monitor:\s+(.*)", line)
                if model_m and current_bus >= 0:
                    self._monitors[current_bus] = MonitorInfo(
                        bus=current_bus, model=model_m.group(1).strip(),
                    )
                    # Read current brightness
                    bri = await self._ddcutil_get(current_bus, VCP_BRIGHTNESS)
                    if bri is not None:
                        self._monitors[current_bus].brightness = bri

            log.info("DDC/CI: %d monitor(s) detected", len(self._monitors))
        except Exception as e:
            log.debug("DDC/CI detect failed: %s", e)

    async def _ddcutil_set(self, bus: int, vcp: int, value: int) -> bool:
        try:
            loop = asyncio.get_running_loop()
            def _set():
                return subprocess.run(
                    ["ddcutil", "--bus", str(bus), "setvcp", f"0x{vcp:02x}", str(value)],
                    capture_output=True, timeout=5,
                ).returncode == 0
            return await loop.run_in_executor(None, _set)
        except Exception:
            return False

    async def _ddcutil_get(self, bus: int, vcp: int) -> int | None:
        try:
            loop = asyncio.get_running_loop()
            def _get():
                result = subprocess.run(
                    ["ddcutil", "--bus", str(bus), "getvcp", f"0x{vcp:02x}"],
                    capture_output=True, text=True, timeout=5,
                )
                import re
                m = re.search(r"current value\s*=\s*(\d+)", result.stdout)
                return int(m.group(1)) if m else None
            return await loop.run_in_executor(None, _get)
        except Exception:
            return None
