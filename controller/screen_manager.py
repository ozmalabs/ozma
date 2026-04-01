# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Pluggable screen manager — three rendering tiers for any display device.

Tier 1: FRAME PUSH (server-rendered)
  Controller renders PNG via Node.js → pushes full frame to device.
  Good for: Stream Deck keys (72x72), OLEDs (128x64), web widgets.
  Refresh: 1-5 fps.  Bandwidth: high (PNG per frame).

Tier 2: NATIVE RENDER (on-device)
  Controller pushes UI layout definition once.  Then streams only data
  updates (JSON key:value) at the target refresh rate.  Device renders
  locally at native speed.
  Good for: ESP32+TFT, Android app, Corsair LCD, ozma endpoints.
  Refresh: 30-60 fps.  Bandwidth: tiny (<100 bytes/update).

Tier 3: CONSTRAINED (mapped values)
  Device has a fixed UI that can't be fully customised.  Controller
  maps metric values to available display elements.
  Good for: X-Touch scribble strip, 7-segment displays, character LCDs.
  Refresh: device-dependent.  Bandwidth: minimal.

Each screen has a ScreenDriver that handles its specific transport.
The ScreenManager orchestrates: collects data, resolves metrics,
interpolates variables, and dispatches to each driver.

Video wall: screens in the same wall_group are tiled.  The renderer
generates one large frame; the manager splits it per-screen.
"""

from __future__ import annotations

import asyncio
import json
import logging
import time
import urllib.request
from dataclasses import dataclass, field
from typing import Any, Protocol

from screen_widgets import ScreenLayout, Widget, BUILTIN_LAYOUTS

log = logging.getLogger("ozma.screen_manager")

RENDERER_URL = "http://localhost:7390"


# ── Screen driver protocol ───────────────────────────────────────────────────

class ScreenDriver(Protocol):
    """Interface for screen output drivers."""

    @property
    def tier(self) -> str:
        """Return 'frame_push', 'native_render', or 'constrained'."""
        ...

    async def push_frame(self, frame: bytes) -> None:
        """Tier 1: push a rendered PNG/JPEG frame."""
        ...

    async def push_layout(self, layout: dict) -> None:
        """Tier 2: push a UI layout definition (once or on change)."""
        ...

    async def push_data(self, data: dict) -> None:
        """Tier 2: push data-only update (at refresh_hz)."""
        ...

    async def push_values(self, values: dict[str, Any]) -> None:
        """Tier 3: push mapped values to constrained device."""
        ...

    async def start(self) -> None: ...
    async def stop(self) -> None: ...


# ── Concrete drivers ─────────────────────────────────────────────────────────

class FramePushDriver:
    """Tier 1: push PNG frames. Base for Stream Deck, OLED, web."""

    tier = "frame_push"

    def __init__(self, push_fn: Any = None) -> None:
        self._push_fn = push_fn  # async callable(bytes) — device-specific

    async def push_frame(self, frame: bytes) -> None:
        if self._push_fn:
            await self._push_fn(frame)

    async def push_layout(self, layout: dict) -> None: pass
    async def push_data(self, data: dict) -> None: pass
    async def push_values(self, values: dict[str, Any]) -> None: pass
    async def start(self) -> None: pass
    async def stop(self) -> None: pass


class NativeRenderDriver:
    """
    Tier 2: push layout once, stream data updates via WebSocket.

    The device connects to the controller's WebSocket endpoint,
    receives the UI definition, then gets data-only updates at
    the target refresh rate.
    """

    tier = "native_render"

    def __init__(self, device_id: str) -> None:
        self.device_id = device_id
        self._ws: Any = None  # WebSocket connection (set by WS handler)
        self._layout_sent = False

    async def push_layout(self, layout: dict) -> None:
        if self._ws:
            try:
                await self._ws.send(json.dumps({"type": "layout", "layout": layout}))
                self._layout_sent = True
            except Exception:
                self._layout_sent = False

    async def push_data(self, data: dict) -> None:
        if self._ws and self._layout_sent:
            try:
                await self._ws.send(json.dumps({"type": "data", "d": data}))
            except Exception:
                pass

    async def push_frame(self, frame: bytes) -> None: pass
    async def push_values(self, values: dict[str, Any]) -> None: pass

    def set_websocket(self, ws: Any) -> None:
        self._ws = ws
        self._layout_sent = False

    async def start(self) -> None: pass
    async def stop(self) -> None: pass


class ConstrainedDriver:
    """
    Tier 3: map values to fixed display elements.

    For devices with a limited UI (scribble strips, 7-seg, character LCDs).
    The mapping defines which metric goes to which display element.
    """

    tier = "constrained"

    def __init__(self, push_fn: Any = None) -> None:
        self._push_fn = push_fn  # async callable(dict[str, Any])

    async def push_values(self, values: dict[str, Any]) -> None:
        if self._push_fn:
            await self._push_fn(values)

    async def push_frame(self, frame: bytes) -> None: pass
    async def push_layout(self, layout: dict) -> None: pass
    async def push_data(self, data: dict) -> None: pass
    async def start(self) -> None: pass
    async def stop(self) -> None: pass


# ── Screen registration ──────────────────────────────────────────────────────

@dataclass
class Screen:
    """A registered display device with its driver and layout."""

    id: str
    name: str
    width: int
    height: int
    driver: Any                     # ScreenDriver implementation
    layout_id: str = ""             # ScreenLayout ID to use
    layout: ScreenLayout | None = None
    data_source: str = ""           # metric source (node_id, or "" for active)
    custom_data: dict = field(default_factory=dict)
    refresh_hz: int = 5
    enabled: bool = True
    # Video wall: grid mode (regular tiling)
    wall_group: str = ""
    wall_x: int = 0             # Grid column
    wall_y: int = 0             # Grid row

    # Swordfish mode: arbitrary placement on a canvas
    # Screens can be placed at any position, any angle, any scale.
    # The renderer composites them onto a virtual canvas and slices
    # each screen's region, accounting for rotation and overlap.
    canvas_x: float = 0.0       # X position on canvas (pixels from origin)
    canvas_y: float = 0.0       # Y position on canvas
    canvas_rotation: float = 0.0  # Degrees clockwise
    canvas_scale: float = 1.0   # Scale factor (1.0 = native resolution)
    canvas_group: str = ""      # Swordfish canvas group ID

    def to_dict(self) -> dict[str, Any]:
        d = {
            "id": self.id, "name": self.name,
            "width": self.width, "height": self.height,
            "tier": self.driver.tier if self.driver else "unknown",
            "layout_id": self.layout_id,
            "data_source": self.data_source,
            "refresh_hz": self.refresh_hz,
            "enabled": self.enabled,
        }
        if self.wall_group:
            d["wall_group"] = self.wall_group
            d["wall_position"] = [self.wall_x, self.wall_y]
        if self.canvas_group:
            d["canvas_group"] = self.canvas_group
            d["canvas"] = {
                "x": self.canvas_x, "y": self.canvas_y,
                "rotation": self.canvas_rotation,
                "scale": self.canvas_scale,
            }
        return d


# ── Screen Manager ───────────────────────────────────────────────────────────

class ScreenManager:
    """
    Orchestrates all screen outputs across three rendering tiers.

    For each screen:
    1. Resolve its layout (from BUILTIN_LAYOUTS or custom)
    2. Collect metric data for all referenced metrics
    3. Dispatch to the driver's tier-appropriate method
    """

    def __init__(self, metrics: Any = None, state: Any = None) -> None:
        self._screens: dict[str, Screen] = {}
        self._layouts: dict[str, ScreenLayout] = dict(BUILTIN_LAYOUTS)
        self._metrics = metrics
        self._state = state
        self._tasks: dict[str, asyncio.Task] = {}
        self._native_drivers: dict[str, NativeRenderDriver] = {}
        self._renderer_url = RENDERER_URL
        self._scenario_data: dict[str, Any] = {}

    async def start(self) -> None:
        for screen in self._screens.values():
            if screen.enabled:
                self._start_screen(screen)
        log.info("Screen manager started (%d screens, %d layouts)",
                 len(self._screens), len(self._layouts))

    async def stop(self) -> None:
        for task in self._tasks.values():
            task.cancel()
        for screen in self._screens.values():
            if screen.driver:
                await screen.driver.stop()

    # ── Screen + layout registration ─────────────────────────────────────────

    def register_screen(self, screen: Screen) -> None:
        self._screens[screen.id] = screen
        if screen.layout_id and screen.layout_id in self._layouts:
            screen.layout = self._layouts[screen.layout_id]
        log.info("Screen registered: %s (%s, %dx%d, tier=%s)",
                 screen.name, screen.id, screen.width, screen.height,
                 screen.driver.tier if screen.driver else "none")

    def register_layout(self, layout: ScreenLayout) -> None:
        self._layouts[layout.id] = layout

    def unregister_screen(self, screen_id: str) -> None:
        if screen_id in self._tasks:
            self._tasks[screen_id].cancel()
        self._screens.pop(screen_id, None)

    def list_screens(self) -> list[dict[str, Any]]:
        return [s.to_dict() for s in self._screens.values()]

    def list_layouts(self) -> list[dict[str, Any]]:
        return [l.to_dict() for l in self._layouts.values()]

    def update_screen(self, screen_id: str, **kwargs: Any) -> bool:
        screen = self._screens.get(screen_id)
        if not screen:
            return False
        for k, v in kwargs.items():
            if k == "layout_id" and v in self._layouts:
                screen.layout = self._layouts[v]
                screen.layout_id = v
                # Re-push layout to native devices
                if screen.driver and screen.driver.tier == "native_render":
                    asyncio.create_task(screen.driver.push_layout(screen.layout.to_dict()))
            elif hasattr(screen, k):
                setattr(screen, k, v)
        return True

    # ── Native device WebSocket registration ─────────────────────────────────

    def register_native_device(self, device_id: str) -> NativeRenderDriver:
        """Create a NativeRenderDriver for a connecting device."""
        driver = NativeRenderDriver(device_id)
        self._native_drivers[device_id] = driver
        return driver

    # ── Scenario integration ─────────────────────────────────────────────────

    async def on_scenario_switch(self, scenario: dict) -> None:
        self._scenario_data = scenario
        for screen in self._screens.values():
            if screen.driver and screen.driver.tier == "native_render":
                data = self._collect_data(screen)
                await screen.driver.push_data(data)

    # ── Video wall ───────────────────────────────────────────────────────────

    def get_wall_screens(self, wall_group: str) -> list[Screen]:
        screens = [s for s in self._screens.values() if s.wall_group == wall_group]
        screens.sort(key=lambda s: (s.wall_y, s.wall_x))
        return screens

    async def render_wall(self, wall_group: str, template: str, data: dict) -> None:
        screens = self.get_wall_screens(wall_group)
        if not screens:
            return
        max_col = max(s.wall_x for s in screens) + 1
        max_row = max(s.wall_y for s in screens) + 1
        tw, th = screens[0].width, screens[0].height
        full_frame = await self._render_frame(template, data, max_col * tw, max_row * th)
        if not full_frame:
            return
        try:
            from PIL import Image
            import io
            img = Image.open(io.BytesIO(full_frame))
            for s in screens:
                tile = img.crop((s.wall_x * tw, s.wall_y * th, (s.wall_x + 1) * tw, (s.wall_y + 1) * th))
                buf = io.BytesIO()
                tile.save(buf, format="PNG")
                await s.driver.push_frame(buf.getvalue())
        except ImportError:
            pass

    # ── Swordfish mode ───────────────────────────────────────────────────────
    #
    # Named after the movie — monitors placed arbitrarily on a canvas at any
    # position, rotation, and scale.  Not the most practical layout, but it
    # looks incredible for demos and showroom installations.
    #
    # The renderer generates one large frame on a virtual canvas.  Each screen
    # is sliced from the canvas at its position + rotation, with perspective
    # correction so the content appears correct when viewed from the front.

    def get_swordfish_screens(self, canvas_group: str) -> list[Screen]:
        return [s for s in self._screens.values() if s.canvas_group == canvas_group]

    async def render_swordfish(self, canvas_group: str, template: str, data: dict) -> None:
        """
        Render content across arbitrarily-placed screens (Swordfish mode).

        Each screen has canvas_x, canvas_y, canvas_rotation, canvas_scale.
        The renderer produces a full canvas image, then each screen's region
        is extracted with rotation and scaling applied.
        """
        screens = self.get_swordfish_screens(canvas_group)
        if not screens:
            return

        # Calculate canvas bounds (bounding box of all screens including rotation)
        import math
        min_x, min_y = float("inf"), float("inf")
        max_x, max_y = float("-inf"), float("-inf")

        for s in screens:
            # Four corners of this screen, rotated
            hw, hh = (s.width * s.canvas_scale) / 2, (s.height * s.canvas_scale) / 2
            rad = math.radians(s.canvas_rotation)
            cos_r, sin_r = math.cos(rad), math.sin(rad)

            for dx, dy in [(-hw, -hh), (hw, -hh), (hw, hh), (-hw, hh)]:
                rx = s.canvas_x + dx * cos_r - dy * sin_r
                ry = s.canvas_y + dx * sin_r + dy * cos_r
                min_x = min(min_x, rx)
                min_y = min(min_y, ry)
                max_x = max(max_x, rx)
                max_y = max(max_y, ry)

        canvas_w = int(max_x - min_x) + 1
        canvas_h = int(max_y - min_y) + 1

        if canvas_w <= 0 or canvas_h <= 0:
            return

        # Render the full canvas
        full_frame = await self._render_frame(template, data, canvas_w, canvas_h)
        if not full_frame:
            return

        try:
            from PIL import Image
            import io

            canvas = Image.open(io.BytesIO(full_frame))

            for s in screens:
                # Calculate this screen's region on the canvas
                cx = s.canvas_x - min_x
                cy = s.canvas_y - min_y
                sw = int(s.width * s.canvas_scale)
                sh = int(s.height * s.canvas_scale)

                # Crop the region (before rotation — we rotate the crop, not the canvas)
                # For rotated screens: rotate the canvas around the screen's centre,
                # then crop the axis-aligned rectangle
                if abs(s.canvas_rotation) > 0.5:
                    # Rotate canvas around this screen's centre point
                    rotated = canvas.rotate(
                        -s.canvas_rotation,
                        center=(cx, cy),
                        expand=False,
                        resample=Image.BICUBIC,
                    )
                    left = int(cx - sw / 2)
                    top = int(cy - sh / 2)
                else:
                    rotated = canvas
                    left = int(cx - sw / 2)
                    top = int(cy - sh / 2)

                # Crop and resize to screen's native resolution
                crop = rotated.crop((
                    max(0, left), max(0, top),
                    min(canvas_w, left + sw), min(canvas_h, top + sh),
                ))
                if crop.size != (s.width, s.height):
                    crop = crop.resize((s.width, s.height), Image.LANCZOS)

                buf = io.BytesIO()
                crop.save(buf, format="PNG")
                await s.driver.push_frame(buf.getvalue())

        except ImportError:
            log.debug("Pillow not available for swordfish mode")

    # ── Render loop ──────────────────────────────────────────────────────────

    def _start_screen(self, screen: Screen) -> None:
        if screen.id in self._tasks:
            self._tasks[screen.id].cancel()
        self._tasks[screen.id] = asyncio.create_task(
            self._screen_loop(screen), name=f"screen-{screen.id}"
        )

    async def _screen_loop(self, screen: Screen) -> None:
        interval = 1.0 / max(screen.refresh_hz, 1)

        # For native devices, push layout on first run
        if screen.driver and screen.driver.tier == "native_render" and screen.layout:
            await screen.driver.push_layout(screen.layout.to_dict())

        while True:
            try:
                data = self._collect_data(screen)

                match screen.driver.tier if screen.driver else "":
                    case "frame_push":
                        template = screen.layout_id or "status"
                        frame = await self._render_frame(template, data, screen.width, screen.height)
                        if frame:
                            await screen.driver.push_frame(frame)

                    case "native_render":
                        await screen.driver.push_data(data)

                    case "constrained":
                        await screen.driver.push_values(data)

                await asyncio.sleep(interval)
            except asyncio.CancelledError:
                return
            except Exception:
                await asyncio.sleep(2.0)

    def _collect_data(self, screen: Screen) -> dict:
        """Collect all metric values referenced by a screen's layout."""
        data: dict[str, Any] = {}

        # Scenario variables
        data["scenario_name"] = self._scenario_data.get("name", "")
        data["scenario_color"] = self._scenario_data.get("color", "#888888")
        data["scenario_id"] = self._scenario_data.get("id", "")

        # Active node
        if self._state:
            active = self._state.get_active_node()
            if active:
                data["node_name"] = active.id.split(".")[0]

        # Time
        data["time"] = time.strftime("%H:%M:%S")
        data["date"] = time.strftime("%Y-%m-%d")

        # Custom data overrides
        data.update(screen.custom_data)

        # Metrics from collector
        if self._metrics and screen.layout:
            source = screen.data_source or ""
            for metric_ref in screen.layout.get_metric_keys():
                # Resolve @active to actual source
                ref = metric_ref.replace("@active.", f"{data.get('node_name', '')}.")
                parts = ref.split(".", 1)
                if len(parts) == 2:
                    src_id, key = parts
                    val = self._metrics.get_metric(src_id, key)
                    if val is not None:
                        data[metric_ref] = val
                        data[ref] = val
                elif source:
                    val = self._metrics.get_metric(source, metric_ref)
                    if val is not None:
                        data[metric_ref] = val

        return data

    async def _render_frame(self, template: str, data: dict, w: int, h: int) -> bytes | None:
        payload = json.dumps({"template": template, "data": data, "width": w, "height": h}).encode()
        try:
            loop = asyncio.get_running_loop()
            def _f():
                req = urllib.request.Request(
                    f"{self._renderer_url}/render", data=payload,
                    headers={"Content-Type": "application/json"}, method="POST",
                )
                with urllib.request.urlopen(req, timeout=3) as r:
                    return r.read()
            return await loop.run_in_executor(None, _f)
        except Exception:
            return None
