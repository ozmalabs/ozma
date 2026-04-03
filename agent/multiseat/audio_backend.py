# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Audio backend abstraction for multi-seat.

Each seat gets its own virtual audio sink so applications on that seat's
display output audio independently. The controller can route each sink
via VBAN or PipeWire linking.
"""

from __future__ import annotations

from abc import ABC, abstractmethod


class SeatAudioBackend(ABC):
    """Abstract interface for per-seat audio sink management."""

    @abstractmethod
    async def create_sink(self, seat_name: str) -> str | None:
        """
        Create a virtual audio sink for the given seat.

        Returns the sink name (e.g. "ozma-seat-0") or None on failure.
        """
        ...

    @abstractmethod
    async def destroy_sink(self, seat_name: str) -> bool:
        """Remove the virtual sink for the given seat. Returns True on success."""
        ...

    @abstractmethod
    async def assign_output(self, seat_name: str, device: str) -> bool:
        """
        Route a seat's virtual sink to a physical audio output device.

        For example, route "ozma-seat-1" to "alsa_output.usb-headset".
        Returns True on success.
        """
        ...

    @abstractmethod
    async def list_sinks(self) -> list[dict]:
        """List all managed seat audio sinks with their current state."""
        ...
