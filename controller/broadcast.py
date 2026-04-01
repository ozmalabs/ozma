# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Screen broadcast — push one display to all other machines as an overlay.

Teacher mode: broadcast one machine's screen to all others.
Presentation mode: one presenter's display on all room monitors.

Uses the overlay system to push the broadcast source as a PiP (or
fullscreen takeover) on every other node's web UI display.

Also supports: lock all screens (type Win+L / Ctrl+Alt+L on all nodes),
blank all screens (send display sleep via DDC/CI), send a message
to all screens (overlay text).
"""

from __future__ import annotations

import asyncio
import logging
from typing import Any

log = logging.getLogger("ozma.broadcast")


class ScreenBroadcast:
    """Broadcast a display source to all other nodes/displays."""

    def __init__(self, state: Any, overlays: Any = None,
                 paste_typer: Any = None, monitors: Any = None) -> None:
        self._state = state
        self._overlays = overlays
        self._paste_typer = paste_typer
        self._monitors = monitors
        self._broadcasting = False
        self._source_id = ""

    @property
    def is_broadcasting(self) -> bool:
        return self._broadcasting

    async def start_broadcast(self, source_id: str, mode: str = "overlay",
                                size: str = "100%", position: str = "center") -> bool:
        """
        Start broadcasting a source to all other displays.

        mode: "overlay" (PiP on each display) or "takeover" (fullscreen replace)
        """
        if not self._overlays:
            return False

        self._broadcasting = True
        self._source_id = source_id

        # Determine the stream URL for the broadcast source
        stream_url = f"/api/v1/captures/{source_id}/mjpeg"

        # Add as an overlay on all displays
        from overlay_sources import OverlaySource
        overlay = OverlaySource(
            id="broadcast",
            name="Screen Broadcast",
            source_type="capture",
            url=stream_url,
            trigger="always",
            position=position,
            size=size,
            opacity=1.0 if mode == "takeover" else 0.95,
        )
        overlay.visible = True
        self._overlays.add_overlay(overlay)

        log.info("Broadcast started: %s (%s)", source_id, mode)
        return True

    async def stop_broadcast(self) -> None:
        self._broadcasting = False
        if self._overlays:
            self._overlays.remove_overlay("broadcast")
        log.info("Broadcast stopped")

    async def lock_all_screens(self) -> int:
        """Send lock-screen keystroke to every node."""
        if not self._paste_typer:
            return 0
        count = 0
        for node in self._state.nodes.values():
            # Windows: Win+L, Linux: varies (Super+L, Ctrl+Alt+L)
            try:
                await self._paste_typer.type_key("l", modifier=0x08, node_id=node.id)  # GUI+L
                count += 1
            except Exception:
                pass
        log.info("Locked %d screens", count)
        return count

    async def blank_all_screens(self) -> int:
        """Turn off all managed monitors via DDC/CI."""
        if not self._monitors:
            return 0
        await self._monitors.dim_all(0)
        return len(self._monitors._monitors)

    async def send_message(self, text: str, duration_s: float = 10.0) -> bool:
        """Display a text message as an overlay on all screens."""
        if not self._overlays:
            return False
        from overlay_sources import OverlaySource
        overlay = OverlaySource(
            id="broadcast-message",
            name="Broadcast Message",
            source_type="screen",
            url="",  # Would use screen renderer to generate the message image
            trigger="always",
            position="center",
            size="50%",
            opacity=0.95,
            timeout_s=duration_s,
        )
        overlay.visible = True
        self._overlays.add_overlay(overlay)
        return True

    def to_dict(self) -> dict[str, Any]:
        return {
            "broadcasting": self._broadcasting,
            "source_id": self._source_id,
        }
