# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Input routing abstraction for multi-seat.

Groups physical input devices (keyboards, mice, gamepads) by USB hub
topology and assigns them to seats.
"""

from __future__ import annotations

from abc import ABC, abstractmethod
from dataclasses import dataclass, field
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from .seat import Seat


@dataclass
class InputGroup:
    """
    A group of input devices sharing a common USB hub.

    Devices plugged into the same USB hub are assumed to belong to one user
    (one keyboard + one mouse per hub = one seat's input).
    """
    hub_path: str                  # USB topology path ("1-1", "2-3.1", etc.)
    keyboards: list[str] = field(default_factory=list)  # evdev paths
    mice: list[str] = field(default_factory=list)
    gamepads: list[str] = field(default_factory=list)
    other: list[str] = field(default_factory=list)

    @property
    def device_count(self) -> int:
        return len(self.keyboards) + len(self.mice) + len(self.gamepads) + len(self.other)

    @property
    def has_input(self) -> bool:
        """True if this group has at least a keyboard or mouse."""
        return bool(self.keyboards or self.mice)

    @property
    def all_devices(self) -> list[str]:
        return self.keyboards + self.mice + self.gamepads + self.other

    def to_dict(self) -> dict:
        return {
            "hub_path": self.hub_path,
            "keyboards": self.keyboards,
            "mice": self.mice,
            "gamepads": self.gamepads,
            "other": self.other,
        }


class InputRouterBackend(ABC):
    """Abstract interface for input device enumeration and seat assignment."""

    @abstractmethod
    def enumerate_groups(self) -> list[InputGroup]:
        """
        Enumerate all input devices and group them by USB hub topology.

        Devices sharing the same parent USB hub are grouped together.
        Internal devices (laptop keyboards, touchpads) go into a group
        with hub_path="internal".
        """
        ...

    @abstractmethod
    def assign(self, group: InputGroup, seat: Seat) -> bool:
        """
        Assign an input group to a seat.

        On Linux this may involve xinput device assignment or evdev grabbing.
        Returns True if assignment succeeded.
        """
        ...

    @abstractmethod
    def unassign(self, group: InputGroup) -> bool:
        """Release an input group from its current seat assignment."""
        ...
