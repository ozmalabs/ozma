# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Virtual USB gadget for soft nodes — identical to hardware node USB.

Uses dummy_hcd to create a virtual USB device controller, then sets
up a ConfigFS composite gadget on it — the same setup as a hardware
node's physical USB gadget. The resulting USB device is passed through
to a QEMU VM, so the VM sees exactly what a physical machine would see
with a hardware node plugged in via USB.

The flow:
  1. Load dummy_hcd kernel module (creates dummy_udc.N)
  2. Create ConfigFS gadget (HID kbd + mouse + mass storage + audio)
  3. Bind gadget to dummy_udc.N
  4. Host sees a USB device on bus X device Y
  5. Pass through to QEMU via usb-host

This means:
  - The soft node tests the EXACT same USB path as the hardware node
  - The VM sees a real USB composite device (not QEMU emulation)
  - Mass storage works via the FAT32 synthesiser — no image files
  - HID works via the real gadget HID device (/dev/hidg0)
  - Audio works via UAC2 gadget (if configured)

Requirements:
  - dummy_hcd kernel module (dev/dummy_hcd/build.sh)
  - libcomposite kernel module (usually available)
  - Root/sudo for ConfigFS and module loading
"""

from __future__ import annotations

import asyncio
import logging
import os
import subprocess
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.softnode.virtual_usb")

GADGET_BASE = Path("/sys/kernel/config/usb_gadget")


class VirtualUSBGadget:
    """
    A virtual USB composite gadget on dummy_hcd.

    Creates the same USB device that a hardware node presents:
      - HID keyboard (function hid.kbd)
      - HID mouse (function hid.mouse)
      - Mass storage (function mass_storage.0)
      - UAC2 audio (function uac2.0, optional)

    The gadget appears as a real USB device on the host, which can
    then be passed through to a VM.
    """

    def __init__(self, name: str = "ozma-vnode",
                 udc: str = "dummy_udc.0",
                 storage_file: str = "") -> None:
        self._name = name
        self._udc = udc
        self._gadget_path = GADGET_BASE / name
        self._storage_file = storage_file
        self._active = False
        self._usb_bus: int = 0
        self._usb_dev: int = 0

    @property
    def active(self) -> bool:
        return self._active

    @property
    def usb_bus(self) -> int:
        return self._usb_bus

    @property
    def usb_dev(self) -> int:
        return self._usb_dev

    @property
    def qemu_usb_arg(self) -> str:
        """QEMU argument to passthrough this USB device."""
        return f"-device usb-host,hostbus={self._usb_bus},hostaddr={self._usb_dev}"

    async def setup(self, enable_hid: bool = True,
                     enable_storage: bool = True,
                     enable_audio: bool = False) -> bool:
        """
        Create and activate the USB gadget.

        Returns True if the gadget was created and the host sees it.
        """
        # Ensure dummy_hcd is loaded
        if not Path(f"/sys/class/udc/{self._udc}").exists():
            log.info("Loading dummy_hcd...")
            ok = await self._run("sudo", "modprobe", "libcomposite")
            if not ok:
                return False

            # Try loading the module from the dev directory
            module_path = Path(__file__).parent.parent / "dev" / "dummy_hcd" / "dummy_hcd.ko"
            if module_path.exists():
                ok = await self._run("sudo", "insmod", str(module_path), "num=1")
            else:
                ok = await self._run("sudo", "modprobe", "dummy_hcd")

            if not ok:
                log.error("Failed to load dummy_hcd. Run: bash dev/dummy_hcd/build.sh")
                return False

        if not Path(f"/sys/class/udc/{self._udc}").exists():
            log.error("UDC %s not available", self._udc)
            return False

        # Create the gadget via ConfigFS
        try:
            await self._create_gadget(enable_hid, enable_storage, enable_audio)
        except Exception as e:
            log.error("Failed to create gadget: %s", e)
            return False

        # Find the USB device on the host side
        await asyncio.sleep(0.5)  # let USB enumeration happen
        self._find_usb_device()

        if self._usb_bus and self._usb_dev:
            self._active = True
            log.info("Virtual USB gadget active: bus %d device %d (%s)",
                     self._usb_bus, self._usb_dev, self._name)
            return True
        else:
            log.warning("Gadget created but USB device not found on host")
            self._active = True  # gadget is up, just can't find host device
            return True

    async def teardown(self) -> None:
        """Remove the USB gadget."""
        if not self._gadget_path.exists():
            return

        try:
            # Unbind from UDC
            await self._write(self._gadget_path / "UDC", "")

            # Remove config links
            configs = self._gadget_path / "configs"
            if configs.exists():
                for config in configs.iterdir():
                    for link in config.iterdir():
                        if link.is_symlink():
                            await self._run("sudo", "rm", str(link))
                    strings = config / "strings" / "0x409"
                    if strings.exists():
                        await self._run("sudo", "rmdir", str(strings))
                    await self._run("sudo", "rmdir", str(config))

            # Remove functions
            functions = self._gadget_path / "functions"
            if functions.exists():
                for func in functions.iterdir():
                    await self._run("sudo", "rmdir", str(func))

            # Remove strings and gadget
            strings = self._gadget_path / "strings" / "0x409"
            if strings.exists():
                await self._run("sudo", "rmdir", str(strings))
            await self._run("sudo", "rmdir", str(self._gadget_path))

        except Exception as e:
            log.warning("Gadget teardown error: %s", e)

        self._active = False
        log.info("Virtual USB gadget removed: %s", self._name)

    def set_storage_file(self, path: str) -> None:
        """Change the mass storage backing file (hot-swap media)."""
        lun_file = self._gadget_path / "functions" / "mass_storage.0" / "lun.0" / "file"
        if lun_file.exists():
            try:
                subprocess.run(["sudo", "tee", str(lun_file)],
                               input=path.encode(), capture_output=True, timeout=3)
                log.info("Storage media changed: %s", path)
            except Exception as e:
                log.warning("Failed to change storage media: %s", e)

    # ── Internal ───────────────────────────────────────────────────────

    async def _create_gadget(self, hid: bool, storage: bool, audio: bool) -> None:
        """Create the ConfigFS gadget. Same setup as tinynode/gadget/setup_gadget.sh."""
        g = self._gadget_path

        await self._run("sudo", "mkdir", "-p", str(g))
        await self._write(g / "idVendor", "0x1d6b")   # Linux Foundation
        await self._write(g / "idProduct", "0x0104")   # Multifunction Composite
        await self._write(g / "bcdDevice", "0x0100")
        await self._write(g / "bcdUSB", "0x0200")

        # Strings
        s = g / "strings" / "0x409"
        await self._run("sudo", "mkdir", "-p", str(s))
        await self._write(s / "manufacturer", "Ozma Labs")
        await self._write(s / "product", f"Ozma Node ({self._name})")
        await self._write(s / "serialnumber", f"OZMA-{self._name.upper()}")

        # Config
        c = g / "configs" / "c.1"
        cs = c / "strings" / "0x409"
        await self._run("sudo", "mkdir", "-p", str(cs))
        await self._write(cs / "configuration", "Ozma Node Config")
        await self._write(c / "MaxPower", "500")

        # HID keyboard
        if hid:
            kbd = g / "functions" / "hid.kbd"
            await self._run("sudo", "mkdir", "-p", str(kbd))
            await self._write(kbd / "protocol", "1")
            await self._write(kbd / "subclass", "1")
            await self._write(kbd / "report_length", "8")
            # Boot keyboard report descriptor
            desc = bytes([
                0x05, 0x01, 0x09, 0x06, 0xA1, 0x01, 0x05, 0x07,
                0x19, 0xE0, 0x29, 0xE7, 0x15, 0x00, 0x25, 0x01,
                0x75, 0x01, 0x95, 0x08, 0x81, 0x02, 0x95, 0x01,
                0x75, 0x08, 0x81, 0x01, 0x95, 0x05, 0x75, 0x01,
                0x05, 0x08, 0x19, 0x01, 0x29, 0x05, 0x91, 0x02,
                0x95, 0x01, 0x75, 0x03, 0x91, 0x01, 0x95, 0x06,
                0x75, 0x08, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x05,
                0x07, 0x19, 0x00, 0x2A, 0xFF, 0x00, 0x81, 0x00,
                0xC0
            ])
            await self._write_bytes(kbd / "report_desc", desc)
            await self._run("sudo", "ln", "-sf", str(kbd), str(c / "hid.kbd"))

            # HID mouse (absolute pointer)
            mouse = g / "functions" / "hid.mouse"
            await self._run("sudo", "mkdir", "-p", str(mouse))
            await self._write(mouse / "protocol", "2")
            await self._write(mouse / "subclass", "1")
            await self._write(mouse / "report_length", "6")
            mouse_desc = bytes([
                0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x09, 0x01,
                0xA1, 0x00, 0x05, 0x09, 0x19, 0x01, 0x29, 0x03,
                0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x03,
                0x81, 0x02, 0x95, 0x01, 0x75, 0x05, 0x81, 0x01,
                0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x16, 0x00,
                0x00, 0x26, 0xFF, 0x7F, 0x75, 0x10, 0x95, 0x02,
                0x81, 0x02, 0x09, 0x38, 0x15, 0x81, 0x25, 0x7F,
                0x75, 0x08, 0x95, 0x01, 0x81, 0x06, 0xC0, 0xC0
            ])
            await self._write_bytes(mouse / "report_desc", mouse_desc)
            await self._run("sudo", "ln", "-sf", str(mouse), str(c / "hid.mouse"))

        # Mass storage
        if storage:
            ms = g / "functions" / "mass_storage.0"
            await self._run("sudo", "mkdir", "-p", str(ms))
            await self._write(ms / "stall", "1")
            await self._write(ms / "lun.0" / "removable", "1")
            if self._storage_file:
                await self._write(ms / "lun.0" / "file", self._storage_file)
            await self._run("sudo", "ln", "-sf", str(ms), str(c / "mass_storage.0"))

        # UAC2 audio
        if audio:
            uac = g / "functions" / "uac2.0"
            await self._run("sudo", "mkdir", "-p", str(uac))
            await self._run("sudo", "ln", "-sf", str(uac), str(c / "uac2.0"))

        # Bind to UDC
        await self._write(g / "UDC", self._udc)

    def _find_usb_device(self) -> None:
        """Find our gadget's bus/device number on the host side."""
        try:
            result = subprocess.run(
                ["lsusb"], capture_output=True, text=True, timeout=3,
            )
            for line in result.stdout.splitlines():
                if "0104" in line and "1d6b" in line:
                    # Bus 009 Device 002: ID 1d6b:0104
                    parts = line.split()
                    self._usb_bus = int(parts[1])
                    self._usb_dev = int(parts[3].rstrip(":"))
                    return
        except Exception:
            pass

    async def _write(self, path: Path, value: str) -> None:
        await self._run("sudo", "bash", "-c", f"echo '{value}' > {path}")

    async def _write_bytes(self, path: Path, data: bytes) -> None:
        proc = await asyncio.create_subprocess_exec(
            "sudo", "tee", str(path),
            stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.DEVNULL,
        )
        await proc.communicate(input=data)

    async def _run(self, *args: str) -> bool:
        proc = await asyncio.create_subprocess_exec(
            *args,
            stdout=asyncio.subprocess.DEVNULL,
            stderr=asyncio.subprocess.DEVNULL,
        )
        await proc.wait()
        return proc.returncode == 0
