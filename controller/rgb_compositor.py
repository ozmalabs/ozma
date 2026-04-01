# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Layered RGB compositing engine.

Blends multiple RGB layers by priority to produce the final output for
each LED in each zone.  This is the core of ozma's RGB system — every
visual effect goes through the compositor.

Layer stack (lowest → highest priority):

  0. AMBIENT     Always-on background effect (rainbow, breathe, colour
                 cycle, static colour).  Runs continuously.

  1. SCENARIO    The active scenario's colour.  Set on scenario switch.
                 Inactive nodes get a dim version of their scenario colour.

  2. NOTE        Transient notifications with TTL.  E.g., "node online"
                 green flash, "audio muted" white pulse.  Multiple notes
                 can be active; newest wins per-LED.

  3. SYSTEM      Highest priority alerts — overcurrent warning, connection
                 lost, power failure.  Always visible when active.

Compositing model:
  - Each layer has a per-LED colour buffer and an opacity (0.0-1.0).
  - Final colour = blend top-down: for each LED, the highest-priority
    non-transparent layer wins.  Layers with opacity < 1.0 blend with
    the layer below.
  - Layers can cover all LEDs or a subset (e.g., a note only affects
    the first 3 LEDs as a status indicator).

The compositor runs at a configurable FPS (default 30) and pushes the
composited result to all RGBOutputManager zones each frame.

Ambient effects:
  - solid:       One static colour
  - rainbow:     Hue sweep across the strip
  - breathe:     Fade in/out
  - chase:       Running dot
  - fire:        Flickering warm tones
  - colour_cycle: Slowly rotate through hue wheel

Note presets:
  - node_online:    Green flash (2s)
  - node_offline:   Red flash (3s)
  - mute_toggle:    White pulse (1s)
  - scenario_switch: Scenario colour wave (0.5s)
  - overcurrent:    Red strobe (stays until cleared)
  - power_off:      Fade to black (2s)
