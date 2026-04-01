# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Screen widget library — pluggable, user-editable, downloadable.

Widget packs are installable bundles containing widget type definitions,
rendering code (for both server and native tiers), themes, and options.

Architecture:
  - Built-in widgets ship with ozma (gauge, bar, vu_meter, etc.)
  - User widget packs are installed to controller/widget_packs/
  - Ozma Connect (online) hosts a widget pack marketplace
  - Each pack has: manifest.json, renderer code, options schema, preview

A screen layout is a JSON document describing widgets, their positions,
styles, and metric bindings.  The same definition drives:

  1. Server-rendered frames (Node.js renderer → PNG)
  2. Native on-device rendering (ESP32/Android/endpoint → local draw)
  3. Constrained devices (mapped to available display elements)

Widget types (built-in):

  gauge       Circular arc gauge (CPU temp, fan RPM)
  bar         Horizontal/vertical bar (RAM %, disk usage)
  vu_meter    Audio level meter with peak hold (real-time audio)
  label       Text label with metric interpolation
  sparkline   Mini line chart (last N values from history)
  number      Large numeric display with unit
  icon        Status icon (on/off, warning, etc.)
  scenario    Scenario card (name + colour)
  clock       Time + date
  image       Static or dynamic image (URL or data URI)
  grid        Sub-layout grid (nested widgets)

Layout definition format::

    {
        "id": "gaming-status",
        "name": "Gaming PC Status",
        "width": 480,
        "height": 480,
        "background": "#0a0a0f",
        "refresh_hz": 30,
        "widgets": [
            {
                "type": "gauge",
                "x": 20, "y": 20, "w": 120, "h": 120,
                "metric": "gaming-pc.cpu_temp",
                "min": 0, "max": 100,
                "color": "#5b6fff",
                "warn_color": "#f0b94a",
                "crit_color": "#f26464",
                "label": "CPU",
                "unit": "°C"
            },
            {
                "type": "vu_meter",
                "x": 160, "y": 20, "w": 60, "h": 200,
                "metric": "audio.level_l",
                "orientation": "vertical",
                "peak_hold_ms": 1000,
                "segments": 20,
                "color": "#3ecf8e",
                "warn_color": "#f0b94a",
                "crit_color": "#f26464"
            },
            {
                "type": "bar",
                "x": 20, "y": 160, "w": 120, "h": 16,
                "metric": "gaming-pc.ram_pct",
                "max": 100,
                "color": "#a78bfa",
                "label": "RAM"
            },
            {
                "type": "label",
                "x": 20, "y": 200, "w": 200, "h": 24,
                "text": "{scenario_name}",
                "font_size": 18,
                "color": "{scenario_color}",
                "align": "left"
            },
            {
                "type": "sparkline",
                "x": 20, "y": 240, "w": 200, "h": 60,
                "metric": "gaming-pc.gpu_usage",
                "history_seconds": 60,
                "color": "#38bdf8",
                "fill": true
            },
            {
                "type": "number",
                "x": 250, "y": 20, "w": 200, "h": 80,
                "metric": "gaming-pc.fps",
                "font_size": 48,
                "color": "#fff",
                "unit": "FPS",
                "decimals": 0
            }
        ]
    }

Metric references use source.key notation.  Special variables:
  {scenario_name}   — active scenario name
  {scenario_color}  — active scenario colour hex
  {scenario_id}     — active scenario ID
  {node_name}       — active node short name
  {time}            — current time HH:MM:SS
  {date}            — current date
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any


