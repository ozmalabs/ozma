# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
HID input capture and UDP forwarding.

Reads events from evdev keyboard and mouse devices, translates them into
the tinynode wire format, and sends UDP packets to the active node.

Wire format (matches tinynode/node/listener.py):
  Keyboard: 0x01 + 8-byte HID boot report
    [modifier, 0x00, key1, key2, key3, key4, key5, key6]
  Mouse:     0x02 + 6-byte absolute report
    [buttons, x_lo, x_hi, y_lo, y_hi, scroll]
    X/Y range 0–32767 (absolute, maps to screen resolution)
"""

import asyncio
import logging
import socket
import struct
from pathlib import Path
from typing import TYPE_CHECKING, Iterator

import evdev
from evdev import InputDevice, categorize, ecodes

from keycodes import KEYCODE_TO_HID, KEYCODE_TO_X11, MODIFIER_BITS
from state import AppState
from config import Config
from transport import PKT_KEYBOARD, PKT_MOUSE, PKT_MOUSE_REL

if TYPE_CHECKING:
    from stream import StreamManager
    from controls import ControlManager
    from session import SessionManager

log = logging.getLogger("ozma.hid")

# HID boot report constants
MAX_KEYS = 6
ABSOLUTE_MAX = 32767


VIRTUAL_DEVICE_PREFIX = "ozma-virtual-"


def find_keyboard_devices(virtual_only: bool = False) -> list[InputDevice]:
    devices = []
    for path in evdev.list_devices():
        try:
            dev = InputDevice(path)
            if virtual_only and not dev.name.startswith(VIRTUAL_DEVICE_PREFIX):
                continue
            cap = dev.capabilities()
            if ecodes.EV_KEY in cap and ecodes.KEY_A in cap.get(ecodes.EV_KEY, []):
                devices.append(dev)
        except Exception:
            pass
    return devices


def find_mouse_devices(virtual_only: bool = False) -> list[InputDevice]:
    devices = []
    for path in evdev.list_devices():
        try:
            dev = InputDevice(path)
            if virtual_only and not dev.name.startswith(VIRTUAL_DEVICE_PREFIX):
                continue
            cap = dev.capabilities()
            has_rel = ecodes.EV_REL in cap and ecodes.REL_X in cap.get(ecodes.EV_REL, [])
            has_abs = ecodes.EV_ABS in cap and ecodes.ABS_X in cap.get(ecodes.EV_ABS, [])
            has_buttons = ecodes.EV_KEY in cap and ecodes.BTN_LEFT in cap.get(ecodes.EV_KEY, [])
            if (has_rel or has_abs) and has_buttons:
                devices.append(dev)
        except Exception:
            pass
    return devices


class KeyboardState:
    def __init__(self) -> None:
        self.modifiers: int = 0
        self.pressed: list[int] = []  # up to 6 HID usage IDs

    def press(self, evdev_code: int) -> None:
        if evdev_code in MODIFIER_BITS:
            self.modifiers |= MODIFIER_BITS[evdev_code]
        elif evdev_code in KEYCODE_TO_HID:
            hid = KEYCODE_TO_HID[evdev_code]
            if hid not in self.pressed:
                self.pressed.append(hid)
                if len(self.pressed) > MAX_KEYS:
                    self.pressed.pop(0)

    def release(self, evdev_code: int) -> None:
        if evdev_code in MODIFIER_BITS:
            self.modifiers &= ~MODIFIER_BITS[evdev_code]
        elif evdev_code in KEYCODE_TO_HID:
            hid = KEYCODE_TO_HID[evdev_code]
            if hid in self.pressed:
                self.pressed.remove(hid)

    def build_report(self) -> bytes:
        keys = (self.pressed + [0] * MAX_KEYS)[:MAX_KEYS]
        return bytes([self.modifiers, 0x00] + keys)


class MouseState:
    def __init__(self) -> None:
        # Relative tracking — we accumulate and clamp to 0–32767
        self.x: int = ABSOLUTE_MAX // 2
        self.y: int = ABSOLUTE_MAX // 2
        self.buttons: int = 0
        self.scroll: int = 0
        # Raw relative deltas for gaming (consumed per report)
        self._rel_dx: int = 0
        self._rel_dy: int = 0
        # Screen dimensions for normalization (updated by caller if known)
        self._screen_w: int = 1920
        self._screen_h: int = 1080
        # Relative accumulator scale (pixels → absolute units)
        self._scale_x: float = ABSOLUTE_MAX / self._screen_w
        self._scale_y: float = ABSOLUTE_MAX / self._screen_h

    def set_screen_size(self, w: int, h: int) -> None:
        self._screen_w = w
        self._screen_h = h
        self._scale_x = ABSOLUTE_MAX / w
        self._scale_y = ABSOLUTE_MAX / h

    def move_rel(self, dx: int, dy: int) -> None:
        self.x = max(0, min(ABSOLUTE_MAX, self.x + int(dx * self._scale_x)))
        self.y = max(0, min(ABSOLUTE_MAX, self.y + int(dy * self._scale_y)))
        # Accumulate raw deltas for relative report (gaming)
        self._rel_dx += dx
        self._rel_dy += dy

    def move_abs(self, raw_x: int, raw_y: int, max_x: int, max_y: int) -> None:
        self.x = int(raw_x * ABSOLUTE_MAX / max(max_x, 1))
        self.y = int(raw_y * ABSOLUTE_MAX / max(max_y, 1))

    def button_press(self, code: int) -> None:
        if code == ecodes.BTN_LEFT:
            self.buttons |= 0x01
        elif code == ecodes.BTN_RIGHT:
            self.buttons |= 0x02
        elif code == ecodes.BTN_MIDDLE:
            self.buttons |= 0x04

    def button_release(self, code: int) -> None:
        if code == ecodes.BTN_LEFT:
            self.buttons &= ~0x01
        elif code == ecodes.BTN_RIGHT:
            self.buttons &= ~0x02
        elif code == ecodes.BTN_MIDDLE:
            self.buttons &= ~0x04

    def add_scroll(self, delta: int) -> None:
        # Clamp scroll to signed byte
        self.scroll = max(-127, min(127, delta))

    def build_report(self) -> bytes:
        x_lo = self.x & 0xFF
        x_hi = (self.x >> 8) & 0xFF
        y_lo = self.y & 0xFF
        y_hi = (self.y >> 8) & 0xFF
        scroll = self.scroll & 0xFF
        self.scroll = 0  # consume scroll after sending
        return bytes([self.buttons, x_lo, x_hi, y_lo, y_hi, scroll])

    def build_relative_report(self) -> bytes | None:
        """Build a relative mouse report with raw deltas. Returns None if no movement."""
        dx, dy = self._rel_dx, self._rel_dy
        if dx == 0 and dy == 0 and self.scroll == 0:
            return None
        # Clamp to signed 16-bit
        dx = max(-32768, min(32767, dx))
        dy = max(-32768, min(32767, dy))
        self._rel_dx = 0
        self._rel_dy = 0
        scroll = self.scroll & 0xFF
        self.scroll = 0
        return struct.pack("<BhhB", self.buttons, dx, dy, scroll)


class HIDForwarder:
    def __init__(
        self,
        config: Config,
        state: AppState,
        streams: "StreamManager | None" = None,
        control_manager: "ControlManager | None" = None,
        session_manager: "SessionManager | None" = None,
    ) -> None:
        self._config = config
        self._state = state
        self._streams = streams
        self._control_manager = control_manager
        self._session_mgr = session_manager
        self._sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self._kbd_state = KeyboardState()
        self._mouse_state = MouseState()
        self._tasks: list[asyncio.Task] = []
        self._captured: set[str] = set()  # paths currently being captured

        # Hotkey map: evdev keycode → (control_name, surface_id)
        # Populated by register_hotkey(); checked in _kbd_loop before forwarding
        self._hotkey_map: dict[int, tuple[str, str]] = {}

    def register_hotkey(self, evdev_code: int, control_name: str, surface_id: str = "hotkeys") -> None:
        """Register an evdev keycode as a hotkey that triggers a control instead of being forwarded."""
        self._hotkey_map[evdev_code] = (control_name, surface_id)
        code_name = ecodes.KEY.get(evdev_code, str(evdev_code))
        log.info("Hotkey registered: %s → %s/%s", code_name, surface_id, control_name)

    async def start(self) -> None:
        # If explicit devices are configured, open them only (no hotplug scan)
        if self._config.keyboard_device or self._config.mouse_device:
            for dev in self._open_keyboards():
                self._start_kbd(dev)
            for dev in self._open_mice():
                self._start_mouse(dev)
        else:
            # Auto-detect on startup, then watch for hotplug
            self._scan_and_capture()
            t = asyncio.create_task(self._hotplug_loop(), name="hid-hotplug")
            self._tasks.append(t)

    async def stop(self) -> None:
        for t in self._tasks:
            t.cancel()
        self._sock.close()

    def _scan_and_capture(self) -> None:
        vo = self._config.virtual_only
        for dev in find_keyboard_devices(virtual_only=vo):
            if dev.path not in self._captured:
                self._start_kbd(dev)
        for dev in find_mouse_devices(virtual_only=vo):
            if dev.path not in self._captured:
                self._start_mouse(dev)

    def _start_kbd(self, dev: InputDevice) -> None:
        self._grab(dev)
        log.info("Capturing keyboard: %s (%s)", dev.name, dev.path)
        self._captured.add(dev.path)
        t = asyncio.create_task(self._kbd_loop(dev), name=f"kbd-{dev.path}")
        self._tasks.append(t)

    def _start_mouse(self, dev: InputDevice) -> None:
        self._grab(dev)
        log.info("Capturing mouse: %s (%s)", dev.name, dev.path)
        self._captured.add(dev.path)
        t = asyncio.create_task(self._mouse_loop(dev), name=f"mouse-{dev.path}")
        self._tasks.append(t)

    def _grab(self, dev: InputDevice) -> None:
        """Exclusively grab a device so the compositor doesn't also see its events."""
        try:
            dev.grab()
            log.debug("Grabbed %s", dev.path)
        except OSError as e:
            log.warning("Could not grab %s: %s", dev.path, e)

    async def _hotplug_loop(self) -> None:
        """Poll /dev/input every 2s for new devices."""
        while True:
            await asyncio.sleep(2.0)
            self._scan_and_capture()

    def _open_keyboards(self) -> list[InputDevice]:
        if self._config.keyboard_device:
            try:
                return [InputDevice(self._config.keyboard_device)]
            except Exception as e:
                log.error("Failed to open keyboard %s: %s", self._config.keyboard_device, e)
                return []
        return find_keyboard_devices(virtual_only=self._config.virtual_only)

    def _open_mice(self) -> list[InputDevice]:
        if self._config.mouse_device:
            try:
                return [InputDevice(self._config.mouse_device)]
            except Exception as e:
                log.error("Failed to open mouse %s: %s", self._config.mouse_device, e)
                return []
        return find_mouse_devices(virtual_only=self._config.virtual_only)

    def _active_vnc_node_id(self) -> str | None:
        """Return the active node's ID if it has a VNC endpoint, else None."""
        node = self._state.get_active_node()
        if node and node.vnc_host and node.vnc_port:
            return node.id
        return None

    def _send_udp(self, packet_type: int, payload: bytes) -> None:
        node = self._state.get_active_node()
        if node is None:
            return

        # Encrypt if we have a session with this node
        if self._session_mgr:
            session = self._session_mgr.get_session(node.id)
            if session:
                packet = session.encrypt(packet_type, payload)
                try:
                    self._sock.sendto(packet, (node.host, node.port))
                except OSError as e:
                    log.debug("UDP send error: %s", e)
                return

        # Fallback: plaintext (for unpaired nodes or during session setup)
        packet = bytes([packet_type]) + payload
        try:
            self._sock.sendto(packet, (node.host, node.port))
        except OSError as e:
            log.debug("UDP send error: %s", e)

    async def _kbd_loop(self, dev: InputDevice) -> None:
        try:
            async for event in dev.async_read_loop():
                if event.type != ecodes.EV_KEY:
                    continue
                if event.value == 1:        # key down
                    down = True
                elif event.value == 0:      # key up
                    down = False
                else:
                    continue               # ignore repeat

                # Hotkey interception — consume the key, don't forward it
                if event.code in self._hotkey_map and self._control_manager:
                    if down:  # fire on key-down only
                        control_name, surface_id = self._hotkey_map[event.code]
                        asyncio.create_task(
                            self._control_manager.on_control_changed(
                                surface_id, control_name, True,
                            ),
                            name=f"hotkey-{control_name}",
                        )
                    continue

                vnc_node = self._active_vnc_node_id()
                if vnc_node and self._streams:
                    x11 = KEYCODE_TO_X11.get(event.code)
                    if x11:
                        await self._streams.send_key(vnc_node, x11, down)
                else:
                    # Non-VNC node: build HID report and send over UDP
                    if down:
                        self._kbd_state.press(event.code)
                    else:
                        self._kbd_state.release(event.code)
                    report = self._kbd_state.build_report()
                    self._send_udp(0x01, report)
                    if self._config.debug:
                        log.debug("KBD report: %s", report.hex())
        except asyncio.CancelledError:
            raise
        except Exception as e:
            log.error("Keyboard loop error (%s): %s", dev.path, e)

    async def _mouse_loop(self, dev: InputDevice) -> None:
        abs_info = dev.capabilities().get(ecodes.EV_ABS, [])
        abs_max: dict[int, int] = {}
        for item in abs_info:
            if isinstance(item, tuple):
                code, info = item
                abs_max[code] = info.max

        try:
            async for event in dev.async_read_loop():
                send = False
                if event.type == ecodes.EV_KEY:
                    if event.value == 1:
                        self._mouse_state.button_press(event.code)
                    else:
                        self._mouse_state.button_release(event.code)
                    send = True
                elif event.type == ecodes.EV_REL:
                    if event.code == ecodes.REL_X:
                        self._mouse_state.move_rel(event.value, 0)
                    elif event.code == ecodes.REL_Y:
                        self._mouse_state.move_rel(0, event.value)
                    elif event.code == ecodes.REL_WHEEL:
                        self._mouse_state.add_scroll(event.value)
                elif event.type == ecodes.EV_ABS:
                    if event.code == ecodes.ABS_X:
                        self._mouse_state.move_abs(
                            event.value, self._mouse_state.y,
                            abs_max.get(ecodes.ABS_X, ABSOLUTE_MAX),
                            ABSOLUTE_MAX,
                        )
                    elif event.code == ecodes.ABS_Y:
                        self._mouse_state.move_abs(
                            self._mouse_state.x, event.value,
                            ABSOLUTE_MAX,
                            abs_max.get(ecodes.ABS_Y, ABSOLUTE_MAX),
                        )
                elif event.type == ecodes.EV_SYN:
                    send = True

                if send:
                    vnc_node = self._active_vnc_node_id()
                    if vnc_node and self._streams:
                        # Map HID absolute coords (0–32767) to display pixels
                        dims = self._streams.vnc_dimensions(vnc_node)
                        w, h = dims if dims else (1280, 800)
                        px = self._mouse_state.x * w // ABSOLUTE_MAX
                        py = self._mouse_state.y * h // ABSOLUTE_MAX
                        await self._streams.send_pointer(vnc_node, px, py, self._mouse_state.buttons)
                    else:
                        report = self._mouse_state.build_report()
                        self._send_udp(PKT_MOUSE, report)
                        # Also send relative report for gaming — node uses
                        # whichever matches its current input mode
                        rel_report = self._mouse_state.build_relative_report()
                        if rel_report:
                            self._send_udp(PKT_MOUSE_REL, rel_report)
                        if self._config.debug:
                            log.debug("MOUSE report: %s", report.hex())
        except asyncio.CancelledError:
            raise
        except Exception as e:
            log.error("Mouse loop error (%s): %s", dev.path, e)
