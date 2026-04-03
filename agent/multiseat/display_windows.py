# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Windows display backend for multi-seat — Phase 2.

Uses DXGI (IDXGIFactory1::EnumAdapters1 + EnumOutputs) via ctypes to
enumerate physical displays with full adapter info. Falls back to
EnumDisplayMonitors + GetMonitorInfo via user32.dll on older systems.

For virtual displays: IDD driver control is Phase 3 (stub provided).
"""

from __future__ import annotations

import asyncio
import logging
import sys
from dataclasses import dataclass, field
from typing import Any

from .display_backend import DisplayBackend, DisplayInfo
from .virtual_display import VirtualDisplayManager

log = logging.getLogger("ozma.agent.multiseat.display_windows")


# ── DXGI COM definitions (ctypes, Windows only) ─────────────────────────────

if sys.platform == "win32":
    import ctypes
    import ctypes.wintypes as wintypes
    from ctypes import (
        HRESULT, POINTER, Structure, byref, c_int, c_uint, c_uint32,
        c_void_p, c_wchar, c_wchar_p, windll,
    )

    # COM helpers
    IID = ctypes.c_byte * 16
    GUID = IID

    def _make_iid(hex_str: str) -> IID:
        """Parse a GUID string like '7b7166ec-...' into 16-byte IID."""
        import uuid
        u = uuid.UUID(hex_str)
        return IID(*u.bytes_le)

    # GUIDs
    IID_IDXGIFactory1 = _make_iid("770aae78-f26f-4dba-a829-253c83d1b387")
    CLSID_DXGI = _make_iid("770aae78-f26f-4dba-a829-253c83d1b387")

    # DXGI_OUTPUT_DESC
    class DXGI_OUTPUT_DESC(Structure):
        _fields_ = [
            ("DeviceName", c_wchar * 32),
            ("DesktopCoordinates", wintypes.RECT),
            ("AttachedToDesktop", ctypes.c_int),
            ("Rotation", c_uint),
            ("Monitor", c_void_p),
        ]

    # DXGI_ADAPTER_DESC1
    class DXGI_ADAPTER_DESC1(Structure):
        _fields_ = [
            ("Description", c_wchar * 128),
            ("VendorId", c_uint),
            ("DeviceId", c_uint),
            ("SubSysId", c_uint),
            ("Revision", c_uint),
            ("DedicatedVideoMemory", ctypes.c_size_t),
            ("DedicatedSystemMemory", ctypes.c_size_t),
            ("SharedSystemMemory", ctypes.c_size_t),
            ("AdapterLuid", ctypes.c_byte * 8),
        ]

    # MONITORINFOEX for fallback path
    class MONITORINFOEX(Structure):
        _fields_ = [
            ("cbSize", wintypes.DWORD),
            ("rcMonitor", wintypes.RECT),
            ("rcWork", wintypes.RECT),
            ("dwFlags", wintypes.DWORD),
            ("szDevice", c_wchar * 32),
        ]

    MONITORENUMPROC = ctypes.WINFUNCTYPE(
        ctypes.c_bool,
        c_void_p,                       # hMonitor
        c_void_p,                       # hdcMonitor
        POINTER(wintypes.RECT),         # lprcMonitor
        ctypes.c_long,                  # dwData
    )

    # DEVMODE for EnumDisplaySettings
    class DEVMODE(Structure):
        _fields_ = [
            ("dmDeviceName", c_wchar * 32),
            ("dmSpecVersion", wintypes.WORD),
            ("dmDriverVersion", wintypes.WORD),
            ("dmSize", wintypes.WORD),
            ("dmDriverExtra", wintypes.WORD),
            ("dmFields", wintypes.DWORD),
            # Positional union (we only need a few fields)
            ("_pad1", ctypes.c_byte * 16),
            ("dmPosition_x", ctypes.c_long),
            ("dmPosition_y", ctypes.c_long),
            ("dmDisplayOrientation", wintypes.DWORD),
            ("dmDisplayFixedOutput", wintypes.DWORD),
            ("_pad2", ctypes.c_byte * 76),
            ("dmBitsPerPel", wintypes.DWORD),
            ("dmPelsWidth", wintypes.DWORD),
            ("dmPelsHeight", wintypes.DWORD),
            ("_pad3", wintypes.DWORD),
            ("dmDisplayFrequency", wintypes.DWORD),
            ("_pad4", ctypes.c_byte * 24),
        ]

    # COM vtable access helpers
    def _vtable_method(iface_ptr: c_void_p, index: int):
        """Get a function pointer from a COM interface vtable."""
        vtable = ctypes.cast(iface_ptr, POINTER(c_void_p))[0]
        vtable_arr = ctypes.cast(vtable, POINTER(c_void_p))
        return vtable_arr[index]

    def _call_com(iface_ptr: c_void_p, vtable_index: int,
                  argtypes: list, *args) -> int:
        """Call a COM method via vtable with given argtypes."""
        fn_ptr = _vtable_method(iface_ptr, vtable_index)
        fn_type = ctypes.CFUNCTYPE(HRESULT, c_void_p, *argtypes)
        fn = fn_type(fn_ptr)
        return fn(iface_ptr, *args)


@dataclass
class _AdapterOutput:
    """Internal: one DXGI output with its adapter info."""
    adapter_index: int
    adapter_name: str
    vendor_id: int
    vram_mb: int
    output_index: int       # index within this adapter (for dxcam output_idx)
    global_output_index: int  # sequential index across all adapters
    device_name: str        # e.g. "\\\\.\\DISPLAY1"
    width: int
    height: int
    x: int
    y: int
    attached: bool
    refresh_rate: int


class WindowsDisplayBackend(DisplayBackend):
    """
    Windows display enumeration via DXGI + user32 fallback.

    DXGI provides per-adapter output enumeration with desktop coordinates.
    The output index maps directly to dxcam's ``output_idx`` parameter for
    per-display capture via DXGI Desktop Duplication.
    """

    def __init__(self) -> None:
        self._virtual_displays: list[DisplayInfo] = []
        self._next_virtual_idx = 100
        self._adapter_outputs: list[_AdapterOutput] = []
        self._vdm = VirtualDisplayManager()

    @property
    def adapter_outputs(self) -> list[_AdapterOutput]:
        """Expose adapter outputs for capture backends."""
        return list(self._adapter_outputs)

    def enumerate(self) -> list[DisplayInfo]:
        """
        Enumerate displays via DXGI, falling back to user32 EnumDisplayMonitors.

        Returns one DisplayInfo per active monitor output. The ``index`` field
        is the DXGI output index (adapter-relative) which maps to dxcam's
        ``output_idx`` parameter.
        """
        if sys.platform != "win32":
            log.warning("WindowsDisplayBackend used on non-Windows platform")
            return [self._default_display()]

        # Try DXGI first (provides adapter info + correct output indices)
        try:
            displays = self._enumerate_dxgi()
            if displays:
                return displays
        except Exception as e:
            log.warning("DXGI enumeration failed: %s — falling back to user32", e)

        # Fallback: EnumDisplayMonitors
        try:
            return self._enumerate_user32()
        except Exception as e:
            log.warning("user32 enumeration failed: %s", e)
            return [self._default_display()]

    def _enumerate_dxgi(self) -> list[DisplayInfo]:
        """
        Enumerate via DXGI COM interfaces.

        IDXGIFactory1 -> EnumAdapters1 -> EnumOutputs for each adapter.
        Each output gives us monitor name, desktop rect, and the adapter
        it belongs to.
        """
        if sys.platform != "win32":
            return []

        # CoInitializeEx for COM
        ole32 = windll.ole32
        hr = ole32.CoInitializeEx(None, 0)  # COINIT_MULTITHREADED
        needs_uninit = hr >= 0  # S_OK or S_FALSE

        displays: list[DisplayInfo] = []
        factory = c_void_p()

        try:
            # CreateDXGIFactory1
            dxgi = windll.dxgi
            hr = dxgi.CreateDXGIFactory1(
                byref(IID_IDXGIFactory1),
                byref(factory),
            )
            if hr < 0 or not factory:
                log.debug("CreateDXGIFactory1 failed: HRESULT 0x%08x", hr & 0xFFFFFFFF)
                return []

            global_output_idx = 0
            adapter_idx = 0

            while True:
                adapter = c_void_p()
                # IDXGIFactory1::EnumAdapters1 is vtable index 12
                hr = _call_com(factory, 12, [c_uint, POINTER(c_void_p)],
                               c_uint(adapter_idx), byref(adapter))

                if hr < 0 or not adapter:
                    break  # DXGI_ERROR_NOT_FOUND — no more adapters

                try:
                    # IDXGIAdapter1::GetDesc1 — vtable index 10
                    desc = DXGI_ADAPTER_DESC1()
                    _call_com(adapter, 10, [POINTER(DXGI_ADAPTER_DESC1)], byref(desc))

                    adapter_name = desc.Description.rstrip("\x00")
                    vendor_id = desc.VendorId
                    vram_mb = desc.DedicatedVideoMemory // (1024 * 1024)

                    # Enumerate outputs on this adapter
                    output_idx = 0
                    while True:
                        output = c_void_p()
                        # IDXGIAdapter::EnumOutputs — vtable index 7
                        hr = _call_com(adapter, 7, [c_uint, POINTER(c_void_p)],
                                       c_uint(output_idx), byref(output))

                        if hr < 0 or not output:
                            break  # no more outputs

                        try:
                            # IDXGIOutput::GetDesc — vtable index 7
                            out_desc = DXGI_OUTPUT_DESC()
                            _call_com(output, 7, [POINTER(DXGI_OUTPUT_DESC)],
                                      byref(out_desc))

                            if not out_desc.AttachedToDesktop:
                                output_idx += 1
                                continue

                            rect = out_desc.DesktopCoordinates
                            width = rect.right - rect.left
                            height = rect.bottom - rect.top
                            dev_name = out_desc.DeviceName.rstrip("\x00")

                            # Get refresh rate via EnumDisplaySettings
                            refresh = self._get_refresh_rate(dev_name)

                            ao = _AdapterOutput(
                                adapter_index=adapter_idx,
                                adapter_name=adapter_name,
                                vendor_id=vendor_id,
                                vram_mb=vram_mb,
                                output_index=output_idx,
                                global_output_index=global_output_idx,
                                device_name=dev_name,
                                width=width,
                                height=height,
                                x=rect.left,
                                y=rect.top,
                                attached=True,
                                refresh_rate=refresh,
                            )
                            self._adapter_outputs.append(ao)

                            displays.append(DisplayInfo(
                                index=global_output_idx,
                                name=dev_name,
                                width=width,
                                height=height,
                                x_offset=rect.left,
                                y_offset=rect.top,
                                x_screen=f"{adapter_idx}:{output_idx}",
                                primary=(rect.left == 0 and rect.top == 0),
                            ))

                            global_output_idx += 1
                            output_idx += 1

                        finally:
                            # Release IDXGIOutput
                            _call_com(output, 2, [])  # IUnknown::Release

                finally:
                    # Release IDXGIAdapter1
                    _call_com(adapter, 2, [])  # IUnknown::Release

                adapter_idx += 1

        finally:
            # Release IDXGIFactory1
            if factory:
                _call_com(factory, 2, [])
            if needs_uninit:
                ole32.CoUninitialize()

        if displays:
            log.info("DXGI enumeration: %d displays across %d adapters",
                     len(displays), adapter_idx)
            for d in displays:
                ao = self._adapter_outputs[d.index] if d.index < len(self._adapter_outputs) else None
                extra = ""
                if ao:
                    extra = f" on {ao.adapter_name} (adapter {ao.adapter_index}, vram={ao.vram_mb}MB, {ao.refresh_rate}Hz)"
                log.info("  [%d] %s: %dx%d+%d+%d%s%s",
                         d.index, d.name, d.width, d.height,
                         d.x_offset, d.y_offset,
                         " (primary)" if d.primary else "", extra)

        return displays

    def _enumerate_user32(self) -> list[DisplayInfo]:
        """
        Fallback enumeration via EnumDisplayMonitors + GetMonitorInfo.

        Works on all Windows versions. Does not provide adapter info or
        correct dxcam output_idx mapping (indices are sequential).
        """
        if sys.platform != "win32":
            return []

        user32 = windll.user32
        displays: list[DisplayInfo] = []

        def callback(hmonitor, hdc, rect, data):
            info = MONITORINFOEX()
            info.cbSize = ctypes.sizeof(MONITORINFOEX)
            if user32.GetMonitorInfoW(hmonitor, byref(info)):
                r = info.rcMonitor
                dev_name = info.szDevice.rstrip("\x00")
                width = r.right - r.left
                height = r.bottom - r.top
                refresh = self._get_refresh_rate(dev_name)

                displays.append(DisplayInfo(
                    index=len(displays),
                    name=dev_name,
                    width=width,
                    height=height,
                    x_offset=r.left,
                    y_offset=r.top,
                    x_screen=str(len(displays)),
                    primary=bool(info.dwFlags & 1),  # MONITORINFOF_PRIMARY
                ))
            return True

        user32.EnumDisplayMonitors(
            None, None,
            MONITORENUMPROC(callback),
            0,
        )

        if displays:
            log.info("user32 enumeration: %d displays", len(displays))
            return displays

        return [self._default_display()]

    @staticmethod
    def _get_refresh_rate(device_name: str) -> int:
        """Query refresh rate via EnumDisplaySettingsW."""
        if sys.platform != "win32":
            return 60
        try:
            user32 = windll.user32
            dm = DEVMODE()
            dm.dmSize = ctypes.sizeof(DEVMODE)
            # ENUM_CURRENT_SETTINGS = -1
            if user32.EnumDisplaySettingsW(device_name, -1, byref(dm)):
                return dm.dmDisplayFrequency or 60
        except Exception:
            pass
        return 60

    @property
    def virtual_display_manager(self) -> VirtualDisplayManager:
        """Expose the VirtualDisplayManager for direct API access."""
        return self._vdm

    def create_virtual(self, width: int = 1920, height: int = 1080,
                       name: str = "") -> DisplayInfo | None:
        """
        Create a virtual display via the best available IDD driver.

        Uses VirtualDisplayManager which auto-detects ozma-vdd, Parsec VDD,
        or Amyuni. Returns the new DisplayInfo, or None if no driver is
        available (in which case dummy HDMI plugs are the fallback).
        """
        if not self._vdm.available:
            log.info("No virtual display driver available on Windows. "
                     "Install ozma-vdd, Parsec VDD, or Amyuni, or use a "
                     "dummy HDMI plug as a hardware workaround.")
            return None

        # Run the async add in a new event loop if we're not in one,
        # or schedule it if we are.
        try:
            loop = asyncio.get_running_loop()
            # We're inside an async context — caller should use the async API
            # directly. Create a future and warn.
            log.warning("create_virtual() called from async context — use "
                        "virtual_display_manager.add_monitor() directly for "
                        "proper async support")
            future = asyncio.ensure_future(
                self._vdm.add_monitor(width, height, 60))
            # Can't await here in a sync method; return None and log guidance.
            return None
        except RuntimeError:
            # No running loop — safe to run synchronously.
            loop = asyncio.new_event_loop()
            try:
                info = loop.run_until_complete(
                    self._vdm.add_monitor(width, height, 60))
                if info:
                    self._virtual_displays.append(info)
                return info
            finally:
                loop.close()

    def destroy_virtual(self, display: DisplayInfo) -> bool:
        """Remove a virtual display created by create_virtual()."""
        if not display.virtual:
            log.warning("Cannot destroy non-virtual display: %s", display.name)
            return False

        if not self._vdm.available:
            return False

        try:
            loop = asyncio.get_running_loop()
            log.warning("destroy_virtual() called from async context — use "
                        "virtual_display_manager.remove_by_display() directly")
            return False
        except RuntimeError:
            loop = asyncio.new_event_loop()
            try:
                ok = loop.run_until_complete(
                    self._vdm.remove_by_display(display))
                if ok:
                    self._virtual_displays = [
                        d for d in self._virtual_displays
                        if d.index != display.index
                    ]
                return ok
            finally:
                loop.close()

    def get_display_for_capture(self, display: DisplayInfo) -> dict:
        """
        Return capture parameters for a specific display.

        For DXGI Desktop Duplication (dxcam), returns adapter_index and
        output_index. For gdigrab fallback, returns crop coordinates.
        """
        # Try to find the adapter output info
        for ao in self._adapter_outputs:
            if ao.global_output_index == display.index:
                return {
                    "method": "dxgi",
                    "adapter_index": ao.adapter_index,
                    "output_index": ao.output_index,
                    "width": ao.width,
                    "height": ao.height,
                    "refresh_rate": ao.refresh_rate,
                }

        # Fallback: gdigrab with crop
        return {
            "method": "gdigrab",
            "offset_x": display.x_offset,
            "offset_y": display.y_offset,
            "width": display.width,
            "height": display.height,
        }

    def get_dxcam_indices(self, display: DisplayInfo) -> tuple[int, int]:
        """
        Return (device_idx, output_idx) for dxcam.create().

        dxcam uses adapter index as device_idx and the output index within
        that adapter as output_idx.
        """
        for ao in self._adapter_outputs:
            if ao.global_output_index == display.index:
                return (ao.adapter_index, ao.output_index)
        return (0, display.index)

    def _default_display(self) -> DisplayInfo:
        return DisplayInfo(
            index=0,
            name="DEFAULT",
            width=1920,
            height=1080,
            x_screen="0",
            primary=True,
        )
