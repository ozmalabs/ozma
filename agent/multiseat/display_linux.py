# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Linux display backend for multi-seat.

Enumerates connected displays via xrandr. Each display maps to an X
screen region (same X server, different output) or separate X screen
(:0.0, :0.1) if Xinerama is off.

For virtual displays: uses xrandr virtual monitors or xf86-video-dummy.
"""

from __future__ import annotations

import asyncio
import logging
import os
import re
import subprocess
from pathlib import Path

from .display_backend import DisplayBackend, DisplayInfo

log = logging.getLogger("ozma.agent.multiseat.display_linux")


class LinuxDisplayBackend(DisplayBackend):
    """
    Linux display enumeration via xrandr.

    Uses `xrandr --listactivemonitors` for fast enumeration, then
    `xrandr --query` for detailed output info when needed.
    """

    def __init__(self) -> None:
        self._virtual_displays: list[DisplayInfo] = []
        self._next_virtual_idx = 100  # virtual display indices start at 100

    def enumerate(self) -> list[DisplayInfo]:
        """
        Enumerate all active displays via xrandr.

        Parses `xrandr --listactivemonitors` which gives:
          Monitors: 2
           0: +*HDMI-1 1920/530x1080/300+0+0  HDMI-1
           1: +DP-2 2560/600x1440/340+1920+0  DP-2
        """
        display = os.environ.get("DISPLAY", ":0")
        displays: list[DisplayInfo] = []

        try:
            result = subprocess.run(
                ["xrandr", "--listactivemonitors"],
                capture_output=True, text=True, timeout=5,
                env={**os.environ, "DISPLAY": display},
            )
            if result.returncode != 0:
                log.warning("xrandr failed: %s", result.stderr.strip())
                return self._fallback_enumerate(display)

            for line in result.stdout.splitlines():
                line = line.strip()
                # Parse: N: +*?NAME WxH/WxH+X+Y  NAME
                m = re.match(
                    r"(\d+):\s+[+*]*(\S+)\s+(\d+)/\d+x(\d+)/\d+\+(\d+)\+(\d+)\s+(\S+)",
                    line,
                )
                if not m:
                    continue

                idx = int(m.group(1))
                name = m.group(7)  # actual output name (HDMI-1, DP-2, etc.)
                width = int(m.group(3))
                height = int(m.group(4))
                x_off = int(m.group(5))
                y_off = int(m.group(6))
                primary = "*" in line.split(":")[1].split()[0] if ":" in line else False

                displays.append(DisplayInfo(
                    index=idx,
                    name=name,
                    width=width,
                    height=height,
                    x_offset=x_off,
                    y_offset=y_off,
                    x_screen=f"{display}",
                    primary=primary,
                ))

        except FileNotFoundError:
            log.warning("xrandr not found — falling back to DISPLAY env")
            return self._fallback_enumerate(display)
        except subprocess.TimeoutExpired:
            log.warning("xrandr timed out")
            return self._fallback_enumerate(display)

        if not displays:
            return self._fallback_enumerate(display)

        log.info("Enumerated %d displays via xrandr", len(displays))
        for d in displays:
            log.debug("  [%d] %s: %dx%d+%d+%d%s",
                      d.index, d.name, d.width, d.height,
                      d.x_offset, d.y_offset,
                      " (primary)" if d.primary else "")

        return displays

    def create_virtual(self, width: int = 1920, height: int = 1080,
                       name: str = "") -> DisplayInfo | None:
        """
        Create a virtual monitor via xrandr.

        Uses `xrandr --setmonitor` to create a virtual monitor that
        compositors treat as a separate display. This works for screen
        capture but doesn't create a real X screen boundary.

        For full isolation, configure separate X screens in xorg.conf.
        """
        display = os.environ.get("DISPLAY", ":0")
        idx = self._next_virtual_idx
        self._next_virtual_idx += 1
        vname = name or f"VIRTUAL-{idx}"

        # Find the highest x_offset among existing displays
        existing = self.enumerate()
        max_x = 0
        for d in existing:
            max_x = max(max_x, d.x_offset + d.width)

        # Create virtual monitor to the right of existing displays
        # xrandr --setmonitor NAME WxH+X+Y OUTPUT
        # Using "none" as output creates a pure virtual monitor
        monitor_spec = f"{width}/{width}x{height}/{height}+{max_x}+0"

        try:
            result = subprocess.run(
                ["xrandr", "--setmonitor", vname, monitor_spec, "none"],
                capture_output=True, text=True, timeout=5,
                env={**os.environ, "DISPLAY": display},
            )
            if result.returncode != 0:
                log.warning("Failed to create virtual monitor: %s", result.stderr.strip())
                return None

            info = DisplayInfo(
                index=idx,
                name=vname,
                width=width,
                height=height,
                x_offset=max_x,
                y_offset=0,
                x_screen=display,
                virtual=True,
            )
            self._virtual_displays.append(info)
            log.info("Created virtual display: %s (%dx%d at +%d+0)",
                     vname, width, height, max_x)
            return info

        except FileNotFoundError:
            log.warning("xrandr not found — cannot create virtual display")
            return None
        except subprocess.TimeoutExpired:
            log.warning("xrandr timed out creating virtual display")
            return None

    def destroy_virtual(self, display: DisplayInfo) -> bool:
        """Remove a virtual monitor created with xrandr --setmonitor."""
        if not display.virtual:
            log.warning("Cannot destroy non-virtual display: %s", display.name)
            return False

        display_env = os.environ.get("DISPLAY", ":0")
        try:
            result = subprocess.run(
                ["xrandr", "--delmonitor", display.name],
                capture_output=True, text=True, timeout=5,
                env={**os.environ, "DISPLAY": display_env},
            )
            if result.returncode == 0:
                self._virtual_displays = [
                    d for d in self._virtual_displays if d.name != display.name
                ]
                log.info("Destroyed virtual display: %s", display.name)
                return True
            log.warning("Failed to destroy virtual display: %s", result.stderr.strip())
            return False
        except Exception as e:
            log.warning("Error destroying virtual display: %s", e)
            return False

    def _fallback_enumerate(self, display: str) -> list[DisplayInfo]:
        """Fallback: return a single display for the current DISPLAY."""
        log.info("Using fallback: single display on %s", display)
        return [DisplayInfo(
            index=0,
            name="default",
            width=1920,
            height=1080,
            x_screen=display,
            primary=True,
        )]

    def get_display_for_capture(self, display: DisplayInfo) -> dict:
        """
        Return ffmpeg x11grab parameters for capturing a specific display.

        For multi-monitor X11, we capture a region at the display's offset
        within the root window.
        """
        return {
            "display": display.x_screen,
            "grab_x": display.x_offset,
            "grab_y": display.y_offset,
            "width": display.width,
            "height": display.height,
        }