@dataclass
class Widget:
    """A single widget in a screen layout."""

    type: str              # gauge, bar, vu_meter, label, sparkline, number, icon, scenario, clock, image, grid
    x: int = 0
    y: int = 0
    w: int = 100
    h: int = 100

    # Data binding
    metric: str = ""       # source_id.key (e.g., "gaming-pc.cpu_temp")
    text: str = ""         # For label: interpolated text with {variables}

    # Value range
    min: float = 0.0
    max: float = 100.0

    # Style
    color: str = "#5b6fff"
    warn_color: str = "#f0b94a"
    crit_color: str = "#f26464"
    background: str = ""
    font_size: int = 14
    align: str = "center"  # left, center, right
    unit: str = ""
    label: str = ""
    decimals: int = 1

    # Widget-specific
    orientation: str = "horizontal"  # horizontal, vertical (for bar, vu_meter)
    peak_hold_ms: int = 1000        # vu_meter peak hold duration
    segments: int = 20               # vu_meter segment count
    history_seconds: int = 60        # sparkline history window
    fill: bool = False               # sparkline fill under curve
    icon_name: str = ""              # icon widget: named icon
    image_url: str = ""              # image widget: URL or data URI
    children: list["Widget"] = field(default_factory=list)  # grid: nested widgets

    def to_dict(self) -> dict[str, Any]:
        d: dict[str, Any] = {"type": self.type, "x": self.x, "y": self.y, "w": self.w, "h": self.h}
        # Only include non-default fields to keep payloads small
        if self.metric: d["metric"] = self.metric
        if self.text: d["text"] = self.text
        if self.min != 0: d["min"] = self.min
        if self.max != 100: d["max"] = self.max
        if self.color != "#5b6fff": d["color"] = self.color
        if self.warn_color != "#f0b94a": d["warn_color"] = self.warn_color
        if self.crit_color != "#f26464": d["crit_color"] = self.crit_color
        if self.background: d["background"] = self.background
        if self.font_size != 14: d["font_size"] = self.font_size
        if self.align != "center": d["align"] = self.align
        if self.unit: d["unit"] = self.unit
        if self.label: d["label"] = self.label
        if self.decimals != 1: d["decimals"] = self.decimals
        if self.orientation != "horizontal": d["orientation"] = self.orientation
        if self.type == "vu_meter":
            d["peak_hold_ms"] = self.peak_hold_ms
            d["segments"] = self.segments
        if self.type == "sparkline":
            d["history_seconds"] = self.history_seconds
            if self.fill: d["fill"] = True
        if self.icon_name: d["icon"] = self.icon_name
        if self.image_url: d["url"] = self.image_url
        if self.children: d["children"] = [c.to_dict() for c in self.children]
        return d

    @classmethod
    def from_dict(cls, d: dict) -> "Widget":
        children = [cls.from_dict(c) for c in d.pop("children", [])]
        w = cls(**{k: v for k, v in d.items() if hasattr(cls, k)})
        w.children = children
        return w


@dataclass
class ScreenLayout:
    """A complete screen layout definition."""

    id: str
    name: str = ""
    width: int = 480
    height: int = 480
    background: str = "#0a0a0f"
    refresh_hz: int = 10
    widgets: list[Widget] = field(default_factory=list)

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id, "name": self.name,
            "width": self.width, "height": self.height,
            "background": self.background, "refresh_hz": self.refresh_hz,
            "widgets": [w.to_dict() for w in self.widgets],
        }

    @classmethod
    def from_dict(cls, d: dict) -> "ScreenLayout":
        widgets = [Widget.from_dict(w) for w in d.pop("widgets", [])]
        layout = cls(**{k: v for k, v in d.items() if hasattr(cls, k)})
        layout.widgets = widgets
        return layout

    def get_metric_keys(self) -> set[str]:
        """Return all metric keys referenced by widgets (for data subscriptions)."""
        keys = set()
        for w in self.widgets:
            if w.metric:
                keys.add(w.metric)
            for child in w.children:
                if child.metric:
                    keys.add(child.metric)
        return keys


# ── Widget Pack System ────────────────────────────────────────────────────────

@dataclass
class WidgetPackOption:
    """A configurable option in a widget pack."""
    key: str
    label: str
    type: str = "color"         # color, number, boolean, select, text
    default: Any = ""
    choices: list[str] = field(default_factory=list)  # for select type
    min_val: float | None = None
    max_val: float | None = None

    def to_dict(self) -> dict[str, Any]:
        d: dict[str, Any] = {"key": self.key, "label": self.label, "type": self.type, "default": self.default}
        if self.choices: d["choices"] = self.choices
        if self.min_val is not None: d["min"] = self.min_val
        if self.max_val is not None: d["max"] = self.max_val
        return d


