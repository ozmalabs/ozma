# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Windows input backend for multi-seat — Phase 2.

Enumerates HID input devices via Raw Input API (GetRawInputDeviceList)
and groups them by USB hub topology using SetupAPI/CfgMgr32. Per-seat
input routing (RegisterRawInputDevices per-window) is Phase 3.

Platform guard: all Windows ctypes calls are inside ``if sys.platform == 'win32'``
blocks. On non-Windows, the module imports cleanly and returns empty results.
"""

from __future__ import annotations

import logging
import re
import sys
from dataclasses import dataclass, field
from typing import TYPE_CHECKING

from .input_router import InputGroup, InputRouterBackend

if TYPE_CHECKING:
    from .seat import Seat

log = logging.getLogger("ozma.agent.multiseat.input_windows")


# ── Win32 structures (ctypes, Windows only) ──────────────────────────────────

if sys.platform == "win32":
    import ctypes
    import ctypes.wintypes as wintypes
    from ctypes import (
        POINTER, Structure, byref, c_uint, c_void_p, c_wchar, windll,
    )

    class RAWINPUTDEVICELIST(Structure):
        _fields_ = [
            ("hDevice", c_void_p),
            ("dwType", wintypes.DWORD),
        ]

    # Device types
    RIM_TYPEMOUSE = 0
    RIM_TYPEKEYBOARD = 1
    RIM_TYPEHID = 2

    _TYPE_NAMES = {
        RIM_TYPEMOUSE: "mouse",
        RIM_TYPEKEYBOARD: "keyboard",
        RIM_TYPEHID: "hid",
    }

    # RIDI flags
    RIDI_DEVICENAME = 0x20000007
    RIDI_DEVICEINFO = 0x2000000b

    class RID_DEVICE_INFO_MOUSE(Structure):
        _fields_ = [
            ("dwId", wintypes.DWORD),
            ("dwNumberOfButtons", wintypes.DWORD),
            ("dwSampleRate", wintypes.DWORD),
            ("fHasHorizontalWheel", wintypes.BOOL),
        ]

    class RID_DEVICE_INFO_KEYBOARD(Structure):
        _fields_ = [
            ("dwType", wintypes.DWORD),
            ("dwSubType", wintypes.DWORD),
            ("dwKeyboardMode", wintypes.DWORD),
            ("dwNumberOfFunctionKeys", wintypes.DWORD),
            ("dwNumberOfIndicators", wintypes.DWORD),
            ("dwNumberOfKeysTotal", wintypes.DWORD),
        ]

    class RID_DEVICE_INFO_HID(Structure):
        _fields_ = [
            ("dwVendorId", wintypes.DWORD),
            ("dwProductId", wintypes.DWORD),
            ("dwVersionNumber", wintypes.DWORD),
            ("usUsagePage", wintypes.WORD),
            ("usUsage", wintypes.WORD),
        ]

    class _RID_DEVICE_INFO_UNION(ctypes.Union):
        _fields_ = [
            ("mouse", RID_DEVICE_INFO_MOUSE),
            ("keyboard", RID_DEVICE_INFO_KEYBOARD),
            ("hid", RID_DEVICE_INFO_HID),
        ]

    class RID_DEVICE_INFO(Structure):
        _fields_ = [
            ("cbSize", wintypes.DWORD),
            ("dwType", wintypes.DWORD),
            ("info", _RID_DEVICE_INFO_UNION),
        ]


@dataclass
class WindowsInputDevice:
    """A single input device discovered via Raw Input API."""
    handle: int
    device_type: str       # "keyboard", "mouse", "hid"
    device_name: str       # Raw Input device path
    vendor_id: int = 0
    product_id: int = 0
    usb_parent: str = ""   # USB hub/port path for grouping
    friendly_name: str = ""


class WindowsInputBackend(InputRouterBackend):
    """
    Windows input device enumeration and grouping via Raw Input API.

    Enumerates all HID devices using GetRawInputDeviceList, gets device
    names and info via GetRawInputDeviceInfoW, and groups devices by
    USB parent path using SetupAPI/CfgMgr32 for topology detection.

    Per-seat input routing (RegisterRawInputDevices per-window with
    RIDEV_DEVNOTIFY) is deferred to Phase 3 — it requires creating
    per-seat message-only windows and a custom message pump.
    """

    def __init__(self) -> None:
        self._devices: list[WindowsInputDevice] = []
        self._assignments: dict[str, str] = {}  # hub_path -> seat_name

    def enumerate_groups(self) -> list[InputGroup]:
        """
        Enumerate all input devices and group them by USB hub topology.

        Phase 2: returns a default group (all devices as one group).
        Raw Input enumeration + USB topology grouping requires ctypes
        calls that can crash on some Windows configurations.
        Full per-device routing is Phase 3.
        """
        default = [InputGroup(
            hub_path="default",
            keyboards=["default-keyboard"],
            mice=["default-mouse"],
        )]

        if sys.platform != "win32":
            return default

        # Phase 2: return default group. Raw Input ctypes enumeration
        # can crash on some Windows configs. Full per-device grouping
        # and routing is Phase 3.
        log.info("Using default input group (per-device routing is Phase 3)")
        return default

    def assign(self, group: InputGroup, seat: "Seat") -> bool:
        """
        Assign an input group to a seat.

        Phase 2: records the assignment. Actual per-seat input routing
        via RegisterRawInputDevices is Phase 3.
        """
        self._assignments[group.hub_path] = seat.name
        log.info("Input group %s assigned to seat %s (routing is Phase 3)",
                 group.hub_path, seat.name)
        return True

    def unassign(self, group: InputGroup) -> bool:
        """Release an input group from its seat assignment."""
        if group.hub_path in self._assignments:
            del self._assignments[group.hub_path]
            log.info("Input group %s unassigned", group.hub_path)
        return True

    def _enumerate_raw_input(self) -> list[WindowsInputDevice]:
        """Enumerate all raw input devices via GetRawInputDeviceList."""
        user32 = windll.user32
        devices: list[WindowsInputDevice] = []

        # Get device count
        num_devices = c_uint(0)
        user32.GetRawInputDeviceList(
            None, byref(num_devices), ctypes.sizeof(RAWINPUTDEVICELIST),
        )

        if num_devices.value == 0:
            return []

        # Allocate and fill device list
        raw_devices = (RAWINPUTDEVICELIST * num_devices.value)()
        count = user32.GetRawInputDeviceList(
            raw_devices, byref(num_devices), ctypes.sizeof(RAWINPUTDEVICELIST),
        )

        if count == ctypes.c_uint(-1).value:
            log.warning("GetRawInputDeviceList failed")
            return []

        for i in range(count):
            rd = raw_devices[i]
            handle = rd.hDevice
            dev_type = _TYPE_NAMES.get(rd.dwType, "unknown")

            if dev_type == "unknown":
                continue

            # Get device name
            name_size = c_uint(0)
            user32.GetRawInputDeviceInfoW(handle, RIDI_DEVICENAME, None, byref(name_size))

            if name_size.value == 0:
                continue

            name_buf = ctypes.create_unicode_buffer(name_size.value)
            user32.GetRawInputDeviceInfoW(
                handle, RIDI_DEVICENAME, name_buf, byref(name_size),
            )
            device_name = name_buf.value

            # Get device info (vendor/product ID)
            vendor_id = 0
            product_id = 0
            info = RID_DEVICE_INFO()
            info.cbSize = ctypes.sizeof(RID_DEVICE_INFO)
            info_size = c_uint(ctypes.sizeof(RID_DEVICE_INFO))
            ret = user32.GetRawInputDeviceInfoW(
                handle, RIDI_DEVICEINFO, byref(info), byref(info_size),
            )

            if ret > 0:
                if rd.dwType == RIM_TYPEHID:
                    vendor_id = info.info.hid.dwVendorId
                    product_id = info.info.hid.dwProductId
                    # Classify HID usage pages
                    usage_page = info.info.hid.usUsagePage
                    usage = info.info.hid.usUsage
                    # Gamepad: usage page 0x01, usage 0x04 or 0x05
                    if usage_page == 0x01 and usage in (0x04, 0x05):
                        dev_type = "gamepad"

            # Try to extract VID/PID from device path
            if not vendor_id:
                vid, pid = self._parse_vid_pid(device_name)
                vendor_id = vid
                product_id = pid

            dev = WindowsInputDevice(
                handle=handle,
                device_type=dev_type,
                device_name=device_name,
                vendor_id=vendor_id,
                product_id=product_id,
            )
            devices.append(dev)

        log.info("Raw Input: found %d devices (%d keyboards, %d mice, %d other)",
                 len(devices),
                 sum(1 for d in devices if d.device_type == "keyboard"),
                 sum(1 for d in devices if d.device_type == "mouse"),
                 sum(1 for d in devices if d.device_type not in ("keyboard", "mouse")))

        return devices

    def _resolve_usb_parents(self) -> None:
        """
        Resolve USB parent device paths for topology grouping.

        Uses SetupAPI and CfgMgr32 to find the USB hub that each device
        is connected to. Devices on the same hub are assumed to belong
        to the same user (one keyboard + one mouse per hub port).

        Falls back to VID:PID grouping if SetupAPI is unavailable.
        """
        if sys.platform != "win32":
            return

        try:
            self._resolve_via_cfgmgr32()
        except Exception as e:
            log.debug("CfgMgr32 parent resolution failed: %s — using path heuristic", e)
            self._resolve_via_path_heuristic()

    def _resolve_via_cfgmgr32(self) -> None:
        """
        Resolve USB parents via CM_Locate_DevNode + CM_Get_Parent.

        The device name from Raw Input has the form:
          \\\\?\\HID#VID_046D&PID_C52B&MI_01#...#{guid}

        We extract the device instance ID and use CfgMgr32 to walk up
        the device tree to find the USB hub parent.
        """
        cfgmgr32 = windll.cfgmgr32

        CR_SUCCESS = 0
        CM_LOCATE_DEVNODE_NORMAL = 0

        for dev in self._devices:
            try:
                # Extract device instance ID from Raw Input path
                # \\?\HID#VID_046D&PID_C52B&MI_01#... -> HID\VID_046D&PID_C52B&MI_01\...
                instance_id = dev.device_name
                if instance_id.startswith("\\\\?\\"):
                    instance_id = instance_id[4:]
                instance_id = instance_id.replace("#", "\\")
                # Strip trailing GUID
                if instance_id.endswith("}"):
                    last_brace = instance_id.rfind("\\{")
                    if last_brace >= 0:
                        instance_id = instance_id[:last_brace]

                # Locate the device node
                dev_inst = wintypes.DWORD(0)
                result = cfgmgr32.CM_Locate_DevNodeW(
                    byref(dev_inst), instance_id, CM_LOCATE_DEVNODE_NORMAL,
                )
                if result != CR_SUCCESS:
                    continue

                # Walk up to find USB hub parent
                parent_inst = wintypes.DWORD(0)
                result = cfgmgr32.CM_Get_Parent(
                    byref(parent_inst), dev_inst, 0,
                )
                if result != CR_SUCCESS:
                    continue

                # Get parent's device instance ID
                buf_size = wintypes.ULONG(256)
                buf = ctypes.create_unicode_buffer(256)
                result = cfgmgr32.CM_Get_Device_IDW(
                    parent_inst, buf, buf_size, 0,
                )
                if result == CR_SUCCESS:
                    parent_id = buf.value
                    # Walk one more level if this is still an HID node
                    if parent_id.upper().startswith("HID\\"):
                        grandparent_inst = wintypes.DWORD(0)
                        result = cfgmgr32.CM_Get_Parent(
                            byref(grandparent_inst), parent_inst, 0,
                        )
                        if result == CR_SUCCESS:
                            result = cfgmgr32.CM_Get_Device_IDW(
                                grandparent_inst, buf, buf_size, 0,
                            )
                            if result == CR_SUCCESS:
                                parent_id = buf.value

                    dev.usb_parent = parent_id

            except Exception as e:
                log.debug("CfgMgr32 lookup failed for %s: %s", dev.device_name, e)
                continue

    def _resolve_via_path_heuristic(self) -> None:
        """
        Fallback: extract USB hub path from Raw Input device name.

        Raw Input device names look like:
          \\\\?\\HID#VID_046D&PID_C52B&MI_01#8&12345678&0&0000#{...}

        The instance path component (8&12345678&0&0000) encodes the USB
        location. We use the first two segments as a grouping key.
        """
        for dev in self._devices:
            parts = dev.device_name.split("#")
            if len(parts) >= 3:
                # Use the HID identifier + instance path as parent key
                dev.usb_parent = f"{parts[0]}#{parts[1]}"
            else:
                dev.usb_parent = f"vid_{dev.vendor_id:04x}_pid_{dev.product_id:04x}"

    def _group_by_parent(self) -> list[InputGroup]:
        """Group devices by their USB parent path."""
        groups: dict[str, InputGroup] = {}

        for dev in self._devices:
            parent = dev.usb_parent or "unknown"

            if parent not in groups:
                groups[parent] = InputGroup(hub_path=parent)

            group = groups[parent]

            if dev.device_type == "keyboard":
                group.keyboards.append(dev.device_name)
            elif dev.device_type == "mouse":
                group.mice.append(dev.device_name)
            elif dev.device_type == "gamepad":
                group.gamepads.append(dev.device_name)
            else:
                group.other.append(dev.device_name)

        return list(groups.values())

    @staticmethod
    def _parse_vid_pid(device_name: str) -> tuple[int, int]:
        """Extract VID and PID from a Raw Input device path."""
        vid = 0
        pid = 0
        m = re.search(r"VID_([0-9A-Fa-f]{4})", device_name)
        if m:
            vid = int(m.group(1), 16)
        m = re.search(r"PID_([0-9A-Fa-f]{4})", device_name)
        if m:
            pid = int(m.group(1), 16)
        return vid, pid

    def cleanup(self) -> None:
        """Release all assignments."""
        self._assignments.clear()
        self._devices.clear()
