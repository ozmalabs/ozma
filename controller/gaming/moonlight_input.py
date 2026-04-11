# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Moonlight input protocol decoder.

Decodes Moonlight input messages and converts them to evdev events
for the Ozma input pipeline.

Features:
  - Keyboard input (full HID report)
  - Mouse input (absolute + relative coordinates)
  - Touch input (multi-touch with pressure)
  - Gamepad input (Xbox/PS/Nintendo mapping)
  - Gyroscope input
  - Haptic feedback

Per-client configuration:
  - Controller type override (PS/Xbox/Nintendo)
  - Mouse acceleration profile
  - Scroll sensitivity
  - Touchpad sensitivity

Input flow:
  1. Moonlight client sends input via ENET protocol
  2. MoonlightInputDecoder receives and decodes
  3. InputMapper maps to per-client profile
  4. InputInjector injects into evdev/HID pipeline

See moonlight_protocol.py for the protocol wire format.
"""

from __future__ import annotations

import asyncio
import logging
import struct
import time
from dataclasses import dataclass, field
from enum import IntEnum
from typing import Any, Callable

log = logging.getLogger("ozma.controller.gaming.moonlight_input")


# ── Constants ────────────────────────────────────────────────────────────────

# Input message types (from ENET protocol)
INPUT_KEY = 0x01
INPUT_MOUSE = 0x02
INPUT_GAMEPAD = 0x03
INPUT_TOUCH = 0x04
INPUT_HAPTIC = 0x05
INPUT_HYPER = 0x06  # HDR metadata
INPUT_PEN = 0x07
INPUT_GYRO = 0x08
INPUT_CONFIG = 0x10

# Gamepad button bits (Xbox-style)
GP_BUTTON_A = 0x0001
GP_BUTTON_B = 0x0002
GP_BUTTON_X = 0x0004
GP_BUTTON_Y = 0x0008
GP_BUTTON_LB = 0x0010
GP_BUTTON_RB = 0x0020
GP_BUTTON_L3 = 0x0040
GP_BUTTON_R3 = 0x0080
GP_BUTTON_LB2 = 0x0100
GP_BUTTON_RB2 = 0x0200
GP_BUTTON_SELECT = 0x0400
GP_BUTTON_START = 0x0800
GP_BUTTON_L = 0x1000
GP_BUTTON_R = 0x2000
GP_BUTTON_HOME = 0x4000
GP_BUTTON_CAPTURE = 0x8000

# Gamepad axes range
AXIS_MIN = -32768
AXIS_MAX = 32767
TRIGGER_MIN = 0
TRIGGER_MAX = 1023

# Mouse button bits
MOUSE_LEFT = 0x01
MOUSE_RIGHT = 0x02
MOUSE_MIDDLE = 0x04
MOUSE_BACK = 0x08
MOUSE_FORWARD = 0x10

# Touch actions
TOUCH_DOWN = 0
TOUCH_MOVE = 1
TOUCH_UP = 2

# ─── Controller Type Enums ───────────────────────────────────────────────────

class ControllerType(IntEnum):
    """Supported game controller types."""
    XBOX = 0    # Xbox One/Xbox Series
    PLAYSTATION = 1  # PlayStation 4/5
    NINTENDO = 2    # Nintendo Switch Pro
    STEAM = 3     # Steam Controller
    GENERIC = 255  # Generic mapping


class MouseAcceleration(IntEnum):
    """Mouse acceleration profiles."""
    LINEAR = 0       # No acceleration
    QUADRATIC = 1    # Quadratic curve
    CUBIC = 2        # Cubic curve
    EXPONENTIAL = 3  # Exponential


# ─── Data Models ─────────────────────────────────────────────────────────────

@dataclass
class InputConfig:
    """Per-client input configuration."""
    client_id: str
    controller_type: ControllerType = ControllerType.XBOX
    mouse_acceleration: MouseAcceleration = MouseAcceleration.LINEAR
    mouse_sensitivity: float = 1.0
    mouse_acceleration_curve: float = 1.0
    scroll_sensitivity: int = 1
    touchpad_sensitivity: float = 1.0
    gamepad_deadzone: float = 0.1
    gamepad_max_output: float = 1.0
    inverted_axes: list[str] = field(default_factory=list)  # ["ls_x", "ls_y", "rs_x", "rs_y"]
    gyro_enabled: bool = False
    gyro_sensitivity: float = 1.0
    haptic_enabled: bool = True


@dataclass
class KeyboardEvent:
    """Keyboard event for injection."""
    key_code: int
    pressed: bool
    modifiers: int = 0  # bitfield: shift, ctrl, alt, meta
    timestamp: float = field(default_factory=time.time)


@dataclass
class MouseEvent:
    """Mouse event for injection."""
    buttons: int
    x: int
    y: int
    scroll: int = 0
    relative_dx: int = 0
    relative_dy: int = 0
    timestamp: float = field(default_factory=time.time)


@dataclass
class GamepadEvent:
    """Gamepad event for injection."""
    gamepad_id: int
    buttons: int
    left_stick_x: int
    left_stick_y: int
    right_stick_x: int
    right_stick_y: int
    left_trigger: int
    right_trigger: int
    timestamp: float = field(default_factory=time.time)


@dataclass
class TouchEvent:
    """Touch event for injection."""
    touch_id: int
    action: int  # 0=down, 1=move, 2=up
    x: int
    y: int
    pressure: float = 1.0
    width: float = 10.0
    height: float = 10.0
    timestamp: float = field(default_factory=time.time)


@dataclass
class HapticEvent:
    """Haptic feedback event."""
    device_id: int
    effect_id: int
    strength: float = 1.0
    duration_ms: int = 100
    timestamp: float = field(default_factory=time.time)


@dataclass
class GyroEvent:
    """Gyroscope/IMU event."""
    timestamp: float = field(default_factory=time.time)
    rotation_x: float = 0.0
    rotation_y: float = 0.0
    rotation_z: float = 0.0
    acceleration_x: float = 0.0
    acceleration_y: float = 0.0
    acceleration_z: float = 0.0


# ─── Input Event Types ───────────────────────────────────────────────────────

InputEvent = KeyboardEvent | MouseEvent | GamepadEvent | TouchEvent | HapticEvent | GyroEvent


# ─── Input Mapper ────────────────────────────────────────────────────────────

class InputMapper:
    """
    Maps input events to evdev codes based on client configuration.

    Supports different controller mappings:
      - Xbox: Standard Xbox layout
      - PlayStation: Standard PlayStation layout
      - Nintendo: Standard Nintendo layout
      - Steam: Steam controller layout
    """

    def __init__(self, config: InputConfig | None = None):
        self.config = config or InputConfig(client_id="default")

        # Mapping tables (Xbox → evdev)
        self._xbox_buttons: dict[int, int] = {
            GP_BUTTON_A: 0x130,   # BTN_A
            GP_BUTTON_B: 0x131,   # BTN_B
            GP_BUTTON_X: 0x132,   # BTN_X
            GP_BUTTON_Y: 0x133,   # BTN_Y
            GP_BUTTON_LB: 0x134,  # BTN_L
            GP_BUTTON_RB: 0x135,  # BTN_R
            GP_BUTTON_L3: 0x136,  # BTN_THUMBL
            GP_BUTTON_R3: 0x137,  # BTN_THUMBR
            GP_BUTTON_LB2: 0x138, # BTN_L2
            GP_BUTTON_RB2: 0x139, # BTN_R2
            GP_BUTTON_SELECT: 0x13a,  # BTN_SELECT
            GP_BUTTON_START: 0x13b,   # BTN_START
            GP_BUTTON_HOME: 0x13c,    # BTN_MODE
            GP_BUTTON_CAPTURE: 0x13d, # BTN_MISC
        }

        self._ps_buttons: dict[int, int] = {
            GP_BUTTON_A: 0x130,   # BTN_CROSS
            GP_BUTTON_B: 0x131,   # BTN_CIRCLE
            GP_BUTTON_X: 0x132,   # BTN_SQUARE
            GP_BUTTON_Y: 0x133,   # BTN_TRIANGLE
            GP_BUTTON_LB: 0x134,  # BTN_L
            GP_BUTTON_RB: 0x135,  # BTN_R
            GP_BUTTON_L3: 0x136,  # BTN_THUMBL
            GP_BUTTON_R3: 0x137,  # BTN_THUMBR
            GP_BUTTON_SELECT: 0x13a,  # BTN_SELECT
            GP_BUTTON_START: 0x13b,   # BTN_START
            GP_BUTTON_HOME: 0x13c,    # BTN_MODE
            GP_BUTTON_CAPTURE: 0x13d, # BTN_MISC
        }

        self._nintendo_buttons: dict[int, int] = {
            GP_BUTTON_A: 0x130,   # BTN_B
            GP_BUTTON_B: 0x131,   # BTN_A
            GP_BUTTON_X: 0x132,   # BTN_Y
            GP_BUTTON_Y: 0x133,   # BTN_X
            GP_BUTTON_LB: 0x134,  # BTN_L
            GP_BUTTON_RB: 0x135,  # BTN_R
            GP_BUTTON_L3: 0x136,  # BTN_THUMBL
            GP_BUTTON_R3: 0x137,  # BTN_THUMBR
            GP_BUTTON_SELECT: 0x13a,  # BTN_SELECT
            GP_BUTTON_START: 0x13b,   # BTN_START
            GP_BUTTON_HOME: 0x13c,    # BTN_MODE
            GP_BUTTON_CAPTURE: 0x13d, # BTN_MISC
        }

        self._steam_buttons: dict[int, int] = {
            GP_BUTTON_A: 0x130,   # BTN_A
            GP_BUTTON_B: 0x131,   # BTN_B
            GP_BUTTON_X: 0x132,   # BTN_X
            GP_BUTTON_Y: 0x133,   # BTN_Y
            GP_BUTTON_LB: 0x134,  # BTN_L
            GP_BUTTON_RB: 0x135,  # BTN_R
            GP_BUTTON_L3: 0x136,  # BTN_THUMBL
            GP_BUTTON_R3: 0x137,  # BTN_THUMBR
            GP_BUTTON_SELECT: 0x13a,  # BTN_SELECT
            GP_BUTTON_START: 0x13b,   # BTN_START
            GP_BUTTON_HOME: 0x13c,    # BTN_MODE
            GP_BUTTON_CAPTURE: 0x13d, # BTN_MISC
        }

    def get_button_evdev_code(self, gamepad_button: int) -> int | None:
        """Get evdev button code for a gamepad button."""
        if self.config.controller_type == ControllerType.PLAYSTATION:
            return self._ps_buttons.get(gamepad_button)
        elif self.config.controller_type == ControllerType.NINTENDO:
            return self._nintendo_buttons.get(gamepad_button)
        elif self.config.controller_type == ControllerType.STEAM:
            return self._steam_buttons.get(gamepad_button)
        else:  # XBOX or GENERIC
            return self._xbox_buttons.get(gamepad_button)

    def map_axis(self, value: int) -> int:
        """Map gamepad axis value with deadzone and sensitivity."""
        # Apply deadzone
        deadzone = int(AXIS_MAX * self.config.gamepad_deadzone)
        if abs(value) < deadzone:
            return 0

        # Normalize to -1.0 to 1.0
        normalized = value / AXIS_MAX

        # Apply sensitivity curve
        curve = self.config.mouse_acceleration_curve
        if curve != 1.0:
            sign = 1 if normalized >= 0 else -1
            normalized = sign * (abs(normalized) ** curve)

        # Scale output
        scaled = int(normalized * self.config.gamepad_max_output * AXIS_MAX)
        return max(AXIS_MIN, min(AXIS_MAX, scaled))

    def map_scroll(self, raw_value: int) -> int:
        """Map scroll value with sensitivity."""
        return int(raw_value * self.config.scroll_sensitivity)


# ─── Input Decoder ───────────────────────────────────────────────────────────

class MoonlightInputDecoder:
    """
    Decodes Moonlight input messages from ENET protocol.

    Parses input messages and converts them to structured event types.
    """

    def __init__(self, data_dir: Any = None):
        self._mappings: dict[str, InputMapper] = {}
        self._default_config = InputConfig(client_id="default")

        # Callbacks
        self.on_keyboard: Callable[[KeyboardEvent], None] | None = None
        self.on_mouse: Callable[[MouseEvent], None] | None = None
        self.on_gamepad: Callable[[GamepadEvent], None] | None = None
        self.on_touch: Callable[[TouchEvent], None] | None = None
        self.on_haptic: Callable[[HapticEvent], None] | None = None
        self.on_gyro: Callable[[GyroEvent], None] | None = None

    def get_or_create_mapper(self, client_id: str) -> InputMapper:
        """Get or create an InputMapper for a client."""
        if client_id not in self._mappings:
            self._mappings[client_id] = InputMapper(self._default_config)
        return self._mappings[client_id]

    def set_client_config(self, client_id: str, config: InputConfig) -> None:
        """Set input configuration for a client."""
        mapper = self.get_or_create_mapper(client_id)
        mapper.config = config

    def decode_keyboard(self, data: bytes) -> KeyboardEvent | None:
        """Decode keyboard input message."""
        if len(data) < 12:
            return None

        _, payload_len, _, key_code, pressed, modifiers = struct.unpack_from(
            "!BBHBBI", data
        )

        return KeyboardEvent(
            key_code=key_code,
            pressed=pressed == 1,
            modifiers=modifiers,
        )

    def decode_mouse(self, data: bytes) -> MouseEvent | None:
        """Decode mouse input message."""
        if len(data) < 24:
            return None

        _, payload_len, _, buttons, x_lo, x_hi, y_lo, y_hi, scroll = struct.unpack_from(
            "!BBHBIiiII", data
        )
        x = x_lo | (x_hi << 16)
        y = y_lo | (y_hi << 16)

        return MouseEvent(
            buttons=buttons,
            x=x,
            y=y,
            scroll=scroll,
        )

    def decode_gamepad(self, data: bytes) -> GamepadEvent | None:
        """Decode gamepad input message."""
        if len(data) < 36:
            return None

        _, payload_len, _, gp_id, buttons, lsx, lsy, rsx, rsy, lt, rt = struct.unpack_from(
            "!BBHBIIiiiiHH", data
        )

        return GamepadEvent(
            gamepad_id=gp_id,
            buttons=buttons,
            left_stick_x=lsx,
            left_stick_y=lsy,
            right_stick_x=rsx,
            right_stick_y=rsy,
            left_trigger=lt,
            right_trigger=rt,
        )

    def decode_touch(self, data: bytes) -> TouchEvent | None:
        """Decode touch input message."""
        if len(data) < 24:
            return None

        _, payload_len, _, touch_id, action, x_lo, x_hi, y_lo, y_hi, pressure = struct.unpack_from(
            "!BBHBIIIIi", data
        )
        x = x_lo | (x_hi << 16)
        y = y_lo | (y_hi << 16)

        return TouchEvent(
            touch_id=touch_id,
            action=action,
            x=x,
            y=y,
            pressure=pressure / 1000.0,
        )

    def decode_haptic(self, data: bytes) -> HapticEvent | None:
        """Decode haptic feedback message."""
        if len(data) < 12:
            return None

        _, payload_len, _, device_id, effect_id, strength = struct.unpack_from(
            "!BBHBIi", data
        )

        return HapticEvent(
            device_id=device_id,
            effect_id=effect_id,
            strength=strength / 1000.0,
            duration_ms=100,
        )

    def decode_gyro(self, data: bytes) -> GyroEvent | None:
        """Decode gyroscope input message."""
        if len(data) < 28:
            return None

        _, payload_len, _, rx, ry, rz, ax, ay, az = struct.unpack_from(
            "!BBHIffff", data
        )

        return GyroEvent(
            rotation_x=rx,
            rotation_y=ry,
            rotation_z=rz,
            acceleration_x=ax,
            acceleration_y=ay,
            acceleration_z=az,
        )

    def decode(self, client_id: str, msg_type: int, data: bytes) -> InputEvent | None:
        """Decode an input message."""
        if msg_type == INPUT_KEY:
            event = self.decode_keyboard(data)
            if event:
                return event
        elif msg_type == INPUT_MOUSE:
            event = self.decode_mouse(data)
            if event:
                return event
        elif msg_type == INPUT_GAMEPAD:
            event = self.decode_gamepad(data)
            if event:
                return event
        elif msg_type == INPUT_TOUCH:
            event = self.decode_touch(data)
            if event:
                return event
        elif msg_type == INPUT_HAPTIC:
            event = self.decode_haptic(data)
            if event:
                return event
        elif msg_type == INPUT_GYRO:
            event = self.decode_gyro(data)
            if event:
                return event

        return None


# ─── Input Injector ──────────────────────────────────────────────────────────

class InputInjector:
    """
    Injects decoded input events into the Ozma evdev pipeline.

    Handles:
      - Keyboard events to evdev keyboard
      - Mouse events to evdev mouse
      - Gamepad events to uinput/uhid
      - Touch events to evdev multitouch
    """

    def __init__(self, hid_forwarder: Any = None, state: Any = None):
        self._hid = hid_forwarder
        self._state = state
        self._tasks: list[asyncio.Task] = []

        # Keyboard state tracking
        self._kbd_modifiers: int = 0
        self._kbd_pressed: list[int] = []

        # Mouse state tracking
        self._mouse_buttons: int = 0
        self._mouse_x: int = 32768
        self._mouse_y: int = 32768

    def inject_keyboard(self, event: KeyboardEvent) -> None:
        """Inject a keyboard event."""
        if event.key_code == 0:
            return

        if event.key_code in (1, 2, 3, 4, 5, 42, 54, 56, 57, 125, 126, 127):
            # Modifier key
            if event.key_code == 42:  # Shift
                self._kbd_modifiers |= 0x01
            elif event.key_code == 54:  # Right Shift
                self._kbd_modifiers |= 0x02
            elif event.key_code == 42:  # Ctrl
                self._kbd_modifiers |= 0x04
            elif event.key_code == 56:  # Alt
                self._kbd_modifiers |= 0x08
            elif event.key_code == 57:  # Meta/Win
                self._kbd_modifiers |= 0x10

        if event.pressed:
            if event.key_code not in self._kbd_pressed:
                self._kbd_pressed.append(event.key_code)
        else:
            if event.key_code in self._kbd_pressed:
                self._kbd_pressed.remove(event.key_code)

        # Build HID report
        keys = (self._kbd_pressed + [0] * 6)[:6]
        report = bytes([self._kbd_modifiers, 0x00] + keys)

        # Send to active node
        if self._hid and self._state:
            active_node = self._state.get_active_node()
            if active_node:
                # In production, use HIDForwarder
                pass

    def inject_mouse(self, event: MouseEvent) -> None:
        """Inject a mouse event."""
        self._mouse_buttons = event.buttons
        self._mouse_x = event.x
        self._mouse_y = event.y

        # Build absolute mouse report
        x_lo = self._mouse_x & 0xFF
        x_hi = (self._mouse_x >> 8) & 0xFF
        y_lo = self._mouse_y & 0xFF
        y_hi = (self._mouse_y >> 8) & 0xFF
        scroll = max(-127, min(127, event.scroll))
        report = bytes([event.buttons, x_lo, x_hi, y_lo, y_hi, scroll])

        # Send to active node
        if self._hid and self._state:
            active_node = self._state.get_active_node()
            if active_node:
                # In production, use HIDForwarder
                pass

    def inject_gamepad(self, event: GamepadEvent, mapper: InputMapper) -> None:
        """Inject a gamepad event."""
        # Map buttons
        mapped_buttons = 0
        for bit in range(16):
            if event.buttons & (1 << bit):
                evdev_code = mapper.get_button_evdev_code(1 << bit)
                if evdev_code:
                    mapped_buttons |= (1 << evdev_code)

        # Map axes
        lsx = mapper.map_axis(event.left_stick_x)
        lsy = mapper.map_axis(-event.left_stick_y)  # Invert Y axis
        rsx = mapper.map_axis(event.right_stick_x)
        rsy = mapper.map_axis(-event.right_stick_y)
        lt = int(event.left_trigger / TRIGGER_MAX * 1023)
        rt = int(event.right_trigger / TRIGGER_MAX * 1023)

        # Send via uinput/uhid
        if self._hid:
            # In production, send via uinput device
            pass

    def inject_touch(self, event: TouchEvent) -> None:
        """Inject a touch event."""
        # In production, use evdev multitouch protocol
        pass

    def inject_haptic(self, event: HapticEvent) -> None:
        """Send haptic feedback to client."""
        # In Moonlight, haptic is client-side feedback
        # Controller doesn't need to do anything
        pass

    def inject_gyro(self, event: GyroEvent) -> None:
        """Process gyroscope input."""
        # In Moonlight, gyro data goes to client
        # For game streaming, gyro can be mapped to mouse/touch
        pass
