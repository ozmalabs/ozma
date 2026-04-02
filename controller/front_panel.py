# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Controller front panel — OLED display + physical buttons.

Drives a small I2C OLED (SSD1306 128×64 or 128×32) and reads physical
buttons connected to GPIO or I2C GPIO expander. Shows:

  Line 1: Active scenario name + colour indicator
  Line 2: Active node name
  Line 3: Volume level bar + mute indicator
  Line 4: Status (node count, uptime, or alert)

Buttons (active low, directly wired or via MCP23017):
  Button 1: Next scenario (same as ScrollLock hotkey)
  Button 2: Mute toggle
  Button 3: Volume up (optional, usually controlled by fader/knob)

Hardware connection:
  OLED: I2C bus (SDA/SCL), address 0x3C (SSD1306 default)
  Buttons: GPIO pins or MCP23017 I2C GPIO expander at 0x20

Works without hardware — gracefully degrades if no I2C device found.
"""

from __future__ import annotations

import asyncio
import logging
import time
from typing import Any

log = logging.getLogger("ozma.front_panel")

# Try to import I2C/OLED libraries — optional dependencies
_HAS_OLED = False
try:
    from luma.core.interface.serial import i2c
    from luma.oled.device import ssd1306
    from luma.core.render import canvas
    from PIL import ImageFont
    _HAS_OLED = True
except ImportError:
    pass

_HAS_GPIO = False
try:
    import gpiod
    _HAS_GPIO = True
except ImportError:
    pass


class FrontPanel:
    """
    Controller front panel with OLED display and physical buttons.

    Integrates with ControlManager for button actions and with
    AppState/ScenarioManager for display content.
    """

    def __init__(self, state: Any = None, scenarios: Any = None,
                 controls: Any = None, audio: Any = None,
                 i2c_port: int = 1, oled_address: int = 0x3C,
                 button_pins: list[int] | None = None) -> None:
        self._state = state
        self._scenarios = scenarios
        self._controls = controls
        self._audio = audio
        self._i2c_port = i2c_port
        self._oled_address = oled_address
        self._button_pins = button_pins or [17, 27, 22]  # BCM pin numbers
        self._device: Any = None
        self._font: Any = None
        self._font_small: Any = None
        self._task: asyncio.Task | None = None
        self._button_task: asyncio.Task | None = None
        self._active = False

        # Display state (updated by render loop)
        self._scenario_name = ""
        self._scenario_colour = "#FFFFFF"
        self._node_name = ""
        self._volume = 0.8
        self._muted = False
        self._node_count = 0
        self._status_text = ""

    async def start(self) -> None:
        """Initialise OLED and start render + button loops."""
        if not _HAS_OLED:
            log.info("Front panel: luma.oled not installed — display disabled "
                     "(uv pip install luma.oled)")
            return

        try:
            serial = i2c(port=self._i2c_port, address=self._oled_address)
            self._device = ssd1306(serial, width=128, height=64)
            self._device.contrast(200)

            # Use default PIL font (monospace bitmap, always available)
            self._font = ImageFont.load_default()
            try:
                self._font_small = ImageFont.truetype(
                    "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf", 10)
            except Exception:
                self._font_small = self._font

            self._active = True
            self._task = asyncio.create_task(self._render_loop(), name="front-panel-render")
            log.info("Front panel OLED active on I2C port %d address 0x%02X",
                     self._i2c_port, self._oled_address)
        except Exception as e:
            log.info("Front panel: no OLED found (%s) — display disabled", e)

        # Start button polling (GPIO)
        if _HAS_GPIO:
            try:
                self._button_task = asyncio.create_task(
                    self._button_loop(), name="front-panel-buttons"
                )
                log.info("Front panel buttons active on GPIO pins %s", self._button_pins)
            except Exception as e:
                log.info("Front panel: GPIO not available (%s) — buttons disabled", e)

    async def stop(self) -> None:
        self._active = False
        if self._task:
            self._task.cancel()
        if self._button_task:
            self._button_task.cancel()
        if self._device:
            self._device.hide()

    async def _render_loop(self) -> None:
        """Update the OLED display every 200ms."""
        while self._active:
            try:
                self._update_state()
                self._render_frame()
            except Exception as e:
                log.debug("Front panel render error: %s", e)
            await asyncio.sleep(0.2)

    def _update_state(self) -> None:
        """Pull current state from managers."""
        if self._scenarios:
            active_id = self._scenarios.active_id
            if active_id:
                scenario = self._scenarios.get(active_id)
                if scenario:
                    self._scenario_name = scenario.name
                    self._scenario_colour = getattr(scenario, 'color', '#FFFFFF')
                    node_id = getattr(scenario, 'node_id', '')
                    # Shorten node name for display
                    self._node_name = node_id.split('.')[0] if node_id else ""
            else:
                self._scenario_name = "No scenario"
                self._node_name = ""

        if self._state:
            self._node_count = len(self._state.nodes)

        if self._audio and hasattr(self._audio, '_watcher') and self._audio._watcher:
            # Get volume of active node if available
            pass  # Volume display from PipeWire watcher would go here

        self._status_text = f"{self._node_count} nodes"

    def _render_frame(self) -> None:
        """Draw one frame to the OLED."""
        if not self._device:
            return

        with canvas(self._device) as draw:
            # Line 1: Scenario name (large)
            name = self._scenario_name or "Ozma"
            draw.text((0, 0), name[:16], fill="white", font=self._font)

            # Line 2: Node name
            draw.text((0, 16), self._node_name[:21], fill="white", font=self._font_small)

            # Line 3: Volume bar
            vol_label = "MUTE" if self._muted else f"Vol: {int(self._volume * 100)}%"
            draw.text((0, 30), vol_label, fill="white", font=self._font_small)
            # Volume bar (64px wide)
            bar_x = 60
            bar_w = 64
            bar_h = 8
            draw.rectangle([bar_x, 31, bar_x + bar_w, 31 + bar_h], outline="white")
            if not self._muted:
                fill_w = int(bar_w * self._volume)
                if fill_w > 0:
                    draw.rectangle([bar_x + 1, 32, bar_x + fill_w, 31 + bar_h - 1],
                                   fill="white")

            # Line 4: Status
            draw.text((0, 46), self._status_text[:21], fill="white", font=self._font_small)

            # Uptime in corner
            uptime = time.strftime("%H:%M")
            draw.text((100, 46), uptime, fill="white", font=self._font_small)

    async def _button_loop(self) -> None:
        """Poll GPIO buttons and trigger actions."""
        if not _HAS_GPIO:
            return

        loop = asyncio.get_running_loop()

        try:
            chip = gpiod.Chip('/dev/gpiochip0')
            lines = chip.get_lines(self._button_pins)
            lines.request(consumer='ozma-panel', type=gpiod.LINE_REQ_DIR_IN,
                          flags=gpiod.LINE_REQ_FLAG_BIAS_PULL_UP)

            prev_state = [1] * len(self._button_pins)

            while self._active:
                values = lines.get_values()
                for i, (prev, cur) in enumerate(zip(prev_state, values)):
                    if prev == 1 and cur == 0:  # Falling edge (button press)
                        await self._on_button(i)
                prev_state = list(values)
                await asyncio.sleep(0.05)  # 50ms debounce/poll

        except Exception as e:
            log.debug("Button polling failed: %s", e)

    async def _on_button(self, button_index: int) -> None:
        """Handle a button press."""
        match button_index:
            case 0:
                # Next scenario
                log.debug("Front panel: button 0 → next scenario")
                if self._controls:
                    await self._controls.on_control_changed("front_panel", "next_scenario", 1)
            case 1:
                # Mute toggle
                log.debug("Front panel: button 1 → mute toggle")
                self._muted = not self._muted
                if self._controls:
                    await self._controls.on_control_changed("front_panel", "mute_toggle", self._muted)
            case 2:
                # Volume up (or any custom action)
                log.debug("Front panel: button 2 → volume up")
                if self._controls:
                    await self._controls.on_control_changed("front_panel", "volume_step", 0.05)
