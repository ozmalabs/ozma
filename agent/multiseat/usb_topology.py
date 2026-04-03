# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
USB topology scanner — groups input devices by shared USB hub.

On Linux: reads /sys/bus/usb/devices/ to build the device tree and maps
evdev input devices back to their USB parent hub.

On Windows: stub for Phase 2 (SetupAPI / WMI).

The key insight: devices plugged into the same USB hub belong to one user.
A USB hub with a keyboard + mouse = one seat's input group.
"""

from __future__ import annotations

import logging
import os
import platform
import re
from pathlib import Path

from .input_router import InputGroup

log = logging.getLogger("ozma.agent.multiseat.usb_topology")


class USBTopologyScanner:
    """
    Scan USB topology and group input devices by hub.

    Linux: reads sysfs to trace each evdev device back to its USB parent hub.
    Windows: stub — returns a single group with all devices (Phase 2).
    """

    def scan(self) -> list[InputGroup]:
        """Scan USB topology and return input device groups."""
        system = platform.system()
        if system == "Linux":
            return self._scan_linux()
        elif system == "Windows":
            return self._scan_windows_stub()
        log.warning("USB topology scanning not supported on %s", system)
        return []

    def _scan_linux(self) -> list[InputGroup]:
        """
        Build USB device tree from sysfs, then group evdev input devices.

        sysfs layout:
          /sys/bus/usb/devices/1-1/        <- hub
          /sys/bus/usb/devices/1-1.1/      <- device on port 1 (keyboard)
          /sys/bus/usb/devices/1-1.2/      <- device on port 2 (mouse)

        We trace each /dev/input/eventN back through sysfs to find its
        USB parent hub path, then group devices with the same hub.
        """
        groups: dict[str, InputGroup] = {}
        internal_group = InputGroup(hub_path="internal")

        input_dir = Path("/dev/input")
        if not input_dir.exists():
            log.warning("/dev/input not found")
            return []

        for entry in sorted(input_dir.iterdir()):
            if not entry.name.startswith("event"):
                continue

            event_path = str(entry)
            device_info = self._classify_evdev_device(entry.name)
            if not device_info:
                continue

            dev_type, capabilities = device_info
            hub_path = self._find_usb_hub_path(entry.name)

            if hub_path is None:
                # Non-USB device (PS/2 keyboard, laptop touchpad, etc.)
                self._add_to_group(internal_group, event_path, dev_type)
                continue

            if hub_path not in groups:
                groups[hub_path] = InputGroup(hub_path=hub_path)
            self._add_to_group(groups[hub_path], event_path, dev_type)

        result = []
        if internal_group.has_input:
            result.append(internal_group)
        result.extend(
            g for g in sorted(groups.values(), key=lambda g: g.hub_path)
            if g.device_count > 0
        )

        log.info("USB topology scan: %d groups (%d internal, %d USB hubs)",
                 len(result),
                 1 if internal_group.has_input else 0,
                 len(groups))
        for g in result:
            log.debug("  hub=%s: kbd=%s mouse=%s gamepad=%s",
                      g.hub_path, g.keyboards, g.mice, g.gamepads)

        return result

    def _classify_evdev_device(self, event_name: str) -> tuple[str, set[str]] | None:
        """
        Classify an evdev device by reading its capabilities from sysfs.

        Returns (device_type, capabilities) or None if not an input device
        we care about.
        """
        caps_path = Path(f"/sys/class/input/{event_name}/device/capabilities")
        if not caps_path.is_dir():
            return None

        try:
            ev_bits = (caps_path / "ev").read_text().strip()
            key_bits = (caps_path / "key").read_text().strip()
            rel_bits = (caps_path / "rel").read_text().strip()
            abs_bits = (caps_path / "abs").read_text().strip()
        except (OSError, FileNotFoundError):
            return None

        ev = int(ev_bits, 16) if ev_bits else 0
        capabilities = set()

        # EV_KEY = 0x01, EV_REL = 0x02, EV_ABS = 0x03
        has_key = bool(ev & (1 << 1))
        has_rel = bool(ev & (1 << 2))
        has_abs = bool(ev & (1 << 3))

        if has_key:
            capabilities.add("key")
        if has_rel:
            capabilities.add("rel")
        if has_abs:
            capabilities.add("abs")

        # Determine device type from capabilities:
        # Keyboard: has many key bits (letters/numbers), no rel/abs movement
        # Mouse: has rel movement + buttons
        # Gamepad: has abs axes + buttons (joystick-like)

        # Count key bits to distinguish keyboard from device with few buttons
        key_count = 0
        if key_bits:
            for hex_word in key_bits.split():
                key_count += bin(int(hex_word, 16)).count("1")

        if has_abs and has_key and key_count < 20:
            # Gamepad: absolute axes + few buttons
            return ("gamepad", capabilities)
        elif has_key and key_count >= 30:
            # Keyboard: many keys
            return ("keyboard", capabilities)
        elif has_rel and has_key:
            # Mouse: relative movement + buttons
            return ("mouse", capabilities)
        elif has_abs and has_key:
            # Touchpad or tablet — treat as mouse
            return ("mouse", capabilities)

        return None

    def _find_usb_hub_path(self, event_name: str) -> str | None:
        """
        Trace an evdev device back through sysfs to find its USB hub path.

        Walks up the device tree from /sys/class/input/eventN/device/
        looking for USB device entries. The parent of the USB device
        (if it's a hub) gives us the grouping key.

        Returns the hub topology path (e.g. "1-1", "2-3.1") or None
        if the device is not USB.
        """
        device_link = Path(f"/sys/class/input/{event_name}/device")
        if not device_link.exists():
            return None

        try:
            # Resolve the full sysfs path
            real_path = device_link.resolve()
        except OSError:
            return None

        # Walk up the path looking for USB topology markers
        # USB device paths contain segments like "1-1.2:1.0" (bus-port.port:config.interface)
        parts = str(real_path).split("/")
        usb_device_path = None

        for i, part in enumerate(parts):
            # USB device path pattern: N-N or N-N.N or N-N.N.N
            if re.match(r"^\d+-\d+(\.\d+)*$", part):
                usb_device_path = part

        if not usb_device_path:
            return None

        # The hub path is the parent: "1-1.2" -> hub is "1-1"
        # "1-1" -> hub is bus root "1-0:1.0" (top-level)
        # For grouping, we want devices that share a hub one level up
        dot_idx = usb_device_path.rfind(".")
        if dot_idx > 0:
            hub_path = usb_device_path[:dot_idx]
        else:
            # Device is directly on a root hub port — use the port itself
            # as the group key. E.g. "1-1" and "1-2" are separate root ports.
            hub_path = usb_device_path

        return hub_path

    def _add_to_group(self, group: InputGroup, event_path: str, dev_type: str) -> None:
        """Add a device path to the appropriate list in a group."""
        match dev_type:
            case "keyboard":
                group.keyboards.append(event_path)
            case "mouse":
                group.mice.append(event_path)
            case "gamepad":
                group.gamepads.append(event_path)
            case _:
                group.other.append(event_path)

    def _scan_windows_stub(self) -> list[InputGroup]:
        """
        Windows USB topology scan — stub for Phase 2.

        Will use SetupAPI's CM_Get_Parent to trace USB devices to their
        parent hub. For now, returns all input devices in a single group.
        """
        log.info("Windows USB topology: stub (Phase 2 — all devices in one group)")
        return [InputGroup(
            hub_path="default",
            keyboards=["default-keyboard"],
            mice=["default-mouse"],
        )]
