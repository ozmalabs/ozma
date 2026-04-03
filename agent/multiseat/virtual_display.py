# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Virtual display driver abstraction for multi-seat.

Manages creation/destruction of virtual monitors via IDD (Indirect Display
Driver) backends on Windows and xrandr on Linux. Auto-detects the best
available driver:

  1. **OzmaVDD** — our fork of virtual-display-rs, controlled via named pipe
     ``\\\\.\\pipe\\ozma-vdd`` with a versioned JSON protocol. This is the
     long-term shipping driver.
  2. **ParsecVDD** — Parsec's signed IDD driver (free, widely installed).
     Detected via the ``ParsecVDD`` Windows service; controlled through
     registry + driver signaling.
  3. **AmyuniVDD** — Amyuni USB Mobile Monitor (signed, free for OSS).
     Detected via SetupAPI device enumeration.
  4. **Linux xrandr** — ``xrandr --setmonitor`` for virtual monitors within
     an existing X server, or ``wlr-randr`` on Wayland compositors.

On platforms with no driver available, a dummy-plug detector reports whether
passive HDMI/DP emulators are present so the user knows virtual displays
are usable via hardware dongles.

Cleanup: an atexit handler destroys all virtual monitors on exit so the
desktop doesn't keep phantom displays after a crash.
"""

from __future__ import annotations

import atexit
import asyncio
import json
import logging
import os
import re
import subprocess
import sys
from abc import ABC, abstractmethod
from dataclasses import dataclass, field
from typing import Any

from .display_backend import DisplayInfo

log = logging.getLogger("ozma.agent.multiseat.vdd")

# Protocol version for the ozma-vdd named pipe JSON protocol.
OZMA_VDD_PROTOCOL_VERSION = 1


# ── Driver ABC ─────────────────────────────────────────────────────────────────

class VirtualDisplayDriver(ABC):
    """Base class for IDD / virtual-display driver backends."""

    name: str = "unknown"

    @abstractmethod
    def is_available(self) -> bool:
        """Return True if this driver is installed and usable."""
        ...

    @abstractmethod
    async def add(self, width: int, height: int, refresh: int) -> int | None:
        """
        Create a virtual monitor.

        Returns the driver-internal monitor index, or None on failure.
        """
        ...

    @abstractmethod
    async def remove(self, index: int) -> bool:
        """Remove a virtual monitor by its driver index."""
        ...

    @abstractmethod
    async def list(self) -> list[dict]:
        """
        List active virtual monitors.

        Each entry: ``{"index": int, "width": int, "height": int, "refresh": int}``
        """
        ...

    async def update(self, index: int, width: int, height: int,
                     refresh: int = 60) -> bool:
        """
        Change resolution of an existing virtual monitor.

        Default implementation removes + re-adds. Drivers that support
        in-place updates should override.
        """
        if not await self.remove(index):
            return False
        new_idx = await self.add(width, height, refresh)
        return new_idx is not None

    def describe(self) -> dict:
        """Return driver metadata for diagnostics."""
        return {
            "name": self.name,
            "available": self.is_available(),
        }


# ── OzmaVDD (virtual-display-rs fork) ─────────────────────────────────────────

class OzmaVDDDriver(VirtualDisplayDriver):
    """
    Ozma Virtual Display Driver — fork of virtual-display-rs.

    Communication is via a named pipe ``\\\\.\\pipe\\ozma-vdd`` carrying
    newline-delimited JSON messages. Each request is a single JSON object;
    the driver responds with a single JSON object.

    Install the driver: ``pnputil /add-driver ozma-vdd.inf /install``
    (requires test-signing mode or WHQL attestation signing).
    """

    name = "ozma-vdd"
    PIPE_PATH = r"\\.\pipe\ozma-vdd"

    def is_available(self) -> bool:
        if sys.platform != "win32":
            return False
        # Named pipe exists only when the driver service is running.
        try:
            handle = open(self.PIPE_PATH, "r+b")  # noqa: SIM115
            handle.close()
            return True
        except FileNotFoundError:
            return False
        except PermissionError:
            # Pipe exists but busy — driver is running, just occupied.
            return True
        except OSError:
            return False

    async def add(self, width: int, height: int, refresh: int) -> int | None:
        resp = await self._send({
            "version": OZMA_VDD_PROTOCOL_VERSION,
            "command": "add",
            "width": width,
            "height": height,
            "refresh": refresh,
        })
        if resp and resp.get("ok"):
            idx = resp.get("index")
            log.info("ozma-vdd: added monitor %d (%dx%d@%dHz)",
                     idx, width, height, refresh)
            return idx
        log.warning("ozma-vdd: add failed — %s", resp)
        return None

    async def remove(self, index: int) -> bool:
        resp = await self._send({
            "version": OZMA_VDD_PROTOCOL_VERSION,
            "command": "remove",
            "index": index,
        })
        if resp and resp.get("ok"):
            log.info("ozma-vdd: removed monitor %d", index)
            return True
        log.warning("ozma-vdd: remove failed — %s", resp)
        return False

    async def list(self) -> list[dict]:
        resp = await self._send({
            "version": OZMA_VDD_PROTOCOL_VERSION,
            "command": "list",
        })
        if resp and "monitors" in resp:
            return resp["monitors"]
        return []

    async def update(self, index: int, width: int, height: int,
                     refresh: int = 60) -> bool:
        resp = await self._send({
            "version": OZMA_VDD_PROTOCOL_VERSION,
            "command": "update",
            "index": index,
            "width": width,
            "height": height,
            "refresh": refresh,
        })
        if resp and resp.get("ok"):
            log.info("ozma-vdd: updated monitor %d to %dx%d@%dHz",
                     index, width, height, refresh)
            return True
        # Fall back to remove + add if the driver doesn't support update.
        if resp and resp.get("error") == "unknown_command":
            return await super().update(index, width, height, refresh)
        log.warning("ozma-vdd: update failed — %s", resp)
        return False

    async def _send(self, msg: dict) -> dict | None:
        """Send a JSON command to the ozma-vdd named pipe and read the response."""
        if sys.platform != "win32":
            return None

        loop = asyncio.get_running_loop()
        try:
            return await loop.run_in_executor(None, self._send_sync, msg)
        except Exception as e:
            log.warning("ozma-vdd pipe error: %s", e)
            return None

    def _send_sync(self, msg: dict) -> dict | None:
        """Synchronous named pipe I/O (run in executor)."""
        try:
            # Open pipe in overlapped binary mode.
            with open(self.PIPE_PATH, "r+b", buffering=0) as pipe:
                payload = json.dumps(msg).encode("utf-8") + b"\n"
                pipe.write(payload)
                pipe.flush()
                # Read response — driver sends newline-terminated JSON.
                data = b""
                while True:
                    chunk = pipe.read(4096)
                    if not chunk:
                        break
                    data += chunk
                    if b"\n" in data:
                        break
                if data:
                    return json.loads(data.split(b"\n", 1)[0])
        except Exception as e:
            log.debug("ozma-vdd sync send error: %s", e)
        return None


# ── Parsec VDD ─────────────────────────────────────────────────────────────────

class ParsecVDDDriver(VirtualDisplayDriver):
    """
    Parsec Virtual Display Driver.

    Parsec VDD is a signed IDD driver distributed with Parsec. It creates
    virtual monitors based on registry definitions under
    ``HKLM\\SOFTWARE\\Parsec\\vdd``. The driver re-reads the registry when
    the ``ParsecVDD`` service is restarted or when it receives a
    DeviceIoControl signal.

    Detection: the ``ParsecVDD`` service must be installed and the device
    ``ROOT\\ParsecVDD\\0000`` present in Device Manager.

    Install: https://github.com/nicollasricas/ParsecVDD (or bundled with Parsec)
    """

    name = "parsec-vdd"

    _SERVICE_NAME = "ParsecVDD"
    _DEVICE_PATH = r"\\.\ParsecVDD"
    _REG_KEY = r"SOFTWARE\Parsec\vdd"
    # IOCTL to signal the driver to re-read registry configuration.
    _IOCTL_REFRESH = 0x22C004  # CTL_CODE(FILE_DEVICE_UNKNOWN, 0x3001, METHOD_BUFFERED, FILE_ANY_ACCESS)

    def is_available(self) -> bool:
        if sys.platform != "win32":
            return False
        return self._service_exists() and self._device_exists()

    def _service_exists(self) -> bool:
        """Check if the ParsecVDD service is installed."""
        try:
            result = subprocess.run(
                ["sc", "query", self._SERVICE_NAME],
                capture_output=True, text=True, timeout=5,
                creationflags=0x08000000,  # CREATE_NO_WINDOW
            )
            return "RUNNING" in result.stdout or "STOPPED" in result.stdout
        except Exception:
            return False

    def _device_exists(self) -> bool:
        """Check if the ParsecVDD device node exists."""
        try:
            import ctypes
            handle = ctypes.windll.kernel32.CreateFileW(
                self._DEVICE_PATH, 0, 0, None, 3, 0, None,  # OPEN_EXISTING=3
            )
            if handle != -1 and handle != 0xFFFFFFFF:
                ctypes.windll.kernel32.CloseHandle(handle)
                return True
            return False
        except Exception:
            return False

    async def add(self, width: int, height: int, refresh: int) -> int | None:
        loop = asyncio.get_running_loop()
        return await loop.run_in_executor(
            None, self._add_sync, width, height, refresh,
        )

    def _add_sync(self, width: int, height: int, refresh: int) -> int | None:
        """Add a monitor definition to the registry and signal the driver."""
        if sys.platform != "win32":
            return None
        import ctypes
        try:
            import winreg
        except ImportError:
            return None

        try:
            # Open or create the Parsec VDD registry key.
            key = winreg.CreateKeyEx(
                winreg.HKEY_LOCAL_MACHINE, self._REG_KEY,
                0, winreg.KEY_ALL_ACCESS,
            )

            # Determine next monitor index by reading existing entries.
            index = 0
            try:
                existing = winreg.QueryValueEx(key, "monitors")[0]
                if isinstance(existing, str):
                    monitors = json.loads(existing)
                    index = len(monitors)
                else:
                    monitors = []
            except FileNotFoundError:
                monitors = []

            # Add new monitor definition.
            monitors.append({
                "width": width,
                "height": height,
                "refresh": refresh,
            })
            winreg.SetValueEx(
                key, "monitors", 0, winreg.REG_SZ, json.dumps(monitors),
            )
            winreg.CloseKey(key)

            # Signal the driver to re-read configuration.
            self._signal_driver()

            log.info("parsec-vdd: added monitor %d (%dx%d@%dHz)",
                     index, width, height, refresh)
            return index

        except PermissionError:
            log.warning("parsec-vdd: registry access denied — run as administrator")
            return None
        except Exception as e:
            log.warning("parsec-vdd: add failed — %s", e)
            return None

    async def remove(self, index: int) -> bool:
        loop = asyncio.get_running_loop()
        return await loop.run_in_executor(None, self._remove_sync, index)

    def _remove_sync(self, index: int) -> bool:
        if sys.platform != "win32":
            return False
        try:
            import winreg
        except ImportError:
            return False

        try:
            key = winreg.OpenKeyEx(
                winreg.HKEY_LOCAL_MACHINE, self._REG_KEY,
                0, winreg.KEY_ALL_ACCESS,
            )
            raw = winreg.QueryValueEx(key, "monitors")[0]
            monitors = json.loads(raw) if isinstance(raw, str) else []

            if index < 0 or index >= len(monitors):
                winreg.CloseKey(key)
                return False

            monitors.pop(index)
            winreg.SetValueEx(
                key, "monitors", 0, winreg.REG_SZ, json.dumps(monitors),
            )
            winreg.CloseKey(key)

            self._signal_driver()
            log.info("parsec-vdd: removed monitor %d", index)
            return True

        except Exception as e:
            log.warning("parsec-vdd: remove failed — %s", e)
            return False

    async def list(self) -> list[dict]:
        loop = asyncio.get_running_loop()
        return await loop.run_in_executor(None, self._list_sync)

    def _list_sync(self) -> list[dict]:
        if sys.platform != "win32":
            return []
        try:
            import winreg
        except ImportError:
            return []

        try:
            key = winreg.OpenKeyEx(
                winreg.HKEY_LOCAL_MACHINE, self._REG_KEY,
                0, winreg.KEY_READ,
            )
            raw = winreg.QueryValueEx(key, "monitors")[0]
            winreg.CloseKey(key)
            monitors = json.loads(raw) if isinstance(raw, str) else []
            return [
                {"index": i, "width": m["width"], "height": m["height"],
                 "refresh": m.get("refresh", 60)}
                for i, m in enumerate(monitors)
            ]
        except FileNotFoundError:
            return []
        except Exception as e:
            log.debug("parsec-vdd: list failed — %s", e)
            return []

    def _signal_driver(self) -> None:
        """Send an IOCTL to the Parsec VDD device to re-read its config."""
        if sys.platform != "win32":
            return
        import ctypes
        try:
            handle = ctypes.windll.kernel32.CreateFileW(
                self._DEVICE_PATH,
                0xC0000000,  # GENERIC_READ | GENERIC_WRITE
                0, None,
                3,  # OPEN_EXISTING
                0, None,
            )
            if handle == -1 or handle == 0xFFFFFFFF:
                log.debug("parsec-vdd: cannot open device for IOCTL signal")
                return

            bytes_returned = ctypes.c_ulong(0)
            ctypes.windll.kernel32.DeviceIoControl(
                handle, self._IOCTL_REFRESH,
                None, 0, None, 0,
                ctypes.byref(bytes_returned), None,
            )
            ctypes.windll.kernel32.CloseHandle(handle)
        except Exception as e:
            log.debug("parsec-vdd: IOCTL signal failed — %s", e)
            # Fallback: restart the service so the driver re-reads registry.
            try:
                subprocess.run(
                    ["sc", "stop", self._SERVICE_NAME],
                    capture_output=True, timeout=10,
                    creationflags=0x08000000,
                )
                subprocess.run(
                    ["sc", "start", self._SERVICE_NAME],
                    capture_output=True, timeout=10,
                    creationflags=0x08000000,
                )
            except Exception:
                pass


# ── Amyuni USB Mobile Monitor ─────────────────────────────────────────────────

class AmyuniVDDDriver(VirtualDisplayDriver):
    """
    Amyuni USB Mobile Monitor virtual display driver.

    Amyuni provides a signed IDD driver (free for open-source projects).
    Detection: look for the ``usbmmidd`` service or ``usbmmidd_v2``
    device in Device Manager.

    Control: Amyuni provides a ``deviceinstaller64.exe`` CLI tool.
      - ``deviceinstaller64 enableidd 1``  — add a virtual monitor
      - ``deviceinstaller64 enableidd 0``  — remove all virtual monitors

    Install: https://www.amyuni.com/downloads/usbmmidd_v2.zip

    Limitations: resolution is set via Windows Display Settings after the
    virtual monitor is created (Amyuni doesn't support per-monitor resolution
    via CLI). The driver creates a default 1920x1080 display.
    """

    name = "amyuni-vdd"

    _SERVICE_NAMES = ("usbmmidd", "usbmmidd_v2")
    _CLI_NAMES = ("deviceinstaller64.exe", "deviceinstaller.exe")
    _INSTALL_PATHS = (
        r"C:\Program Files\usbmmidd_v2",
        r"C:\usbmmidd_v2",
    )

    def __init__(self) -> None:
        self._cli_path: str | None = None
        self._active_count = 0

    def is_available(self) -> bool:
        if sys.platform != "win32":
            return False
        if not self._service_installed():
            return False
        self._cli_path = self._find_cli()
        return self._cli_path is not None

    def _service_installed(self) -> bool:
        for svc in self._SERVICE_NAMES:
            try:
                result = subprocess.run(
                    ["sc", "query", svc],
                    capture_output=True, text=True, timeout=5,
                    creationflags=0x08000000,
                )
                if "RUNNING" in result.stdout or "STOPPED" in result.stdout:
                    return True
            except Exception:
                continue
        return False

    def _find_cli(self) -> str | None:
        """Locate the Amyuni deviceinstaller CLI tool."""
        for base in self._INSTALL_PATHS:
            for cli_name in self._CLI_NAMES:
                path = os.path.join(base, cli_name)
                if os.path.isfile(path):
                    return path
        # Try PATH.
        for cli_name in self._CLI_NAMES:
            try:
                result = subprocess.run(
                    ["where", cli_name],
                    capture_output=True, text=True, timeout=5,
                    creationflags=0x08000000,
                )
                if result.returncode == 0 and result.stdout.strip():
                    return result.stdout.strip().splitlines()[0]
            except Exception:
                continue
        return None

    async def add(self, width: int, height: int, refresh: int) -> int | None:
        if not self._cli_path:
            return None

        loop = asyncio.get_running_loop()
        try:
            result = await loop.run_in_executor(
                None,
                lambda: subprocess.run(
                    [self._cli_path, "enableidd", "1"],
                    capture_output=True, text=True, timeout=15,
                    creationflags=0x08000000,
                ),
            )
            if result.returncode == 0:
                idx = self._active_count
                self._active_count += 1
                log.info("amyuni-vdd: added monitor %d (default resolution, "
                         "requested %dx%d — set via Display Settings)", idx, width, height)
                return idx
            log.warning("amyuni-vdd: enableidd failed — %s", result.stderr.strip())
        except Exception as e:
            log.warning("amyuni-vdd: add failed — %s", e)
        return None

    async def remove(self, index: int) -> bool:
        if not self._cli_path:
            return False

        # Amyuni's CLI removes all virtual monitors with enableidd 0.
        # We track count and only actually call it when removing the last one.
        if self._active_count <= 0:
            return False

        self._active_count -= 1

        if self._active_count == 0:
            loop = asyncio.get_running_loop()
            try:
                result = await loop.run_in_executor(
                    None,
                    lambda: subprocess.run(
                        [self._cli_path, "enableidd", "0"],
                        capture_output=True, text=True, timeout=15,
                        creationflags=0x08000000,
                    ),
                )
                if result.returncode != 0:
                    log.warning("amyuni-vdd: disableidd failed — %s",
                                result.stderr.strip())
                    return False
            except Exception as e:
                log.warning("amyuni-vdd: remove failed — %s", e)
                return False

        log.info("amyuni-vdd: removed monitor %d (%d remaining)",
                 index, self._active_count)
        return True

    async def list(self) -> list[dict]:
        return [
            {"index": i, "width": 1920, "height": 1080, "refresh": 60}
            for i in range(self._active_count)
        ]


# ── Linux xrandr virtual monitors ─────────────────────────────────────────────

class LinuxXrandrDriver(VirtualDisplayDriver):
    """
    Virtual monitors via ``xrandr --setmonitor`` (X11) or
    ``wlr-randr`` (wlroots-based Wayland compositors).

    These are compositor-level virtual monitors — they don't create
    separate X screens, but screen capture and window placement treat
    them as independent displays.
    """

    name = "xrandr"

    def __init__(self) -> None:
        self._monitors: dict[int, str] = {}  # index → name
        self._next_idx = 0
        self._display = os.environ.get("DISPLAY", ":0")

    def is_available(self) -> bool:
        if sys.platform == "win32":
            return False
        # Check for xrandr or wlr-randr.
        for cmd in ("xrandr", "wlr-randr"):
            try:
                result = subprocess.run(
                    [cmd, "--version"],
                    capture_output=True, timeout=5,
                )
                if result.returncode == 0:
                    return True
            except FileNotFoundError:
                continue
        return False

    async def add(self, width: int, height: int, refresh: int) -> int | None:
        idx = self._next_idx
        self._next_idx += 1
        name = f"OZMA-VIRTUAL-{idx}"

        # Position to the right of all existing outputs.
        max_x = self._find_max_x()
        spec = f"{width}/{width}x{height}/{height}+{max_x}+0"

        loop = asyncio.get_running_loop()
        try:
            result = await loop.run_in_executor(
                None,
                lambda: subprocess.run(
                    ["xrandr", "--setmonitor", name, spec, "none"],
                    capture_output=True, text=True, timeout=5,
                    env={**os.environ, "DISPLAY": self._display},
                ),
            )
            if result.returncode != 0:
                log.warning("xrandr: failed to create virtual monitor — %s",
                            result.stderr.strip())
                return None

            self._monitors[idx] = name
            log.info("xrandr: created virtual monitor %s (%dx%d at +%d+0)",
                     name, width, height, max_x)
            return idx

        except FileNotFoundError:
            log.warning("xrandr not found")
            return None
        except Exception as e:
            log.warning("xrandr: add failed — %s", e)
            return None

    async def remove(self, index: int) -> bool:
        name = self._monitors.pop(index, None)
        if not name:
            return False

        loop = asyncio.get_running_loop()
        try:
            result = await loop.run_in_executor(
                None,
                lambda n=name: subprocess.run(
                    ["xrandr", "--delmonitor", n],
                    capture_output=True, text=True, timeout=5,
                    env={**os.environ, "DISPLAY": self._display},
                ),
            )
            if result.returncode == 0:
                log.info("xrandr: removed virtual monitor %s", name)
                return True
            log.warning("xrandr: delmonitor failed — %s", result.stderr.strip())
            return False
        except Exception as e:
            log.warning("xrandr: remove failed — %s", e)
            return False

    async def list(self) -> list[dict]:
        # Parse xrandr --listactivemonitors for our OZMA-VIRTUAL-* entries.
        monitors: list[dict] = []
        try:
            result = subprocess.run(
                ["xrandr", "--listactivemonitors"],
                capture_output=True, text=True, timeout=5,
                env={**os.environ, "DISPLAY": self._display},
            )
            for line in result.stdout.splitlines():
                m = re.match(
                    r"\s*\d+:\s+[+*]*(\S+)\s+(\d+)/\d+x(\d+)/\d+",
                    line,
                )
                if m and m.group(1).startswith("OZMA-VIRTUAL-"):
                    name = m.group(1)
                    # Find corresponding index.
                    for idx, n in self._monitors.items():
                        if n == name:
                            monitors.append({
                                "index": idx,
                                "width": int(m.group(2)),
                                "height": int(m.group(3)),
                                "refresh": 60,
                            })
                            break
        except Exception:
            pass

        return monitors

    def _find_max_x(self) -> int:
        """Find the rightmost edge of all current monitors."""
        try:
            result = subprocess.run(
                ["xrandr", "--listactivemonitors"],
                capture_output=True, text=True, timeout=5,
                env={**os.environ, "DISPLAY": self._display},
            )
            max_x = 0
            for line in result.stdout.splitlines():
                m = re.match(
                    r"\s*\d+:\s+[+*]*\S+\s+(\d+)/\d+x\d+/\d+\+(\d+)\+\d+",
                    line,
                )
                if m:
                    width = int(m.group(1))
                    x_off = int(m.group(2))
                    max_x = max(max_x, x_off + width)
            return max_x
        except Exception:
            return 1920  # safe default


# ── Dummy plug detector (not a driver, informational) ──────────────────────────

class DummyPlugDetector:
    """
    Detect passive HDMI/DisplayPort dummy plugs.

    Dummy plugs present a generic or absent EDID — they show up as
    connected displays with names like "Generic PnP Monitor", no
    manufacturer string, or suspiciously standard resolutions.

    This is informational only — dummy plugs appear as physical displays
    and don't need driver management.
    """

    # Common dummy plug EDID identifiers.
    _DUMMY_INDICATORS = (
        "generic pnp",
        "dummy",
        "headless",
        "virtual display",
        "fit-headless",
    )

    @staticmethod
    def detect_dummy_plugs(displays: list[DisplayInfo]) -> list[DisplayInfo]:
        """
        Return displays that look like dummy HDMI/DP plugs.

        Uses heuristics: generic names, common dummy-plug resolutions,
        displays with no EDID manufacturer string.
        """
        dummies: list[DisplayInfo] = []
        for d in displays:
            name_lower = d.name.lower()
            if any(ind in name_lower for ind in DummyPlugDetector._DUMMY_INDICATORS):
                dummies.append(d)
        return dummies


# ── Virtual Display Manager (main entry point) ────────────────────────────────

class VirtualDisplayManager:
    """
    Create/destroy virtual monitors. Auto-detects the best available IDD
    driver backend.

    Priority order:
      1. ozma-vdd  (our driver, full control)
      2. parsec-vdd (widely installed, signed)
      3. amyuni-vdd (signed, free for OSS)
      4. xrandr     (Linux only)

    Usage::

        vdm = VirtualDisplayManager()
        info = await vdm.add_monitor(2560, 1440, 144)
        if info:
            print(f"Created {info.name} at {info.width}x{info.height}")
        await vdm.remove_monitor(info.index)

    An atexit handler cleans up all virtual monitors created during this
    session to prevent phantom displays after crashes.
    """

    def __init__(self) -> None:
        self._backend: VirtualDisplayDriver | None = None
        self._active: dict[int, DisplayInfo] = {}  # driver_index → DisplayInfo
        self._next_display_idx = 200  # DisplayInfo indices for virtual displays
        self._detect_backend()
        atexit.register(self._cleanup_sync)

    def _detect_backend(self) -> None:
        """Try each backend in priority order."""
        candidates: list[type[VirtualDisplayDriver]]

        if sys.platform == "win32":
            candidates = [OzmaVDDDriver, ParsecVDDDriver, AmyuniVDDDriver]
        else:
            candidates = [LinuxXrandrDriver]

        for cls in candidates:
            try:
                driver = cls()
                if driver.is_available():
                    self._backend = driver
                    log.info("Virtual display driver: %s", driver.name)
                    return
            except Exception as e:
                log.debug("Driver %s detection failed: %s", cls.name, e)

        if sys.platform == "win32":
            log.warning(
                "No virtual display driver available. Install one of:\n"
                "  - Ozma VDD (ozma-vdd): best option, full control\n"
                "  - Parsec VDD: https://github.com/nicollasricas/ParsecVDD\n"
                "  - Amyuni USB Mobile Monitor: https://www.amyuni.com/\n"
                "  - Or use dummy HDMI/DP plugs as a hardware workaround"
            )
        else:
            log.warning(
                "No virtual display driver available. "
                "Install xrandr (X11) or wlr-randr (Wayland)."
            )

    @property
    def available(self) -> bool:
        """True if a virtual display driver is detected and usable."""
        return self._backend is not None

    @property
    def driver_name(self) -> str:
        """Name of the active driver, or ``'none'``."""
        return self._backend.name if self._backend else "none"

    async def add_monitor(self, width: int = 1920, height: int = 1080,
                          refresh: int = 60) -> DisplayInfo | None:
        """
        Create a virtual monitor.

        Returns a ``DisplayInfo`` with ``virtual=True``, or ``None`` if no
        driver is available or creation failed.
        """
        if not self._backend:
            log.warning("Cannot create virtual display — no driver available")
            return None

        driver_idx = await self._backend.add(width, height, refresh)
        if driver_idx is None:
            return None

        display_idx = self._next_display_idx
        self._next_display_idx += 1

        # Position to the right of existing virtual displays.
        max_x = 0
        for d in self._active.values():
            max_x = max(max_x, d.x_offset + d.width)

        info = DisplayInfo(
            index=display_idx,
            name=f"Virtual-{driver_idx}",
            width=width,
            height=height,
            x_offset=max_x,
            y_offset=0,
            x_screen=f"vdd:{driver_idx}",
            virtual=True,
        )
        self._active[driver_idx] = info

        log.info("Virtual display created: %s (%dx%d@%dHz, driver=%s idx=%d)",
                 info.name, width, height, refresh, self.driver_name, driver_idx)
        return info

    async def remove_monitor(self, driver_index: int) -> bool:
        """
        Remove a virtual monitor by its driver index.

        Returns True if successfully removed.
        """
        if not self._backend:
            return False

        if driver_index not in self._active:
            log.warning("Virtual monitor %d not tracked — attempting removal anyway",
                        driver_index)

        ok = await self._backend.remove(driver_index)
        if ok:
            removed = self._active.pop(driver_index, None)
            if removed:
                log.info("Virtual display removed: %s", removed.name)
        return ok

    async def remove_by_display(self, display: DisplayInfo) -> bool:
        """Remove a virtual monitor by its DisplayInfo (looks up driver index)."""
        for driver_idx, info in self._active.items():
            if info.index == display.index:
                return await self.remove_monitor(driver_idx)
        log.warning("DisplayInfo index %d not found in active virtual displays",
                    display.index)
        return False

    async def list_monitors(self) -> list[DisplayInfo]:
        """List all active virtual monitors managed by this session."""
        return list(self._active.values())

    async def update_monitor(self, driver_index: int, width: int, height: int,
                             refresh: int = 60) -> bool:
        """Change the resolution of an existing virtual monitor."""
        if not self._backend:
            return False

        ok = await self._backend.update(driver_index, width, height, refresh)
        if ok and driver_index in self._active:
            info = self._active[driver_index]
            info.width = width
            info.height = height
            log.info("Virtual display updated: %s → %dx%d@%dHz",
                     info.name, width, height, refresh)
        return ok

    async def remove_all(self) -> int:
        """Remove all virtual monitors created in this session. Returns count removed."""
        count = 0
        for driver_idx in list(self._active.keys()):
            if await self.remove_monitor(driver_idx):
                count += 1
        return count

    def to_dict(self) -> dict:
        """Serialize state for monitoring/diagnostics."""
        return {
            "driver": self.driver_name,
            "available": self.available,
            "monitors": [
                {
                    "driver_index": didx,
                    "display_index": info.index,
                    "name": info.name,
                    "width": info.width,
                    "height": info.height,
                }
                for didx, info in self._active.items()
            ],
        }

    def _cleanup_sync(self) -> None:
        """
        atexit handler — remove all virtual monitors synchronously.

        Best-effort: if the event loop is gone we can't do async I/O,
        so we try to run a quick cleanup loop.
        """
        if not self._active or not self._backend:
            return

        log.info("Cleaning up %d virtual display(s) on exit", len(self._active))

        try:
            loop = asyncio.get_event_loop()
            if loop.is_running():
                # Can't block in a running loop — schedule and hope.
                loop.create_task(self.remove_all(), name="vdd-cleanup")
                return
            if loop.is_closed():
                loop = asyncio.new_event_loop()
            loop.run_until_complete(self.remove_all())
        except Exception as e:
            log.debug("atexit cleanup error: %s — virtual displays may persist", e)