@dataclass
class WidgetPack:
    """
    An installable widget pack — downloaded from Ozma Connect or created locally.

    Pack structure on disk (controller/widget_packs/{pack_id}/):
      manifest.json     — metadata, widget types, options schema
      renderer.js       — Node.js rendering code (for Tier 1 server render)
      native.json       — native render instructions (for Tier 2 ESP32/Android)
      preview.png       — thumbnail preview for the marketplace
      theme.json        — default theme (colours, fonts, effects)

    manifest.json example:
      {
        "id": "retro-gauges",
        "name": "Retro Analog Gauges",
        "version": "1.2.0",
        "author": "ozmalabs",
        "description": "Skeuomorphic analog gauge widgets with needle animation",
        "widget_types": ["retro_gauge", "retro_meter", "retro_dial"],
        "options": [
          {"key": "bezel_color", "label": "Bezel Colour", "type": "color", "default": "#8B7355"},
          {"key": "needle_style", "label": "Needle Style", "type": "select", "default": "classic",
           "choices": ["classic", "modern", "minimal"]},
          {"key": "glow_enabled", "label": "Glow Effect", "type": "boolean", "default": true}
        ],
        "themes": ["dark", "light", "amber", "green-phosphor"],
        "tags": ["retro", "analog", "skeuomorphic"],
        "ozma_connect_url": "https://connect.ozma.io/packs/retro-gauges"
      }
    """

    id: str
    name: str
    version: str = "1.0.0"
    author: str = ""
    description: str = ""
    widget_types: list[str] = field(default_factory=list)
    options: list[WidgetPackOption] = field(default_factory=list)
    option_values: dict[str, Any] = field(default_factory=dict)  # user-set values
    themes: list[str] = field(default_factory=list)
    active_theme: str = ""
    tags: list[str] = field(default_factory=list)
    installed: bool = True
    path: str = ""  # filesystem path

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id, "name": self.name, "version": self.version,
            "author": self.author, "description": self.description,
            "widget_types": self.widget_types,
            "options": [o.to_dict() for o in self.options],
            "option_values": self.option_values,
            "themes": self.themes, "active_theme": self.active_theme,
            "tags": self.tags, "installed": self.installed,
        }

    @classmethod
    def from_manifest(cls, manifest: dict, path: str = "") -> "WidgetPack":
        options = [WidgetPackOption(**o) for o in manifest.get("options", [])]
        return cls(
            id=manifest.get("id", ""),
            name=manifest.get("name", ""),
            version=manifest.get("version", "1.0.0"),
            author=manifest.get("author", ""),
            description=manifest.get("description", ""),
            widget_types=manifest.get("widget_types", []),
            options=options,
            themes=manifest.get("themes", []),
            tags=manifest.get("tags", []),
            path=path,
        )


class WidgetPackManager:
    """
    Manages installed widget packs — load, install, configure, remove.

    Scans controller/widget_packs/ for installed packs.
    Downloads packs from Ozma Connect marketplace.
    """

    def __init__(self) -> None:
        self._packs: dict[str, WidgetPack] = {}
        self._custom_widget_types: dict[str, str] = {}  # widget_type → pack_id
        self._packs_dir = Path(__file__).parent / "widget_packs"

    def load(self) -> None:
        """Scan widget_packs/ directory and load all manifests."""
        if not self._packs_dir.exists():
            self._packs_dir.mkdir(parents=True, exist_ok=True)
            return

        for pack_dir in sorted(self._packs_dir.iterdir()):
            if not pack_dir.is_dir():
                continue
            manifest_path = pack_dir / "manifest.json"
            if not manifest_path.exists():
                continue
            try:
                import json
                manifest = json.loads(manifest_path.read_text())
                pack = WidgetPack.from_manifest(manifest, path=str(pack_dir))

                # Load saved option values
                options_path = pack_dir / "options.json"
                if options_path.exists():
                    pack.option_values = json.loads(options_path.read_text())

                self._packs[pack.id] = pack
                for wt in pack.widget_types:
                    self._custom_widget_types[wt] = pack.id
                log.info("Widget pack loaded: %s v%s (%d types)",
                         pack.name, pack.version, len(pack.widget_types))
            except Exception as e:
                log.warning("Failed to load widget pack %s: %s", pack_dir.name, e)

    def list_packs(self) -> list[dict[str, Any]]:
        return [p.to_dict() for p in self._packs.values()]

    def get_pack(self, pack_id: str) -> WidgetPack | None:
        return self._packs.get(pack_id)

    def set_option(self, pack_id: str, key: str, value: Any) -> bool:
        """Set a widget pack option value."""
        pack = self._packs.get(pack_id)
        if not pack:
            return False
        pack.option_values[key] = value
        # Persist
        if pack.path:
            try:
                import json
                options_path = Path(pack.path) / "options.json"
                options_path.write_text(json.dumps(pack.option_values, indent=2))
            except Exception:
                pass
        return True

    def set_theme(self, pack_id: str, theme: str) -> bool:
        """Activate a theme for a widget pack."""
        pack = self._packs.get(pack_id)
        if not pack or theme not in pack.themes:
            return False
        pack.active_theme = theme
        return True

    def install_pack(self, manifest: dict, files: dict[str, bytes]) -> bool:
        """
        Install a widget pack from uploaded files.

        manifest: parsed manifest.json
        files: {"renderer.js": bytes, "preview.png": bytes, ...}
        """
        pack_id = manifest.get("id", "")
        if not pack_id:
            return False

        pack_dir = self._packs_dir / pack_id
        pack_dir.mkdir(parents=True, exist_ok=True)

        # Write manifest
        import json
        (pack_dir / "manifest.json").write_text(json.dumps(manifest, indent=2))

        # Write files
        for filename, content in files.items():
            (pack_dir / filename).write_bytes(content)

        # Load the pack
        pack = WidgetPack.from_manifest(manifest, path=str(pack_dir))
        self._packs[pack_id] = pack
        for wt in pack.widget_types:
            self._custom_widget_types[wt] = pack_id

        log.info("Widget pack installed: %s v%s", pack.name, pack.version)
        return True

    def remove_pack(self, pack_id: str) -> bool:
        """Remove an installed widget pack."""
        pack = self._packs.pop(pack_id, None)
        if not pack:
            return False
        # Remove custom widget type registrations
        for wt in pack.widget_types:
            self._custom_widget_types.pop(wt, None)
        # Remove files
        if pack.path:
            import shutil
            shutil.rmtree(pack.path, ignore_errors=True)
        return True

    def get_pack_for_widget(self, widget_type: str) -> WidgetPack | None:
        """Find which pack provides a given widget type."""
        pack_id = self._custom_widget_types.get(widget_type)
        return self._packs.get(pack_id) if pack_id else None

    def is_custom_widget(self, widget_type: str) -> bool:
        return widget_type in self._custom_widget_types


