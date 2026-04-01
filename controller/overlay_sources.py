# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Video overlay sources — security cameras, media, alerts, and custom feeds.

The compositor can overlay video from external sources on top of the
active display capture.  Overlays are picture-in-picture windows that
appear based on triggers or permanently.

Sources:
  RTSP camera     — any IP camera (Frigate, Ubiquiti UniFi Protect, Hikvision, etc.)
  HTTP MJPEG      — simple MJPEG streams (many cameras, baby monitors)
  HLS stream      — any HLS source (TV, media server)
  Frigate         — Frigate NVR (MQTT events + RTSP streams)
  UniFi Protect   — Ubiquiti cameras via Protect API
  Media file      — local video/image file
  Web page        — rendered web content (weather, dashboards)
  Ozma screen     — content from the screen renderer (status panels, widgets)
  Capture source  — another ozma capture card (PiP of another machine)

Trigger modes:
  always        — overlay is always visible (security cam, clock)
  on_event      — appears on trigger, auto-hides after timeout
                  MQTT event, webhook, motion detection, doorbell
  on_scenario   — visible only in specific scenarios
  on_hotkey     — toggle with a hotkey or control surface button
  on_schedule   — visible during specific times

Overlay properties:
  position      — top-left, top-right, bottom-left, bottom-right, center, or x,y coordinates
  size          — percentage of screen or pixel dimensions
  opacity       — 0.0 (invisible) to 1.0 (opaque)
  border        — colour and width
  rounded       — corner radius
  animation     — slide_in, fade_in, pop (on show/hide)
  timeout_s     — auto-hide after N seconds (for triggered overlays)

Integration with the compositor:
  Overlays are composited onto the active capture feed in the web UI.
  They can also be burned into the HLS stream via ffmpeg filter_complex
  for recording or remote viewing.

Example configs:
  Doorbell camera (Frigate):
    {"source": "frigate", "camera": "front_door", "trigger": "doorbell",
     "position": "top-right", "size": "25%", "timeout_s": 30,
     "animation": "slide_in"}

  Security camera grid (always on, secondary display):
    {"source": "rtsp", "url": "rtsp://cam1/stream", "trigger": "always",
     "position": "full", "display": "monitor-2"}

  Another machine PiP:
    {"source": "capture", "capture_id": "hdmi-1", "trigger": "on_hotkey",
     "position": "bottom-right", "size": "20%", "opacity": 0.9}
