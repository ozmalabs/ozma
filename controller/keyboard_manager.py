# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Programmable keyboard management — VIA, QMK, Keychron.

Detects VIA/QMK keyboards connected to the controller via USB HID,
reads their keymap configuration, and supports layer switching and
firmware flashing.

VIA protocol:
  VIA uses raw HID (usage page 0xFF60) with a simple command/response
  protocol.  Commands are 32-byte packets:
    Byte 0: command ID
    Bytes 1-31: payload

  Key commands:
    0x01  get_protocol_version    → [major, minor]
    0x04  get_keyboard_value      → device info
    0x05  set_keyboard_value      → set device config
    0x06  dynamic_keymap_get_keycode  → read a key from the keymap
    0x07  dynamic_keymap_set_keycode  → write a key to the keymap
    0x0B  dynamic_keymap_get_layer_count → number of layers
    0x11  lighting_get_value      → RGB config
    0x12  lighting_set_value      → set RGB

QMK DFU detection:
  QMK boards enter DFU mode for flashing (USB VID:PID changes).
  Common DFU VID/PIDs:
    03EB:2FF4  (Atmel DFU)
    0483:DF11  (STM32 DFU)
    1C11:B007  (QMK bootloader)
    239A:0035  (RP2040 UF2)

Known VIA-compatible vendor IDs:
  Keychron: 3434
  GMMK:    320F
  ZSA:     3297
  Drop:    04D8
  Ducky:   04D9
  KBDfans: 4B42

