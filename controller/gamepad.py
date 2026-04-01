# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Gamepad control surface driver for ozma.

First-class support for Xbox and PlayStation controllers, plus generic
gamepads.  Uses evdev — any controller Linux recognises as a gamepad
will work.

Default mapping (configurable via controls.yaml):

  Xbox / PS        Action
  ─────────────    ────────────────────
  D-pad left/right Scenario prev/next
  LB / RB          Scenario prev/next
  A / Cross        Activate scenario
  Guide / PS       Toggle mute (active node)
  LT (analog)      Volume down (active node)
  RT (analog)      Volume up (active node)
  Left stick       (reserved — future: cursor)
  Right stick      (reserved — future: scroll)
  Start            (reserved)
  Select/Share     (reserved)

Controller type is auto-detected from the device name.
"""

from __future__ import annotations

import asyncio
import logging
from dataclasses import dataclass
from typing import Any

import evdev
from evdev import InputDevice, ecodes

from controls import ControlSurface, Control, ControlBinding

log = logging.getLogger("ozma.gamepad")

# ── Controller profiles ──────────────────────────────────────────────────────

# Standard Linux evdev button codes for gamepads.  Xbox and PlayStation
# controllers both map to these codes via the kernel HID driver.

BTN = ecodes

# Button name → evdev code.  These are the same across Xbox/PS via evdev.
GAMEPAD_BUTTONS = {
    "south":   BTN.BTN_SOUTH,    # A / Cross
    "east":    BTN.BTN_EAST,     # B / Circle
    "north":   BTN.BTN_NORTH,    # X / Triangle  (note: Xbox X = evdev NORTH)
    "west":    BTN.BTN_WEST,     # Y / Square    (note: Xbox Y = evdev WEST)
    "lb":      BTN.BTN_TL,       # LB / L1
    "rb":      BTN.BTN_TR,       # RB / R1
    "lt_btn":  BTN.BTN_TL2,      # LT digital / L2 digital (some controllers)
    "rt_btn":  BTN.BTN_TR2,      # RT digital / R2 digital (some controllers)
    "select":  BTN.BTN_SELECT,   # Back / Share / Select
    "start":   BTN.BTN_START,    # Start / Options
    "guide":   BTN.BTN_MODE,     # Xbox / PS button
    "lstick":  BTN.BTN_THUMBL,   # Left stick click
    "rstick":  BTN.BTN_THUMBR,   # Right stick click
}

# Axis name → evdev code
GAMEPAD_AXES = {
    "lx":    ecodes.ABS_X,       # Left stick X
    "ly":    ecodes.ABS_Y,       # Left stick Y
    "rx":    ecodes.ABS_RX,      # Right stick X
    "ry":    ecodes.ABS_RY,      # Right stick Y
    "lt":    ecodes.ABS_Z,       # Left trigger (analog)
    "rt":    ecodes.ABS_RZ,      # Right trigger (analog)
    "hat_x": ecodes.ABS_HAT0X,   # D-pad X (-1, 0, +1)
    "hat_y": ecodes.ABS_HAT0Y,   # D-pad Y (-1, 0, +1)
}


@dataclass
class ControllerProfile:
    """Naming and labelling for a specific controller family."""
    family: str          # "xbox", "playstation", "generic"
    variant: str         # "360", "one", "series", "ds3", "ds4", "dualsense", ""
    south_label: str     # "A" / "Cross"
    east_label: str      # "B" / "Circle"
    north_label: str     # "X" / "Triangle"
    west_label: str      # "Y" / "Square"


_PROFILES = {
    "xbox": ControllerProfile("xbox", "", "A", "B", "X", "Y"),
    "xbox360": ControllerProfile("xbox", "360", "A", "B", "X", "Y"),
    "xbox_one": ControllerProfile("xbox", "one", "A", "B", "X", "Y"),
    "xbox_series": ControllerProfile("xbox", "series", "A", "B", "X", "Y"),
    "ps3": ControllerProfile("playstation", "ds3", "Cross", "Circle", "Triangle", "Square"),
    "ps4": ControllerProfile("playstation", "ds4", "Cross", "Circle", "Triangle", "Square"),
    "ps5": ControllerProfile("playstation", "dualsense", "Cross", "Circle", "Triangle", "Square"),
    "playstation": ControllerProfile("playstation", "", "Cross", "Circle", "Triangle", "Square"),
    "generic": ControllerProfile("generic", "", "South", "East", "North", "West"),
}

# Keywords in evdev device names → profile key
_DETECT_PATTERNS: list[tuple[str, str]] = [
    ("dualsense", "ps5"),
    ("dualshock 4", "ps4"),
    ("dualshock 3", "ps3"),
    ("sony", "playstation"),
    ("playstation", "playstation"),
    ("xbox series", "xbox_series"),
    ("xbox one", "xbox_one"),
    ("xbox 360", "xbox360"),
    ("xbox", "xbox"),
    ("microsoft", "xbox"),
    ("8bitdo", "generic"),        # 8BitDo controllers
    ("switch pro", "generic"),    # Nintendo Switch Pro
    ("pro controller", "generic"),
]


def detect_profile(device_name: str) -> ControllerProfile:
    """Detect controller profile from evdev device name."""
    name_lower = device_name.lower()
    for pattern, profile_key in _DETECT_PATTERNS:
        if pattern in name_lower:
            return _PROFILES[profile_key]
    return _PROFILES["generic"]


# ── Gamepad detection ────────────────────────────────────────────────────────

def find_gamepad_devices() -> list[InputDevice]:
    """Find all connected gamepad devices via evdev."""
    devices = []
    for path in evdev.list_devices():
        try:
            dev = InputDevice(path)
            cap = dev.capabilities()
            # A gamepad has BTN_GAMEPAD (== BTN_SOUTH) and at least one analog axis
            has_gamepad_btn = (ecodes.EV_KEY in cap and
                               ecodes.BTN_SOUTH in cap.get(ecodes.EV_KEY, []))
            has_abs = ecodes.EV_ABS in cap
            if has_gamepad_btn and has_abs:
                devices.append(dev)
        except Exception:
            pass
    return devices


# ── GamepadSurface ───────────────────────────────────────────────────────────

class GamepadSurface(ControlSurface):
    """
    A gamepad registered as an ozma control surface.

    Auto-creates controls with default bindings based on the detected
    controller profile (Xbox, PlayStation, or generic).
    """

    def __init__(self, device: InputDevice, profile: ControllerProfile | None = None) -> None:
        self._device = device
        self._profile = profile or detect_profile(device.name)
        surface_id = f"gamepad-{self._profile.family}"
        if self._profile.variant:
            surface_id = f"gamepad-{self._profile.family}-{self._profile.variant}"
        super().__init__(surface_id)

        self._task: asyncio.Task | None = None
        self._on_changed: Any = None  # set by caller

        # Axis state for analog triggers
        self._axis_info: dict[int, tuple[int, int]] = {}  # code → (min, max)
        self._trigger_deadzone = 0.15
        self._last_volume_from_triggers: float | None = None

        # Build controls with default bindings
        self._build_controls()

        log.info("Gamepad detected: %s (%s %s) at %s",
                 device.name, self._profile.family, self._profile.variant, device.path)

    def _build_controls(self) -> None:
        """Create controls with default scenario/audio bindings."""
        p = self._profile

        # D-pad and bumpers: scenario cycling
        self.controls["dpad_right"] = Control(
            name="dpad_right", surface_id=self.id,
            binding=ControlBinding(action="scenario.next", value=1),
        )
        self.controls["dpad_left"] = Control(
            name="dpad_left", surface_id=self.id,
            binding=ControlBinding(action="scenario.next", value=-1),
        )
        self.controls["rb"] = Control(
            name="rb", surface_id=self.id,
            binding=ControlBinding(action="scenario.next", value=1),
        )
        self.controls["lb"] = Control(
            name="lb", surface_id=self.id,
            binding=ControlBinding(action="scenario.next", value=-1),
        )

        # South button (A / Cross): activate scenario (confirm)
        self.controls["south"] = Control(
            name=f"south ({p.south_label})", surface_id=self.id,
            binding=ControlBinding(action="scenario.activate"),
        )

        # Guide / PS button: toggle mute
        self.controls["guide"] = Control(
            name="guide", surface_id=self.id,
            binding=ControlBinding(action="audio.mute", target="@active"),
        )

        # Analog triggers: volume control
        self.controls["rt_volume"] = Control(
            name="rt_volume", surface_id=self.id,
            binding=ControlBinding(action="audio.volume", target="@active"),
        )

        # D-pad up/down: volume increment
        self.controls["dpad_up"] = Control(
            name="dpad_up", surface_id=self.id,
            binding=ControlBinding(action="audio.volume_step", target="@active", value=0.05),
        )
        self.controls["dpad_down"] = Control(
            name="dpad_down", surface_id=self.id,
            binding=ControlBinding(action="audio.volume_step", target="@active", value=-0.05),
        )

    async def start(self) -> None:
        # Read axis calibration info
        cap = self._device.capabilities(absinfo=True)
        if ecodes.EV_ABS in cap:
            for item in cap[ecodes.EV_ABS]:
                if isinstance(item, tuple):
                    code, absinfo = item
                    self._axis_info[code] = (absinfo.min, absinfo.max)

        self._task = asyncio.create_task(self._read_loop(), name=f"gamepad-{self._device.path}")

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
        """Fire a control change through the ControlManager."""
        if self._on_changed:
            await self._on_changed(self.id, control_name, value)

    def _normalize_axis(self, code: int, raw: int) -> float:
        """Normalize axis value to 0.0-1.0 (triggers) or -1.0 to 1.0 (sticks)."""
        min_val, max_val = self._axis_info.get(code, (0, 255))
        if max_val == min_val:
            return 0.0
        return (raw - min_val) / (max_val - min_val)

    async def _read_loop(self) -> None:
        """Main event loop — reads evdev events from the gamepad."""
        try:
            async for event in self._device.async_read_loop():
                if event.type == ecodes.EV_KEY:
                    await self._handle_button(event.code, event.value == 1)
                elif event.type == ecodes.EV_ABS:
                    await self._handle_axis(event.code, event.value)
        except asyncio.CancelledError:
            raise
        except OSError:
            log.warning("Gamepad disconnected: %s", self._device.name)
        except Exception as e:
            log.error("Gamepad read error: %s", e)

    async def _handle_button(self, code: int, pressed: bool) -> None:
        """Handle a button press/release."""
        if not pressed:
            return  # Only act on press, not release

        match code:
            case ecodes.BTN_SOUTH:
                await self._fire("south", True)
            case ecodes.BTN_EAST:
                pass  # B / Circle — reserved
            case ecodes.BTN_NORTH:
                pass  # X / Triangle — reserved
            case ecodes.BTN_WEST:
                pass  # Y / Square — reserved
            case ecodes.BTN_TL:
                await self._fire("lb", True)
            case ecodes.BTN_TR:
                await self._fire("rb", True)
            case ecodes.BTN_MODE:
                await self._fire("guide", True)
            case ecodes.BTN_SELECT:
                pass  # reserved
            case ecodes.BTN_START:
                pass  # reserved
            case ecodes.BTN_THUMBL:
                pass  # reserved
            case ecodes.BTN_THUMBR:
                pass  # reserved

    async def _handle_axis(self, code: int, raw_value: int) -> None:
        """Handle an axis event (sticks, triggers, d-pad)."""
        match code:
            # D-pad
            case ecodes.ABS_HAT0X:
                if raw_value == 1:
                    await self._fire("dpad_right", True)
                elif raw_value == -1:
                    await self._fire("dpad_left", True)
            case ecodes.ABS_HAT0Y:
                if raw_value == -1:
                    await self._fire("dpad_up", True)
                elif raw_value == 1:
                    await self._fire("dpad_down", True)

            # Right trigger → volume (analog)
            case ecodes.ABS_RZ:
                val = self._normalize_axis(code, raw_value)
                if val > self._trigger_deadzone:
                    await self._fire("rt_volume", val)

            # Left trigger — reserved for future use
            case ecodes.ABS_Z:
                pass

            # Sticks — reserved
            case ecodes.ABS_X | ecodes.ABS_Y | ecodes.ABS_RX | ecodes.ABS_RY:
                pass

    def to_dict(self) -> dict:
        d = super().to_dict()
        d["device"] = self._device.name
        d["profile"] = {
            "family": self._profile.family,
            "variant": self._profile.variant,
        }
        return d
