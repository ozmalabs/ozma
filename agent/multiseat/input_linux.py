# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Linux input backend for multi-seat.

Enumerates evdev input devices from /dev/input/, groups them by USB hub
topology, and assigns them to seats.

Assignment works by:
1. Grouping devices by USB hub (via usb_topology.py)
2. Optionally grabbing devices with EVIOCGRAB to prevent other processes
   from reading them (exclusive access for the seat's X display)
3. Creating xinput device → X screen mappings when running multi-head X
"""

from __future__ import annotations

import logging
import os
import re
import subprocess
from pathlib import Path
from typing import TYPE_CHECKING

from .input_router import InputGroup, InputRouterBackend
from .usb_topology import USBTopologyScanner

if TYPE_CHECKING:
    from .seat import Seat

log = logging.getLogger("ozma.agent.multiseat.input_linux")


class LinuxInputBackend(InputRouterBackend):
    """
    Linux input device enumeration and seat assignment.

    Uses the USBTopologyScanner to group evdev devices by USB hub, then
    assigns groups to seats. Assignment uses xinput to map devices to
    specific output heads when running under X11.
    """

    def __init__(self) -> None:
        self._scanner = USBTopologyScanner()
        self._assignments: dict[str, str] = {}  # hub_path -> seat_name
        self._grabbed_fds: dict[str, int] = {}  # evdev_path -> fd

    def enumerate_groups(self) -> list[InputGroup]:
        """
        Enumerate all input devices grouped by USB hub topology.

        Returns InputGroups with evdev paths sorted by device type.
        Groups with no keyboard or mouse are still returned (gamepads).
        """
        return self._scanner.scan()

    def assign(self, group: InputGroup, seat: "Seat") -> bool:
        """
        Assign an input group to a seat.

        On X11: uses xinput to map devices to the seat's display output.
        This uses `xinput map-to-output` to confine pointer devices to
        the seat's monitor, and `xinput set-prop` for keyboard mapping.
        """
        display = os.environ.get("DISPLAY", ":0")

        if not group.has_input:
            log.warning("Cannot assign group %s: no keyboard or mouse", group.hub_path)
            return False

        success = True

        # Map pointer devices (mice, touchpads) to the seat's display output
        for mouse_path in group.mice:
            xinput_id = self._evdev_to_xinput_id(mouse_path, display)
            if xinput_id and seat.display:
                ok = self._xinput_map_to_output(xinput_id, seat.display.name, display)
                if not ok:
                    success = False

        # For keyboards, we track the assignment but don't need xinput mapping
        # (keyboard events go to the focused window regardless of output)
        self._assignments[group.hub_path] = seat.name
        log.info("Assigned input group %s → seat %s (%d kbd, %d mice, %d gamepad)",
                 group.hub_path, seat.name,
                 len(group.keyboards), len(group.mice), len(group.gamepads))

        return success

    def unassign(self, group: InputGroup) -> bool:
        """Release an input group from its seat assignment."""
        hub = group.hub_path
        if hub in self._assignments:
            del self._assignments[hub]
            log.info("Unassigned input group %s", hub)

        # Release any grabbed devices
        for dev_path in group.all_devices:
            self._ungrab_device(dev_path)

        return True

    def grab_exclusive(self, group: InputGroup) -> bool:
        """
        Grab all devices in a group for exclusive access.

        Uses EVIOCGRAB ioctl to prevent other processes from reading the
        devices. This is optional — useful when you want to prevent the
        default X input handler from processing events that are being
        injected via uinput for a different seat.
        """
        import fcntl
        import struct

        EVIOCGRAB = 0x40044590
        success = True

        for dev_path in group.all_devices:
            try:
                fd = os.open(dev_path, os.O_RDONLY | os.O_NONBLOCK)
                fcntl.ioctl(fd, EVIOCGRAB, 1)
                self._grabbed_fds[dev_path] = fd
                log.debug("Grabbed exclusive: %s", dev_path)
            except (OSError, IOError) as e:
                log.warning("Failed to grab %s: %s (need root?)", dev_path, e)
                success = False

        return success

    def _ungrab_device(self, dev_path: str) -> None:
        """Release EVIOCGRAB on a single device."""
        fd = self._grabbed_fds.pop(dev_path, None)
        if fd is not None:
            try:
                import fcntl
                EVIOCGRAB = 0x40044590
                fcntl.ioctl(fd, EVIOCGRAB, 0)
                os.close(fd)
                log.debug("Released grab: %s", dev_path)
            except (OSError, IOError):
                try:
                    os.close(fd)
                except OSError:
                    pass

    def _evdev_to_xinput_id(self, evdev_path: str, display: str) -> int | None:
        """
        Find the xinput device ID for a given evdev path.

        Parses `xinput list` and matches against the device name or
        reads /sys/class/input/ to find the mapping.
        """
        event_name = Path(evdev_path).name  # "event5"
        if not event_name.startswith("event"):
            return None

        # Read the device name from sysfs
        name_path = Path(f"/sys/class/input/{event_name}/device/name")
        if not name_path.exists():
            return None

        try:
            device_name = name_path.read_text().strip()
        except OSError:
            return None

        if not device_name:
            return None

        # Find this device in xinput list
        try:
            result = subprocess.run(
                ["xinput", "list", "--short"],
                capture_output=True, text=True, timeout=5,
                env={**os.environ, "DISPLAY": display},
            )
            if result.returncode != 0:
                return None

            for line in result.stdout.splitlines():
                if device_name in line:
                    # Parse: "⎜ ↳ Device Name    id=12  [slave pointer  (2)]"
                    m = re.search(r"id=(\d+)", line)
                    if m:
                        return int(m.group(1))
        except (FileNotFoundError, subprocess.TimeoutExpired):
            return None

        return None

    def _xinput_map_to_output(self, xinput_id: int, output_name: str,
                               display: str) -> bool:
        """
        Map a pointer device to a specific xrandr output.

        Uses `xinput map-to-output <id> <output>` to confine the pointer
        to the given monitor's area.
        """
        try:
            result = subprocess.run(
                ["xinput", "map-to-output", str(xinput_id), output_name],
                capture_output=True, text=True, timeout=5,
                env={**os.environ, "DISPLAY": display},
            )
            if result.returncode == 0:
                log.debug("Mapped xinput %d → output %s", xinput_id, output_name)
                return True
            log.warning("xinput map-to-output failed: %s", result.stderr.strip())
            return False
        except (FileNotFoundError, subprocess.TimeoutExpired) as e:
            log.warning("xinput map-to-output error: %s", e)
            return False

    def cleanup(self) -> None:
        """Release all grabbed devices."""
        for dev_path in list(self._grabbed_fds.keys()):
            self._ungrab_device(dev_path)
        self._assignments.clear()
