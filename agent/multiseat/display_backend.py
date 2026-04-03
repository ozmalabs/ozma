# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Display backend abstraction for multi-seat.

Enumerates physical and virtual displays. Each display maps to one seat.
"""

from __future__ import annotations

from abc import ABC, abstractmethod
from dataclasses import dataclass, field


@dataclass
class DisplayInfo:
    """A single display output (physical monitor or virtual display)."""
    index: int
    name: str          # "HDMI-1", "DP-2", "Virtual-0", etc.
    width: int
    height: int
    x_offset: int = 0  # pixel offset in the X screen (multi-monitor)
    y_offset: int = 0
    x_screen: str = ""  # ":0.0", ":0.1" (Linux Xinerama/RandR) or output idx
    primary: bool = False
    virtual: bool = False


class DisplayBackend(ABC):
    """Abstract interface for display enumeration and virtual display creation."""

    @abstractmethod
    def enumerate(self) -> list[DisplayInfo]:
        """Detect all connected displays. Returns one DisplayInfo per output."""
        ...

    @abstractmethod
    def create_virtual(self, width: int = 1920, height: int = 1080,
                       name: str = "") -> DisplayInfo | None:
        """
        Create a virtual display for headless seats.

        Returns the new DisplayInfo, or None if virtual displays are not
        supported on this platform.
        """
        ...

    @abstractmethod
    def destroy_virtual(self, display: DisplayInfo) -> bool:
        """Remove a previously created virtual display."""
        ...
