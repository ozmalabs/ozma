# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Virtual input devices per session.

Provides per-session uinput/uhid devices for gamepad and other input devices.

Features:
  - Per-session uinput devices
  - Gamepad hotplug simulation in containers
  - Session teardown cleans up uinput devices
"""

from __future__ import annotations

import asyncio
import logging
import os
import struct
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.controller.gaming.virtual_input")

try:
    import uinput
    UINPUT_AVAILABLE = True
except ImportError:
    UINPUT_AVAILABLE = False

# ─── Constants ───────────────────────────────────────────────────────────────

UINPUT_DIR = Path("/dev/input")
GAMEPAD_NAME = "Ozma Gamepad"
MOUSE_NAME = "Ozma Mouse"
KEYBOARD_NAME = "Ozma Keyboard"

# Gamepad capabilities (Xbox-style)
GAMEPAD_CAPS = {
    uinput.BTN_A: (0, 1, 0, 0),
    uinput.BTN_B: (0, 1, 0, 0),
    uinput.BTN_X: (0, 1, 0, 0),
    uinput.BTN_Y: (0, 1, 0, 0),
    uinput.BTN_LB: (0, 1, 0, 0),
    uinput.BTN_RB: (0, 1, 0, 0),
    uinput.BTN_SELECT: (0, 1, 0, 0),
    uinput.BTN_START: (0, 1, 0, 0),
    uinput.BTN_THUMBL: (0, 1, 0, 0),
    uinput.BTN_THUMBR: (0, 1, 0, 0),
    uinput.ABS_X: (-32768, 32767, 0, 0),
    uinput.ABS_Y: (-32768, 32767, 0, 0),
    uinput.ABS_RX: (-32768, 32767, 0, 0),
    uinput.ABS_RY: (-32768, 32767, 0, 0),
    uinput.ABS_Z: (0, 1023, 0, 0),
    uinput.ABS_RZ: (0, 1023, 0, 0),
}

# ─── Virtual Gamepad ─────────────────────────────────────────────────────────

class VirtualGamepad:
    """A virtual uinput gamepad device."""

    def __init__(self, session_id: str, device_path: str | None = None):
        self.session_id = session_id
        self.device_path = device_path or f"/dev/uinput/{session_id}"
        self._device: uinput.Device | None = None
        self._enabled = False
        self._created_at = time.time()

    def create(self) -> bool:
        """Create the virtual gamepad device."""
        if not UINPUT_AVAILABLE:
            log.warning("uinput not available - virtual gamepad disabled")
            return False

        try:
            self._device = uinput.Device(
                GAMEPAD_CAPS,
                name=f"Ozma Gamepad {self.session_id[:8]}",
            )
            self._device.create()
            self._enabled = True
            log.info("Created virtual gamepad for session %s", self.session_id)
            return True
        except Exception as e:
            log.error("Failed to create virtual gamepad: %s", e)
            return False

    def destroy(self) -> None:
        """Destroy the virtual gamepad device."""
        if self._device:
            try:
                self._device.destroy()
            except Exception:
                pass
            self._device = None
        self._enabled = False
        log.info("Destroyed virtual gamepad for session %s", self.session_id)

    def send_event(self, event_type: int, code: int, value: int) -> bool:
        """Send an input event."""
        if not self._enabled or not self._device:
            return False

        try:
            self._device.emit(event_type, code, value)
            return True
        except Exception as e:
            log.error("Failed to send event: %s", e)
            return False

    def emit(self, event_type: int, code: int, value: int) -> None:
        """Emit an event (non-blocking wrapper)."""
        asyncio.create_task(self.send_event_async(event_type, code, value))

    async def send_event_async(self, event_type: int, code: int, value: int) -> bool:
        """Send an event asynchronously."""
        return await asyncio.to_thread(self.send_event, event_type, code, value)

    def emit_btn(self, button: int, pressed: bool) -> None:
        """Emit a button event."""
        self.emit(uinput.BTN_MOUSE if button < 3 else uinput.BTN_GAMEPAD, button, 1 if pressed else 0)

    def emit_abs(self, axis: int, value: int) -> None:
        """Emit an absolute axis event."""
        self.emit(uinput.ABS_X + axis, value, 0)

    def emit_scroll(self, value: int) -> None:
        """Emit a scroll event."""
        self.emit(uinput.REL_WHEEL, value, 0)

    def reset(self) -> None:
        """Reset all buttons and axes."""
        for btn in [uinput.BTN_A, uinput.BTN_B, uinput.BTN_X, uinput.BTN_Y,
                    uinput.BTN_LB, uinput.BTN_RB, uinput.BTN_SELECT, uinput.BTN_START]:
            self.send_event(uinput.BTN_GAMEPAD, btn, 0)
        for axis in range(6):
            self.send_event(uinput.ABS_X + axis, 0, 0)


# ─── Virtual Input Manager ───────────────────────────────────────────────────

class VirtualInputManager:
    """
    Manages virtual input devices per session.

    Features:
      - Per-session uinput devices
      - Gamepad hotplug simulation (fake-udev pattern)
      - Session teardown cleans up devices
    """

    def __init__(self, data_dir: Path = Path("/var/lib/ozma/gaming")):
        self._data_dir = data_dir
        self._data_dir.mkdir(parents=True, exist_ok=True)
        self._sessions: dict[str, VirtualGamepad] = {}
        self._tasks: list[asyncio.Task] = []

    async def start(self) -> None:
        """Start the virtual input manager."""
        log.info("VirtualInputManager started")

    async def stop(self) -> None:
        """Stop the virtual input manager."""
        for task in self._tasks:
            task.cancel()
        self._tasks.clear()

        # Destroy all devices
        for session_id in list(self._sessions.keys()):
            await self.destroy_session(session_id)

        log.info("VirtualInputManager stopped")

    async def create_session(self, session_id: str) -> VirtualGamepad | None:
        """Create a virtual input device for a session."""
        if session_id in self._sessions:
            return self._sessions[session_id]

        gamepad = VirtualGamepad(session_id)
        if not gamepad.create():
            return None

        self._sessions[session_id] = gamepad
        log.info("Created virtual input for session %s", session_id)
        return gamepad

    async def destroy_session(self, session_id: str) -> bool:
        """Destroy virtual input devices for a session."""
        if session_id not in self._sessions:
            return False

        gamepad = self._sessions.pop(session_id)
        gamepad.destroy()
        log.info("Destroyed virtual input for session %s", session_id)
        return True

    def get_session_device(self, session_id: str) -> VirtualGamepad | None:
        """Get the virtual device for a session."""
        return self._sessions.get(session_id)

    def get_all_sessions(self) -> list[str]:
        """Get all active session IDs."""
        return list(self._sessions.keys())

    # ─── Fake-udev hotplug simulation ────────────────────────────────────────

    async def simulate_hotplug(self, session_id: str, connected: bool) -> None:
        """Simulate gamepad hotplug event."""
        gamepad = self._sessions.get(session_id)
        if not gamepad:
            return

        # Send hotplug event
        if connected:
            # In a real implementation, this would write to udev
            # For now, just log
            log.info("Hotplug: gamepad connected for session %s", session_id)
        else:
            log.info("Hotplug: gamepad disconnected for session %s", session_id)


# ─── Input Forwarder ─────────────────────────────────────────────────────────

class InputForwarder:
    """
    Forwards Moonlight input to virtual uinput devices.

    Receives input from Moonlight protocol and forwards to virtual devices.
    """

    def __init__(self, virtual_input: VirtualInputManager):
        self._virtual = virtual_input
        self._last_positions: dict[str, tuple[int, int]] = {}

    async def forward_gamepad(self, session_id: str, buttons: int, axes: dict[str, int]) -> bool:
        """Forward gamepad input to virtual device."""
        gamepad = self._virtual.get_session_device(session_id)
        if not gamepad:
            return False

        # Update buttons
        for bit in range(16):
            mask = 1 << bit
            pressed = bool(buttons & mask)
            if pressed:
                gamepad.emit_btn(bit, True)
            else:
                gamepad.emit_btn(bit, False)

        # Update axes
        gamepad.emit_abs(0, axes.get("x", 0))   # Left stick X
        gamepad.emit_abs(1, axes.get("y", 0))   # Left stick Y
        gamepad.emit_abs(2, axes.get("rx", 0))  # Right stick X
        gamepad.emit_abs(3, axes.get("ry", 0))  # Right stick Y
        gamepad.emit_abs(4, axes.get("z", 0))   # Left trigger
        gamepad.emit_abs(5, axes.get("rz", 0))  # Right trigger

        return True

    async def forward_mouse(self, session_id: str, x: int, y: int, buttons: int) -> bool:
        """Forward mouse input to virtual device."""
        gamepad = self._virtual.get_session_device(session_id)
        if not gamepad:
            return False

        # Update mouse buttons
        gamepad.emit_btn(0, bool(buttons & 0x01))  # Left
        gamepad.emit_btn(1, bool(buttons & 0x02))  # Right
        gamepad.emit_btn(2, bool(buttons & 0x04))  # Middle

        # Update position
        self._last_positions[session_id] = (x, y)

        return True

    async def forward_keyboard(self, session_id: str, keys: list[int], pressed: bool) -> bool:
        """Forward keyboard input to virtual device."""
        gamepad = self._virtual.get_session_device(session_id)
        if not gamepad:
            return False

        # Map keys to button events (simplified)
        for key in keys:
            gamepad.emit_btn(key % 16, pressed)

        return True