"""

from __future__ import annotations

import asyncio
import json
import logging
import time
import urllib.request
from dataclasses import dataclass, field
from typing import Any

log = logging.getLogger("ozma.overlays")


@dataclass
class OverlaySource:
    """A video source for overlay display."""

    id: str
    name: str
    source_type: str          # rtsp, mjpeg, hls, frigate, unifi, media, web, capture, screen
    url: str = ""             # Stream URL (RTSP, MJPEG, HLS)
    camera: str = ""          # Camera name (for Frigate/UniFi)

    # Display properties
    position: str = "top-right"  # top-left, top-right, bottom-left, bottom-right, center, or "x,y"
    size: str = "25%"            # Percentage or "WxH"
    opacity: float = 0.9
    border_color: str = ""
    border_width: int = 0
    corner_radius: int = 8
    animation: str = "fade_in"   # slide_in, fade_in, pop, none

    # Trigger
    trigger: str = "on_event"    # always, on_event, on_scenario, on_hotkey, on_schedule
    trigger_config: dict = field(default_factory=dict)  # event pattern, scenario IDs, hotkey, schedule
    timeout_s: float = 30.0      # Auto-hide after N seconds (0 = manual dismiss)
    scenario_ids: list[str] = field(default_factory=list)  # For on_scenario trigger

    # State
    visible: bool = False
    last_shown: float = 0.0

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id, "name": self.name, "type": self.source_type,
            "url": self.url, "camera": self.camera,
            "position": self.position, "size": self.size,
            "opacity": self.opacity, "trigger": self.trigger,
            "visible": self.visible, "timeout_s": self.timeout_s,
        }


class OverlayManager:
    """
    Manages video overlay sources and their visibility.

    Overlays are pushed to the web UI via WebSocket events.
    The web UI composites them on top of the active display.
    """

    def __init__(self, state: Any = None) -> None:
        self._state = state
        self._overlays: dict[str, OverlaySource] = {}
        self._mqtt_task: asyncio.Task | None = None
        self._timeout_task: asyncio.Task | None = None
        self.on_change: Any = None  # async callback when overlay state changes

    async def start(self) -> None:
        self._timeout_task = asyncio.create_task(self._timeout_loop(), name="overlay-timeout")
        log.info("Overlay manager started (%d sources)", len(self._overlays))

    async def stop(self) -> None:
        if self._timeout_task:
            self._timeout_task.cancel()
        if self._mqtt_task:
            self._mqtt_task.cancel()

    def add_overlay(self, overlay: OverlaySource) -> None:
        self._overlays[overlay.id] = overlay
        if overlay.trigger == "always":
            overlay.visible = True

    def remove_overlay(self, overlay_id: str) -> None:
        self._overlays.pop(overlay_id, None)

    def list_overlays(self) -> list[dict]:
        return [o.to_dict() for o in self._overlays.values()]

    def get_visible(self) -> list[dict]:
        """Return only currently visible overlays (for the compositor)."""
        return [o.to_dict() for o in self._overlays.values() if o.visible]

    # ── Show / hide ──────────────────────────────────────────────────────────

    async def show_overlay(self, overlay_id: str) -> bool:
        o = self._overlays.get(overlay_id)
        if not o:
            return False
        o.visible = True
        o.last_shown = time.time()
        await self._notify_change(o)
        log.info("Overlay shown: %s (%s)", o.name, o.source_type)
        return True

    async def hide_overlay(self, overlay_id: str) -> bool:
        o = self._overlays.get(overlay_id)
        if not o:
            return False
        o.visible = False
        await self._notify_change(o)
        return True

    async def toggle_overlay(self, overlay_id: str) -> bool:
        o = self._overlays.get(overlay_id)
        if not o:
            return False
        if o.visible:
            return await self.hide_overlay(overlay_id)
        return await self.show_overlay(overlay_id)

    # ── Event triggers ───────────────────────────────────────────────────────

    async def on_event(self, event_type: str, data: dict) -> None:
        """Check if any overlay should show based on this event."""
        for o in self._overlays.values():
            if o.trigger != "on_event":
                continue
            pattern = o.trigger_config.get("event_pattern", "")
            if pattern and pattern in event_type:
                await self.show_overlay(o.id)

    async def on_scenario_switch(self, scenario_id: str) -> None:
        """Show/hide overlays based on scenario."""
        for o in self._overlays.values():
            if o.trigger == "on_scenario":
                if scenario_id in o.scenario_ids:
                    await self.show_overlay(o.id)
                else:
                    await self.hide_overlay(o.id)

    async def on_mqtt_message(self, topic: str, payload: dict) -> None:
        """Handle MQTT messages (Frigate events, etc.)."""
        for o in self._overlays.values():
            if o.source_type == "frigate" and o.trigger == "on_event":
                # Frigate publishes to frigate/events
                if "frigate" in topic:
                    event_type = payload.get("type", "")
                    camera = payload.get("camera", "")
                    if camera == o.camera and event_type in ("new", "update"):
                        label = payload.get("after", {}).get("label", "")
                        if label in ("person", "car", "dog", "cat", "package"):
                            await self.show_overlay(o.id)

    # ── Frigate integration ──────────────────────────────────────────────────

    def add_frigate_camera(self, camera_name: str, frigate_url: str,
                            trigger: str = "on_event", **kwargs: Any) -> OverlaySource:
        """Add a Frigate camera as an overlay source."""
        overlay = OverlaySource(
            id=f"frigate-{camera_name}",
            name=f"Frigate: {camera_name}",
            source_type="frigate",
            url=f"{frigate_url}/api/{camera_name}/latest.jpg",
            camera=camera_name,
            trigger=trigger,
            **kwargs,
        )
        # RTSP stream for live video
        overlay.trigger_config["rtsp_url"] = f"rtsp://{frigate_url.split('//')[1]}:8554/{camera_name}"
        self.add_overlay(overlay)
        return overlay

    # ── UniFi Protect integration ────────────────────────────────────────────

    def add_unifi_camera(self, camera_id: str, protect_url: str,
                          api_key: str = "", **kwargs: Any) -> OverlaySource:
        """Add a UniFi Protect camera as an overlay source."""
        overlay = OverlaySource(
            id=f"unifi-{camera_id}",
            name=f"UniFi: {camera_id}",
            source_type="unifi",
            url=f"{protect_url}/proxy/protect/api/cameras/{camera_id}/snapshot",
            camera=camera_id,
            trigger=kwargs.pop("trigger", "on_event"),
            **kwargs,
        )
        self.add_overlay(overlay)
        return overlay

    # ── Timeout handling ─────────────────────────────────────────────────────

    async def _timeout_loop(self) -> None:
        """Auto-hide overlays after their timeout expires."""
        while True:
            try:
                now = time.time()
                for o in self._overlays.values():
                    if o.visible and o.timeout_s > 0 and o.last_shown > 0:
                        if now - o.last_shown >= o.timeout_s:
                            o.visible = False
                            await self._notify_change(o)
                await asyncio.sleep(1)
            except asyncio.CancelledError:
                return

    async def _notify_change(self, overlay: OverlaySource) -> None:
        if self.on_change:
            await self.on_change("overlay.changed", overlay.to_dict())
