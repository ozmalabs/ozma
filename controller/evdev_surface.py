# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Generic evdev control surface driver for ozma.

Supports any Linux input device that exposes buttons (EV_KEY) and/or
axes (EV_ABS, EV_REL) — this covers:

  - Contour ShuttlePRO / ShuttleXpress (jog + shuttle + buttons)
  - USB foot pedals (single or multi-pedal)
  - Macro keypads (Razer Tartarus, Logitech G13, etc.)
  - USB button boxes / switch panels
  - Any other /dev/input device not caught by the gamepad driver

Config-driven: each button or axis is mapped to an ozma action in
controls.yaml.  The driver matches devices by name substring.

Example controls.yaml entry::

    surfaces:
      shuttle:
        type: evdev
        device: "ShuttlePRO"          # Substring match on device name
        grab: true                     # Exclusively grab the device
        buttons:
          260: { action: scenario.next, value: -1 }   # Button 1 → prev
          261: { action: scenario.next, value: 1 }     # Button 2 → next
          262: { action: audio.mute, target: "@active" }
        axes:
          7: { action: audio.volume, target: "@active", min: 0, max: 255 }
"""

from __future__ import annotations

import asyncio
import logging
from typing import Any

import evdev
from evdev import InputDevice, ecodes

from controls import ControlSurface, Control, ControlBinding

log = logging.getLogger("ozma.evdev_surface")


def find_evdev_device(name_pattern: str) -> InputDevice | None:
    """Find an evdev device whose name contains the given pattern (case-insensitive)."""
    pattern_lower = name_pattern.lower()
    for path in evdev.list_devices():
        try:
            dev = InputDevice(path)
            if pattern_lower in dev.name.lower():
                return dev
        except Exception:
            pass
    return None


class EvdevSurface(ControlSurface):
    """
    A generic evdev input device registered as an ozma control surface.

    Config structure::

        {
            "device": "ShuttlePRO",   # Name substring match
            "grab": true,             # Exclusive grab
            "buttons": {
                "260": {"action": "scenario.next", "value": -1},
                ...
            },
            "axes": {
                "7": {"action": "audio.volume", "target": "@active"},
                ...
            },
            "rel_axes": {
                "7": {"action": "scenario.next"},  # REL_DIAL on ShuttlePRO
                ...
            }
        }
    """

    def __init__(self, surface_id: str, config: dict) -> None:
        super().__init__(surface_id)
        self._config = config
        self._device_pattern = config.get("device", "")
        self._grab = config.get("grab", False)
        self._device: InputDevice | None = None
        self._task: asyncio.Task | None = None
        self._on_changed: Any = None

        # Parse button mappings: evdev_code → ControlBinding
        self._button_map: dict[int, tuple[str, ControlBinding]] = {}
        for code_str, binding_cfg in config.get("buttons", {}).items():
            code = int(code_str)
            name = f"btn_{code}"
            binding = ControlBinding(
                action=binding_cfg.get("action", ""),
                target=binding_cfg.get("target", ""),
                value=binding_cfg.get("value"),
            )
            self._button_map[code] = (name, binding)
            self.controls[name] = Control(name=name, surface_id=self.id, binding=binding)

        # Parse absolute axis mappings
        self._abs_map: dict[int, tuple[str, ControlBinding, int, int]] = {}
        for code_str, axis_cfg in config.get("axes", {}).items():
            code = int(code_str)
            name = f"abs_{code}"
            binding = ControlBinding(
                action=axis_cfg.get("action", ""),
                target=axis_cfg.get("target", ""),
            )
            min_val = axis_cfg.get("min", 0)
            max_val = axis_cfg.get("max", 255)
            self._abs_map[code] = (name, binding, min_val, max_val)
            self.controls[name] = Control(name=name, surface_id=self.id, binding=binding)

        # Parse relative axis mappings (jog wheels, scroll rings)
        self._rel_map: dict[int, tuple[str, ControlBinding]] = {}
        for code_str, rel_cfg in config.get("rel_axes", {}).items():
            code = int(code_str)
            name = f"rel_{code}"
            binding = ControlBinding(
                action=rel_cfg.get("action", ""),
                target=rel_cfg.get("target", ""),
            )
            self._rel_map[code] = (name, binding)
            self.controls[name] = Control(name=name, surface_id=self.id, binding=binding)

    async def start(self) -> None:
        self._device = find_evdev_device(self._device_pattern)
        if not self._device:
            log.warning("evdev surface '%s': device matching '%s' not found",
                        self.id, self._device_pattern)
            return

        if self._grab:
            try:
                self._device.grab()
            except OSError as e:
                log.warning("Could not grab %s: %s", self._device.path, e)

        # Read actual axis info for normalization
        cap = self._device.capabilities(absinfo=True)
        if ecodes.EV_ABS in cap:
            for item in cap[ecodes.EV_ABS]:
                if isinstance(item, tuple):
                    code, absinfo = item
                    if code in self._abs_map:
                        name, binding, _, _ = self._abs_map[code]
                        self._abs_map[code] = (name, binding, absinfo.min, absinfo.max)

        self._task = asyncio.create_task(
            self._read_loop(), name=f"evdev-{self._device.path}"
        )
        log.info("evdev surface '%s' started: %s (%s) — %d buttons, %d axes, %d rel",
                 self.id, self._device.name, self._device.path,
                 len(self._button_map), len(self._abs_map), len(self._rel_map))

    async def stop(self) -> None:
        if self._task:
            self._task.cancel()
            try:
                await self._task
            except asyncio.CancelledError:
                pass

    def set_on_changed(self, callback: Any) -> None:
        self._on_changed = callback

    async def _fire(self, control_name: str, value: Any) -> None:
        if self._on_changed:
            await self._on_changed(self.id, control_name, value)

    async def _read_loop(self) -> None:
        try:
            async for event in self._device.async_read_loop():
                if event.type == ecodes.EV_KEY and event.value == 1:  # press only
                    mapping = self._button_map.get(event.code)
                    if mapping:
                        name, binding = mapping
                        val = binding.value if binding.value is not None else True
                        await self._fire(name, val)

                elif event.type == ecodes.EV_ABS:
                    mapping = self._abs_map.get(event.code)
                    if mapping:
                        name, binding, min_val, max_val = mapping
                        if max_val != min_val:
                            normalized = (event.value - min_val) / (max_val - min_val)
                        else:
                            normalized = 0.0
                        await self._fire(name, normalized)

                elif event.type == ecodes.EV_REL:
                    mapping = self._rel_map.get(event.code)
                    if mapping:
                        name, binding = mapping
                        # Relative events: positive = forward/right, negative = backward/left
                        direction = 1 if event.value > 0 else -1
                        await self._fire(name, direction)

        except asyncio.CancelledError:
            raise
        except OSError:
            log.warning("evdev device disconnected: %s", self._device.name if self._device else "?")
        except Exception as e:
            log.error("evdev surface read error: %s", e)

    def to_dict(self) -> dict:
        d = super().to_dict()
        d["device"] = self._device.name if self._device else self._device_pattern
        d["device_path"] = self._device.path if self._device else None
        return d