This module is read-only by default — keymap writes and flashing
require explicit user confirmation via the API.
"""

from __future__ import annotations

import asyncio
import logging
import re
import subprocess
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.keyboard_manager")

# VIA protocol constants
VIA_CMD_GET_PROTOCOL = 0x01
VIA_CMD_GET_KEYBOARD_VALUE = 0x04
VIA_CMD_SET_KEYBOARD_VALUE = 0x05
VIA_CMD_GET_KEYCODE = 0x06
VIA_CMD_SET_KEYCODE = 0x07
VIA_CMD_GET_LAYER_COUNT = 0x0B
VIA_CMD_LIGHTING_GET = 0x11
VIA_CMD_LIGHTING_SET = 0x12
VIA_PACKET_SIZE = 32

# Known VIA/QMK vendor IDs
_VIA_VENDORS: dict[str, str] = {
    "3434": "Keychron",
    "320f": "GMMK/Glorious",
    "3297": "ZSA (Ergodox/Moonlander)",
    "04d8": "Drop",
    "04d9": "Ducky/HID",
    "4b42": "KBDfans",
    "feed": "QMK Generic",
    "cb10": "Cannonkeys",
    "4653": "IDB Keyboards",
    "534b": "Skyloong",
}

# DFU mode VID:PIDs (bootloader detection)
_DFU_DEVICES = {
    ("03eb", "2ff4"): "Atmel DFU",
    ("0483", "df11"): "STM32 DFU",
    ("1c11", "b007"): "QMK Bootloader",
    ("239a", "0035"): "RP2040 UF2",
    ("2341", "0036"): "Arduino DFU",
}


@dataclass
class ManagedKeyboard:
    """A detected programmable keyboard."""

    path: str                       # HID device path
    vid: str                        # USB vendor ID
    pid: str                        # USB product ID
    manufacturer: str = ""
    product: str = ""
    vendor_name: str = ""           # Resolved from _VIA_VENDORS
    via_compatible: bool = False    # Speaks VIA protocol
    via_version: tuple[int, int] = (0, 0)
    layer_count: int = 0
    dfu_mode: bool = False          # In bootloader/flash mode
    dfu_type: str = ""

    def to_dict(self) -> dict[str, Any]:
        return {
            "path": self.path,
            "vid": self.vid,
            "pid": self.pid,
            "manufacturer": self.manufacturer,
            "product": self.product,
            "vendor_name": self.vendor_name,
            "via_compatible": self.via_compatible,
            "via_version": list(self.via_version),
            "layer_count": self.layer_count,
            "dfu_mode": self.dfu_mode,
            "dfu_type": self.dfu_type,
        }


class KeyboardManager:
    """
    Detects and manages VIA/QMK programmable keyboards.

    Scans USB devices for known VIA vendors and DFU bootloaders.
    Can read keymap info from VIA-compatible boards.
    """

    def __init__(self) -> None:
        self._keyboards: dict[str, ManagedKeyboard] = {}
        self._scan_task: asyncio.Task | None = None

    async def start(self) -> None:
        await self._scan()
        self._scan_task = asyncio.create_task(self._scan_loop(), name="keyboard-scan")

    async def stop(self) -> None:
        if self._scan_task:
            self._scan_task.cancel()

    def list_keyboards(self) -> list[dict[str, Any]]:
        return [k.to_dict() for k in self._keyboards.values()]

    def get_keyboard(self, vid_pid: str) -> ManagedKeyboard | None:
        return self._keyboards.get(vid_pid)

    # ── Detection ────────────────────────────────────────────────────────────

    async def _scan_loop(self) -> None:
        while True:
            try:
                await asyncio.sleep(10.0)
                await self._scan()
            except asyncio.CancelledError:
                return

    async def _scan(self) -> None:
        """Scan USB devices for VIA/QMK keyboards and DFU bootloaders."""
        usb_root = Path("/sys/bus/usb/devices")
        if not usb_root.exists():
            return

        found: dict[str, ManagedKeyboard] = {}

        for entry in sorted(usb_root.iterdir()):
            if entry.name.startswith("usb") or ":" in entry.name:
                continue

            def _r(field: str) -> str:
                try:
                    return (entry / field).read_text().strip()
                except OSError:
                    return ""

            vid = _r("idVendor").lower()
            pid = _r("idProduct").lower()
            if not vid:
                continue

            key = f"{vid}:{pid}"

            # Check for DFU bootloader
            if (vid, pid) in _DFU_DEVICES:
                found[key] = ManagedKeyboard(
                    path=str(entry),
                    vid=vid, pid=pid,
                    manufacturer=_r("manufacturer"),
                    product=_r("product") or "Keyboard in DFU mode",
                    vendor_name=_DFU_DEVICES[(vid, pid)],
                    dfu_mode=True,
                    dfu_type=_DFU_DEVICES[(vid, pid)],
                )
                continue

            # Check for VIA-compatible vendor
            if vid in _VIA_VENDORS:
                kb = ManagedKeyboard(
                    path=str(entry),
                    vid=vid, pid=pid,
                    manufacturer=_r("manufacturer"),
                    product=_r("product"),
                    vendor_name=_VIA_VENDORS[vid],
                    via_compatible=True,
                )
                found[key] = kb

        # Detect new/removed keyboards
        for key, kb in found.items():
            if key not in self._keyboards:
                log.info("Programmable keyboard detected: %s %s (%s) [%s]",
                         kb.vendor_name, kb.product, key,
                         "DFU" if kb.dfu_mode else "VIA")
        for key in list(self._keyboards):
            if key not in found:
                old = self._keyboards[key]
                log.info("Keyboard removed: %s %s", old.vendor_name, old.product)

        self._keyboards = found

    # ── VIA protocol ─────────────────────────────────────────────────────────

    async def via_get_protocol_version(self, vid_pid: str) -> tuple[int, int] | None:
        """Read VIA protocol version from a keyboard."""
        resp = await self._via_command(vid_pid, VIA_CMD_GET_PROTOCOL)
        if resp and len(resp) >= 3:
            return (resp[1], resp[2])
        return None

    async def via_get_layer_count(self, vid_pid: str) -> int | None:
        """Read the number of keymap layers."""
        resp = await self._via_command(vid_pid, VIA_CMD_GET_LAYER_COUNT)
        if resp and len(resp) >= 2:
            return resp[1]
        return None

    async def _via_command(self, vid_pid: str, cmd: int, payload: bytes = b"") -> bytes | None:
        """Send a VIA command and read the response via hidraw."""
        kb = self._keyboards.get(vid_pid)
        if not kb or kb.dfu_mode:
            return None

        # Find the hidraw device for this keyboard
        hidraw = self._find_hidraw(kb)
        if not hidraw:
            log.debug("No hidraw device for %s", vid_pid)
            return None

        packet = bytearray(VIA_PACKET_SIZE)
        packet[0] = cmd
        for i, b in enumerate(payload[:VIA_PACKET_SIZE - 1]):
            packet[i + 1] = b

        try:
            loop = asyncio.get_running_loop()
            def _io():
                with open(hidraw, "r+b", buffering=0) as f:
                    f.write(bytes(packet))
                    return f.read(VIA_PACKET_SIZE)
            return await asyncio.wait_for(
                loop.run_in_executor(None, _io), timeout=2.0
            )
        except Exception as e:
            log.debug("VIA command failed on %s: %s", vid_pid, e)
            return None

    def _find_hidraw(self, kb: ManagedKeyboard) -> str | None:
        """Find the hidraw device path for a keyboard."""
        # Walk sysfs to find hidraw associated with this USB device
        for hidraw in sorted(Path("/sys/class/hidraw").glob("hidraw*")):
            try:
                device_link = (hidraw / "device").resolve()
                # Check if the USB parent matches
                uevent = (device_link / "uevent").read_text()
                if f"HID_ID=" in uevent and kb.vid.upper() in uevent.upper():
                    return f"/dev/{hidraw.name}"
            except (OSError, ValueError):
                continue
        return None

    # ── QMK flash detection ──────────────────────────────────────────────────

    async def flash_firmware(self, vid_pid: str, firmware_path: str) -> dict[str, Any]:
        """
        Flash QMK firmware to a keyboard in DFU mode.

        The keyboard must be in bootloader mode (DFU). This is typically
        triggered by holding a physical reset button or a key combo.

        Returns: {"ok": bool, "output": str}
        """
        kb = self._keyboards.get(vid_pid)
        if not kb or not kb.dfu_mode:
            return {"ok": False, "error": "Keyboard not in DFU mode"}

        if not Path(firmware_path).exists():
            return {"ok": False, "error": f"Firmware file not found: {firmware_path}"}

        # Select flash tool based on DFU type
        cmd: list[str] = []
        match kb.dfu_type:
            case "STM32 DFU":
                cmd = ["dfu-util", "-D", firmware_path, "-a", "0", "-s", "0x08000000:leave"]
            case "Atmel DFU":
                cmd = ["dfu-programmer", "atmega32u4", "flash", firmware_path]
            case "RP2040 UF2":
                # RP2040 appears as a USB mass storage device — just copy the .uf2 file
                # Find the mount point
                return {"ok": False, "error": "RP2040 UF2: copy firmware to the mounted drive"}
            case _:
                cmd = ["dfu-util", "-D", firmware_path]

        try:
            proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            stdout, stderr = await asyncio.wait_for(proc.communicate(), timeout=60.0)
            output = stdout.decode() + stderr.decode()
            ok = proc.returncode == 0
            if ok:
                log.info("Firmware flashed to %s: %s", vid_pid, firmware_path)
            else:
                log.warning("Flash failed on %s: %s", vid_pid, output[:200])
            return {"ok": ok, "output": output}
        except FileNotFoundError:
            return {"ok": False, "error": f"Flash tool not found ({cmd[0]})"}
        except asyncio.TimeoutError:
            return {"ok": False, "error": "Flash timed out (60s)"}