"""

from __future__ import annotations

import asyncio
import colorsys
import logging
import math
import time
from dataclasses import dataclass, field
from enum import IntEnum
from typing import Any

log = logging.getLogger("ozma.rgb_compositor")

RGB = tuple[int, int, int]
BLACK: RGB = (0, 0, 0)


class LayerPriority(IntEnum):
    AMBIENT = 0
    SCENARIO = 1
    NOTE = 2
    SYSTEM = 3


@dataclass
class RGBLayer:
    """A single compositing layer."""

    name: str
    priority: LayerPriority
    opacity: float = 1.0            # 0.0 = fully transparent, 1.0 = fully opaque
    leds: list[RGB | None] = field(default_factory=list)  # None = transparent (pass-through)
    effect: str = "solid"           # Effect name for animated layers
    color: RGB = BLACK              # Base colour for the layer
    ttl: float | None = None        # Seconds until auto-removal (None = permanent)
    created_at: float = 0.0         # time.monotonic() when created
    active: bool = True


@dataclass
class AmbientConfig:
    """Configuration for the ambient layer effect."""
    effect: str = "solid"           # solid, rainbow, breathe, chase, fire, colour_cycle
    color: RGB = (20, 15, 40)       # Base colour for solid/breathe
    speed: float = 1.0              # Effect speed multiplier
    brightness: float = 0.3         # 0.0-1.0


class RGBCompositor:
    """
    Composites multiple RGB layers and outputs the result at a fixed FPS.

    Usage::

        comp = RGBCompositor(led_count=30, fps=30)
        comp.on_frame = my_output_callback  # async def (list[RGB])

        comp.set_ambient(AmbientConfig(effect="rainbow", brightness=0.3))
        comp.set_scenario_color((74, 144, 217))
        comp.add_note("mute", color=(255, 255, 255), ttl=1.0)
        comp.set_system_alert("overcurrent", color=(255, 0, 0))

        await comp.start()
    """

    def __init__(self, led_count: int = 30, fps: int = 30) -> None:
        self._led_count = led_count
        self._fps = fps
        self._layers: dict[str, RGBLayer] = {}
        self._ambient_config = AmbientConfig()
        self._task: asyncio.Task | None = None
        self._t = 0.0  # time accumulator for effects

        # Callback: async def (zone_id_or_none, list[RGB])
        self.on_frame: Any = None

    @property
    def led_count(self) -> int:
        return self._led_count

    async def start(self) -> None:
        self._task = asyncio.create_task(self._render_loop(), name="rgb-compositor")

    async def stop(self) -> None:
        if self._task:
            self._task.cancel()
            try:
                await self._task
            except asyncio.CancelledError:
                pass

    # ── Layer control ────────────────────────────────────────────────────────

    def set_ambient(self, config: AmbientConfig) -> None:
        """Set the ambient (background) effect."""
        self._ambient_config = config

    def set_scenario_color(self, color: RGB, inactive_color: RGB | None = None) -> None:
        """Set the scenario layer colour."""
        leds = [color] * self._led_count
        self._layers["scenario"] = RGBLayer(
            name="scenario",
            priority=LayerPriority.SCENARIO,
            opacity=1.0,
            leds=leds,
            color=color,
            effect="solid",
        )

    def clear_scenario(self) -> None:
        self._layers.pop("scenario", None)

    def add_note(
        self,
        name: str,
        color: RGB,
        ttl: float = 2.0,
        opacity: float = 1.0,
        effect: str = "flash",
        led_range: tuple[int, int] | None = None,
    ) -> None:
        """
        Add a notification layer.

        Args:
            name: Unique note name (replaces existing note with same name)
            color: Note colour
            ttl: Time-to-live in seconds (None = permanent until cleared)
            opacity: Layer opacity
            effect: "flash" (fade out), "pulse" (fade in+out), "strobe", "solid"
            led_range: (start, end) indices, or None for all LEDs
        """
        leds: list[RGB | None] = [None] * self._led_count
        start = led_range[0] if led_range else 0
        end = led_range[1] if led_range else self._led_count
        for i in range(start, min(end, self._led_count)):
            leds[i] = color

        self._layers[f"note:{name}"] = RGBLayer(
            name=name,
            priority=LayerPriority.NOTE,
            opacity=opacity,
            leds=leds,
            color=color,
            effect=effect,
            ttl=ttl,
            created_at=time.monotonic(),
            active=True,
        )

    def clear_note(self, name: str) -> None:
        self._layers.pop(f"note:{name}", None)

    def set_system_alert(self, name: str, color: RGB, effect: str = "strobe") -> None:
        """Set a system alert (highest priority, stays until cleared)."""
        leds = [color] * self._led_count
        self._layers[f"system:{name}"] = RGBLayer(
            name=name,
            priority=LayerPriority.SYSTEM,
            opacity=1.0,
            leds=leds,
            color=color,
            effect=effect,
        )

    def clear_system_alert(self, name: str) -> None:
        self._layers.pop(f"system:{name}", None)

    def clear_all_alerts(self) -> None:
        for key in list(self._layers):
            if key.startswith("system:"):
                del self._layers[key]

    # ── Preset notifications ─────────────────────────────────────────────────

    def notify_node_online(self, node_name: str) -> None:
        self.add_note(f"online-{node_name}", color=(0, 255, 80), ttl=2.0, effect="flash")

    def notify_node_offline(self, node_name: str) -> None:
        self.add_note(f"offline-{node_name}", color=(255, 50, 50), ttl=3.0, effect="pulse")

    def notify_mute_toggle(self, muted: bool) -> None:
        color = (255, 60, 60) if muted else (60, 255, 60)
        self.add_note("mute", color=color, ttl=1.0, effect="flash")

    def notify_scenario_switch(self, color: RGB) -> None:
        self.add_note("switch", color=color, ttl=0.5, effect="flash")

    def alert_overcurrent(self) -> None:
        self.set_system_alert("overcurrent", color=(255, 0, 0), effect="strobe")

    def alert_power_lost(self) -> None:
        self.set_system_alert("power_lost", color=(255, 100, 0), effect="pulse")

    # ── Render loop ──────────────────────────────────────────────────────────

    async def _render_loop(self) -> None:
        interval = 1.0 / self._fps
        while True:
            try:
                frame_start = time.monotonic()
                self._t += interval

                # Expire TTL layers
                self._expire_layers()

                # Render each layer
                ambient = self._render_ambient()
                composited = self._composite(ambient)

                # Push to output
                if self.on_frame:
                    await self.on_frame(composited)

                elapsed = time.monotonic() - frame_start
                await asyncio.sleep(max(0.0, interval - elapsed))

            except asyncio.CancelledError:
                return

    def _expire_layers(self) -> None:
        """Remove layers that have exceeded their TTL."""
        now = time.monotonic()
        expired = [
            key for key, layer in self._layers.items()
            if layer.ttl is not None and (now - layer.created_at) >= layer.ttl
        ]
        for key in expired:
            del self._layers[key]

    def _composite(self, base: list[RGB]) -> list[RGB]:
        """Blend all layers onto the base (ambient) buffer."""
        result = list(base)

        # Sort layers by priority (lowest first)
        sorted_layers = sorted(
            self._layers.values(),
            key=lambda l: (l.priority, l.created_at),
        )

        for layer in sorted_layers:
            if not layer.active:
                continue

            # Calculate effective opacity (may be modulated by effect)
            t_alive = time.monotonic() - layer.created_at if layer.created_at else self._t
            eff_opacity = self._effect_opacity(layer, t_alive)
            if eff_opacity <= 0.001:
                continue

            for i in range(self._led_count):
                if i >= len(layer.leds):
                    break
                led_color = layer.leds[i]
                if led_color is None:
                    continue  # Transparent — pass through
                result[i] = _blend(result[i], led_color, eff_opacity * layer.opacity)

        return result

    def _effect_opacity(self, layer: RGBLayer, t_alive: float) -> float:
        """Calculate opacity modulation based on effect type."""
        match layer.effect:
            case "solid":
                return 1.0
            case "flash":
                # Fade out over TTL
                if layer.ttl and layer.ttl > 0:
                    return max(0.0, 1.0 - (t_alive / layer.ttl))
                return 1.0
            case "pulse":
                # Sine wave fade in/out
                if layer.ttl and layer.ttl > 0:
                    phase = t_alive / layer.ttl * math.pi
                    return max(0.0, math.sin(phase))
                return (math.sin(t_alive * 3.0) + 1.0) / 2.0
            case "strobe":
                # Fast on/off
                return 1.0 if int(t_alive * 6) % 2 == 0 else 0.0
            case _:
                return 1.0

    # ── Ambient effects ──────────────────────────────────────────────────────

    def _render_ambient(self) -> list[RGB]:
        """Generate the ambient (base) layer."""
        cfg = self._ambient_config
        n = self._led_count
        t = self._t * cfg.speed
        bri = cfg.brightness

        match cfg.effect:
            case "solid":
                r, g, b = cfg.color
                return [_dim((r, g, b), bri)] * n

            case "rainbow":
                leds = []
                for i in range(n):
                    hue = (i / n + t * 0.1) % 1.0
                    r, g, b = colorsys.hsv_to_rgb(hue, 1.0, 1.0)
                    leds.append(_dim((int(r * 255), int(g * 255), int(b * 255)), bri))
                return leds

            case "breathe":
                phase = (math.sin(t * 2.0) + 1.0) / 2.0
                r, g, b = cfg.color
                return [_dim((r, g, b), bri * phase)] * n

            case "chase":
                leds = [_dim(cfg.color, bri * 0.05)] * n
                pos = int(t * 15) % n
                for offset in range(-1, 2):
                    idx = (pos + offset) % n
                    dim_factor = 1.0 if offset == 0 else 0.3
                    leds[idx] = _dim(cfg.color, bri * dim_factor)
                return leds

            case "fire":
                import random
                leds = []
                for i in range(n):
                    flicker = 0.5 + random.random() * 0.5
                    r = int(min(255, 255 * flicker * bri))
                    g = int(min(255, 80 * flicker * bri))
                    b = int(min(255, 10 * flicker * bri))
                    leds.append((r, g, b))
                return leds

            case "colour_cycle":
                hue = (t * 0.05) % 1.0
                r, g, b = colorsys.hsv_to_rgb(hue, 0.8, 1.0)
                return [_dim((int(r * 255), int(g * 255), int(b * 255)), bri)] * n

            case _:
                return [BLACK] * n

    # ── State for API ────────────────────────────────────────────────────────

    def state_dict(self) -> dict[str, Any]:
        return {
            "led_count": self._led_count,
            "fps": self._fps,
            "ambient": {
                "effect": self._ambient_config.effect,
                "color": list(self._ambient_config.color),
                "speed": self._ambient_config.speed,
                "brightness": self._ambient_config.brightness,
            },
            "layers": [
                {
                    "name": l.name,
                    "priority": l.priority.name,
                    "opacity": l.opacity,
                    "color": list(l.color),
                    "effect": l.effect,
                    "ttl": l.ttl,
                    "age": round(time.monotonic() - l.created_at, 1) if l.created_at else 0,
                }
                for l in sorted(self._layers.values(), key=lambda l: l.priority)
            ],
        }


# ── Helpers ──────────────────────────────────────────────────────────────────

def _blend(base: RGB, overlay: RGB, opacity: float) -> RGB:
    """Alpha-blend overlay onto base."""
    if opacity >= 1.0:
        return overlay
    if opacity <= 0.0:
        return base
    inv = 1.0 - opacity
    return (
        int(base[0] * inv + overlay[0] * opacity),
        int(base[1] * inv + overlay[1] * opacity),
        int(base[2] * inv + overlay[2] * opacity),
    )


def _dim(color: RGB, brightness: float) -> RGB:
    """Scale a colour by brightness (0.0-1.0)."""
    return (
        int(color[0] * brightness),
        int(color[1] * brightness),
        int(color[2] * brightness),
    )
