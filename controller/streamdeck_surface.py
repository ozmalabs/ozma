# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Elgato Stream Deck control surface driver for ozma.

Maps each key to a scenario.  Key images show the scenario name and colour.
The active scenario's key is highlighted.  Pressing a key activates that
scenario.

Supports all Stream Deck models:
  - Stream Deck Mini (6 keys)
  - Stream Deck Original / V2 (15 keys)
  - Stream Deck XL (32 keys)
  - Stream Deck Pedal (3 foot switches, no display)

Requires: pip install elgato-streamdeck pillow
"""

from __future__ import annotations

import asyncio
import logging
from typing import Any

from controls import ControlSurface, Control, ControlBinding, DisplayControl

log = logging.getLogger("ozma.streamdeck")

try:
    from StreamDeck.DeviceManager import DeviceManager
    from StreamDeck.ImageHelpers import PILHelper
    _SD_AVAILABLE = True
except ImportError:
    _SD_AVAILABLE = False

try:
    from PIL import Image, ImageDraw, ImageFont
    _PIL_AVAILABLE = True
except ImportError:
    _PIL_AVAILABLE = False


def discover_streamdecks() -> list:
    """Return list of connected Stream Deck devices."""
    if not _SD_AVAILABLE:
        return []
    try:
        return DeviceManager().enumerate()
    except Exception as e:
        log.debug("Stream Deck enumeration failed: %s", e)
        return []


class StreamDeckSurface(ControlSurface):
    """
    An Elgato Stream Deck registered as an ozma control surface.

    Each key is mapped to a scenario (by index in the scenario list).
    Key images render the scenario name and colour.  The active scenario
    key is highlighted with a bright border.

    For the Stream Deck Pedal (no display), keys are mapped to
    scenario.next / scenario.prev / audio.mute.
    """

    def __init__(self, deck: Any, surface_id: str | None = None) -> None:
        self._deck = deck
        self._is_visual = deck.is_visual()
        self._key_count = 0
        sid = surface_id or f"streamdeck-{deck.deck_type().lower().replace(' ', '-')}"
        super().__init__(sid)

        self._loop: asyncio.AbstractEventLoop | None = None
        self._on_changed: Any = None
        self._scenarios: list[dict] = []     # cached scenario list for rendering
        self._active_scenario_id: str | None = None

    async def start(self) -> None:
        if not _SD_AVAILABLE:
            log.warning("StreamDeck library not available")
            return

        self._loop = asyncio.get_running_loop()

        try:
            self._deck.open()
            self._deck.reset()
            self._key_count = self._deck.key_count()
            self._deck.set_brightness(60)
            self._deck.set_key_callback(self._key_callback)
        except Exception as e:
            log.warning("Failed to open Stream Deck: %s", e)
            return

        # Build controls: one per key
        if not self._is_visual:
            # Pedal mode: 3 keys → prev / next / mute
            self._build_pedal_controls()
        else:
            # Visual mode: keys map to scenarios (built dynamically)
            for i in range(self._key_count):
                ctrl = Control(
                    name=f"key_{i}",
                    surface_id=self.id,
                    binding=ControlBinding(action="scenario.activate"),
                )
                self.controls[f"key_{i}"] = ctrl

        log.info("Stream Deck started: %s (%d keys, visual=%s)",
                 self._deck.deck_type(), self._key_count, self._is_visual)

    async def stop(self) -> None:
        try:
            with self._deck:
                self._deck.reset()
                self._deck.close()
        except Exception:
            pass

    def set_on_changed(self, callback: Any) -> None:
        self._on_changed = callback

    def update_scenarios(self, scenarios: list[dict], active_id: str | None) -> None:
        """Called by ControlManager when scenarios change.  Re-render all keys."""
        self._scenarios = scenarios
        self._active_scenario_id = active_id
        if self._is_visual:
            self._render_all_keys()

    # ── Pedal controls ───────────────────────────────────────────────────────

    def _build_pedal_controls(self) -> None:
        """Stream Deck Pedal: 3 foot switches."""
        self.controls["pedal_left"] = Control(
            name="pedal_left", surface_id=self.id,
            binding=ControlBinding(action="scenario.next", value=-1),
        )
        self.controls["pedal_middle"] = Control(
            name="pedal_middle", surface_id=self.id,
            binding=ControlBinding(action="audio.mute", target="@active"),
        )
        self.controls["pedal_right"] = Control(
            name="pedal_right", surface_id=self.id,
            binding=ControlBinding(action="scenario.next", value=1),
        )

    # ── Key image rendering ──────────────────────────────────────────────────

    def _render_all_keys(self) -> None:
        """Render scenario info onto all keys."""
        if not self._is_visual or not _PIL_AVAILABLE:
            return

        try:
            for i in range(self._key_count):
                if i < len(self._scenarios):
                    sc = self._scenarios[i]
                    is_active = sc.get("id") == self._active_scenario_id
                    image = self._render_scenario_key(sc, is_active)
                else:
                    image = self._render_blank_key()

                with self._deck:
                    self._deck.set_key_image(i, image)
        except Exception as e:
            log.debug("Stream Deck render error: %s", e)

    def _render_scenario_key(self, scenario: dict, is_active: bool) -> bytes:
        """Render a scenario onto a key image."""
        image = PILHelper.create_image(self._deck)
        draw = ImageDraw.Draw(image)

        # Background colour from scenario
        color = scenario.get("color", "#888888")
        try:
            r = int(color[1:3], 16)
            g = int(color[3:5], 16)
            b = int(color[5:7], 16)
        except (ValueError, IndexError):
            r, g, b = 136, 136, 136

        if is_active:
            # Bright background for active scenario
            draw.rectangle([0, 0, image.width, image.height], fill=(r, g, b))
            text_color = (0, 0, 0) if (r + g + b) > 384 else (255, 255, 255)
        else:
            # Dim background with colored border
            draw.rectangle([0, 0, image.width, image.height], fill=(20, 20, 30))
            draw.rectangle([2, 2, image.width - 3, image.height - 3],
                           outline=(r, g, b), width=2)
            text_color = (r, g, b)

        # Draw scenario name
        name = scenario.get("name", scenario.get("id", "?"))
        try:
            font = ImageFont.truetype("/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf", 14)
        except (OSError, IOError):
            font = ImageFont.load_default()

        # Center text
        bbox = draw.textbbox((0, 0), name, font=font)
        tw, th = bbox[2] - bbox[0], bbox[3] - bbox[1]
        x = (image.width - tw) // 2
        y = (image.height - th) // 2
        draw.text((x, y), name, fill=text_color, font=font)

        return PILHelper.to_native_format(self._deck, image)

    def _render_blank_key(self) -> bytes:
        """Render an empty dark key."""
        image = PILHelper.create_image(self._deck)
        draw = ImageDraw.Draw(image)
        draw.rectangle([0, 0, image.width, image.height], fill=(10, 10, 15))
        return PILHelper.to_native_format(self._deck, image)

    # ── Key press callback (runs in Stream Deck's thread) ────────────────────

    def _key_callback(self, deck: Any, key: int, pressed: bool) -> None:
        """Called from Stream Deck's internal thread on key state change."""
        if not pressed or not self._loop or not self._on_changed:
            return

        if not self._is_visual:
            # Pedal mode
            names = ["pedal_left", "pedal_middle", "pedal_right"]
            if key < len(names):
                self._loop.call_soon_threadsafe(
                    asyncio.ensure_future,
                    self._on_changed(self.id, names[key], True),
                )
            return

        # Visual mode: key index → scenario
        if key < len(self._scenarios):
            scenario_id = self._scenarios[key].get("id")
            if scenario_id:
                self._loop.call_soon_threadsafe(
                    asyncio.ensure_future,
                    self._on_changed(self.id, f"key_{key}", scenario_id),
                )

    def to_dict(self) -> dict:
        d = super().to_dict()
        d["deck_type"] = self._deck.deck_type() if self._deck else "unknown"
        d["key_count"] = self._key_count
        d["visual"] = self._is_visual
        return d
