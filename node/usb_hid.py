# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
USB HID gadget management.

Manages /dev/hidg0 (keyboard) and /dev/hidg1 (mouse) created by the
USB gadget ConfigFS setup (tinynode/gadget/setup_gadget.sh).

Can optionally run the setup script automatically if the devices are absent
and the script is available.

Report formats (must match the HID descriptors in setup_gadget.sh):
  Keyboard : 8 bytes  [modifier, reserved, key1..key6]
  Mouse    : 6 bytes  [buttons, x_lo, x_hi, y_lo, y_hi, scroll]
"""

from __future__ import annotations

import asyncio
import logging
import os
import subprocess
from pathlib import Path

log = logging.getLogger("ozma.node.usb_hid")

KBD_REPORT_LEN = 8
MOUSE_REPORT_LEN = 6

# Path to the gadget setup script relative to this file's location
_GADGET_SCRIPT = Path(__file__).parent.parent / "tinynode" / "gadget" / "setup_gadget.sh"


class USBHIDGadget:
    """
    Wraps /dev/hidg0 and /dev/hidg1 for writing HID boot reports.

    Usage:
        async with USBHIDGadget.open() as gadget:
            await gadget.write_keyboard(report_bytes)
            await gadget.write_mouse(report_bytes)
    """

    def __init__(self, kbd_path: str = "/dev/hidg0", mouse_path: str = "/dev/hidg1") -> None:
        self._kbd_path = kbd_path
        self._mouse_path = mouse_path
        self._kbd_fd: int | None = None
        self._mouse_fd: int | None = None
        self._lock = asyncio.Lock()

    @classmethod
    async def open(
        cls,
        kbd_path: str = "/dev/hidg0",
        mouse_path: str = "/dev/hidg1",
        auto_setup: bool = True,
    ) -> "USBHIDGadget":
        gadget = cls(kbd_path, mouse_path)
        await gadget._ensure_devices(auto_setup)
        gadget._open_fds()
        return gadget

    async def close(self) -> None:
        if self._kbd_fd is not None:
            try:
                os.close(self._kbd_fd)
            except OSError:
                pass
            self._kbd_fd = None
        if self._mouse_fd is not None:
            try:
                os.close(self._mouse_fd)
            except OSError:
                pass
            self._mouse_fd = None

    async def __aenter__(self) -> "USBHIDGadget":
        return self

    async def __aexit__(self, *_) -> None:
        await self.close()

    # ── write ─────────────────────────────────────────────────────────────────

    async def write_keyboard(self, report: bytes) -> None:
        if len(report) != KBD_REPORT_LEN:
            raise ValueError(f"Keyboard report must be {KBD_REPORT_LEN} bytes, got {len(report)}")
        if self._kbd_fd is None:
            return
        async with self._lock:
            await asyncio.get_running_loop().run_in_executor(
                None, os.write, self._kbd_fd, report
            )

    async def write_mouse(self, report: bytes) -> None:
        if len(report) != MOUSE_REPORT_LEN:
            raise ValueError(f"Mouse report must be {MOUSE_REPORT_LEN} bytes, got {len(report)}")
        if self._mouse_fd is None:
            return
        async with self._lock:
            await asyncio.get_running_loop().run_in_executor(
                None, os.write, self._mouse_fd, report
            )

    # ── convenience constructors ──────────────────────────────────────────────

    @staticmethod
    def keyboard_report(
        modifiers: int = 0,
        keys: list[int] | None = None,
    ) -> bytes:
        """Build a boot-protocol keyboard HID report."""
        slots = (keys or []) + [0] * 6
        return bytes([modifiers, 0x00] + slots[:6])

    @staticmethod
    def mouse_report(
        buttons: int = 0,
        x: int = 0,
        y: int = 0,
        scroll: int = 0,
    ) -> bytes:
        """
        Build an absolute mouse HID report.
        x, y: 0–32767 absolute position.
        scroll: signed, −127–+127.
        """
        x = max(0, min(0x7FFF, x))
        y = max(0, min(0x7FFF, y))
        scroll_byte = scroll & 0xFF
        return bytes([
            buttons & 0xFF,
            x & 0xFF, (x >> 8) & 0xFF,
            y & 0xFF, (y >> 8) & 0xFF,
            scroll_byte,
        ])

    # ── internal ──────────────────────────────────────────────────────────────

    async def _ensure_devices(self, auto_setup: bool) -> None:
        kbd_ok = Path(self._kbd_path).exists()
        mouse_ok = Path(self._mouse_path).exists()
        if kbd_ok and mouse_ok:
            return

        if not auto_setup:
            missing = [p for p in (self._kbd_path, self._mouse_path) if not Path(p).exists()]
            raise FileNotFoundError(f"HID gadget devices not found: {missing}")

        if not _GADGET_SCRIPT.exists():
            raise FileNotFoundError(
                f"Gadget setup script not found at {_GADGET_SCRIPT}. "
                "Run tinynode/gadget/setup_gadget.sh manually as root."
            )

        log.info("HID gadget devices absent — running setup script (requires root)")
        try:
            result = await asyncio.get_running_loop().run_in_executor(
                None,
                lambda: subprocess.run(
                    ["sudo", str(_GADGET_SCRIPT)],
                    capture_output=True, text=True, timeout=30,
                ),
            )
            if result.returncode != 0:
                raise RuntimeError(
                    f"Gadget setup failed (rc={result.returncode}):\n{result.stderr}"
                )
            log.info("Gadget setup complete:\n%s", result.stdout.strip())
        except subprocess.TimeoutExpired:
            raise RuntimeError("Gadget setup script timed out")

        # Wait up to 3 s for the device files to appear
        for _ in range(30):
            if Path(self._kbd_path).exists() and Path(self._mouse_path).exists():
                return
            await asyncio.sleep(0.1)
        raise RuntimeError(
            f"Gadget setup ran but devices still absent: "
            f"{self._kbd_path}, {self._mouse_path}"
        )

    def _open_fds(self) -> None:
        try:
            self._kbd_fd = os.open(self._kbd_path, os.O_WRONLY | os.O_NONBLOCK)
            log.info("Opened keyboard gadget: %s", self._kbd_path)
        except OSError as e:
            log.error("Cannot open keyboard gadget %s: %s", self._kbd_path, e)

        try:
            self._mouse_fd = os.open(self._mouse_path, os.O_WRONLY | os.O_NONBLOCK)
            log.info("Opened mouse gadget: %s", self._mouse_path)
        except OSError as e:
            log.error("Cannot open mouse gadget %s: %s", self._mouse_path, e)