from pathlib import Path
import logging
log = logging.getLogger("ozma.screen_widgets")

# ── Built-in layouts ─────────────────────────────────────────────────────────

BUILTIN_LAYOUTS: dict[str, ScreenLayout] = {}


def _register_builtin(layout: ScreenLayout) -> None:
    BUILTIN_LAYOUTS[layout.id] = layout


# Compact status (Stream Deck key size)
_register_builtin(ScreenLayout(
    id="key-cpu", name="CPU Gauge Key", width=72, height=72, refresh_hz=2,
    widgets=[Widget(type="gauge", x=4, y=4, w=64, h=64, metric="@active.cpu_temp", label="CPU", unit="°C")],
))

_register_builtin(ScreenLayout(
    id="key-gpu", name="GPU Gauge Key", width=72, height=72, refresh_hz=2,
    widgets=[Widget(type="gauge", x=4, y=4, w=64, h=64, metric="@active.gpu_temp", label="GPU", unit="°C", color="#3ecf8e")],
))

_register_builtin(ScreenLayout(
    id="key-scenario", name="Scenario Key", width=72, height=72, refresh_hz=1,
    widgets=[Widget(type="scenario", x=0, y=0, w=72, h=72)],
))

# Medium status panel (Corsair LCD / small display)
_register_builtin(ScreenLayout(
    id="panel-status", name="System Status Panel", width=480, height=480, refresh_hz=5,
    widgets=[
        Widget(type="label", x=0, y=10, w=480, h=30, text="{node_name}", font_size=20, color="#fff"),
        Widget(type="gauge", x=40, y=60, w=100, h=100, metric="@active.cpu_usage", label="CPU", unit="%"),
        Widget(type="gauge", x=190, y=60, w=100, h=100, metric="@active.gpu_usage", label="GPU", unit="%", color="#3ecf8e"),
        Widget(type="gauge", x=340, y=60, w=100, h=100, metric="@active.cpu_temp", label="TEMP", unit="°C", color="#f0b94a"),
        Widget(type="bar", x=40, y=190, w=400, h=20, metric="@active.ram_pct", label="RAM", color="#a78bfa"),
        Widget(type="bar", x=40, y=220, w=400, h=20, metric="@active.disk_pct", label="Disk", color="#38bdf8"),
        Widget(type="sparkline", x=40, y=260, w=400, h=80, metric="@active.cpu_usage", history_seconds=120, color="#5b6fff", fill=True),
        Widget(type="label", x=40, y=360, w=400, h=20, text="↓ {net_rx_rate} ↑ {net_tx_rate}", font_size=12, color="#888"),
        Widget(type="label", x=40, y=390, w=400, h=20, text="Power: {power_draw}W", font_size=12, color="#888"),
        Widget(type="label", x=0, y=440, w=480, h=30, text="{scenario_name}", font_size=18, color="{scenario_color}"),
    ],
))

# VU meter pair (audio monitoring)
_register_builtin(ScreenLayout(
    id="vu-stereo", name="Stereo VU Meter", width=240, height=320, refresh_hz=30,
    widgets=[
        Widget(type="vu_meter", x=20, y=20, w=80, h=260, metric="audio.level_l", orientation="vertical", segments=24, peak_hold_ms=1500),
        Widget(type="vu_meter", x=140, y=20, w=80, h=260, metric="audio.level_r", orientation="vertical", segments=24, peak_hold_ms=1500, color="#3ecf8e"),
        Widget(type="label", x=20, y=290, w=80, h=20, text="L", font_size=14, color="#888"),
        Widget(type="label", x=140, y=290, w=80, h=20, text="R", font_size=14, color="#888"),
    ],
))

# NOC wall grid
_register_builtin(ScreenLayout(
    id="noc-wall", name="NOC Wall Grid", width=1920, height=1080, refresh_hz=2,
    widgets=[Widget(type="grid", x=0, y=0, w=1920, h=1080, metric="*")],
))
