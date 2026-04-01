# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Sensor descriptions — turn dry metrics into personality.

Instead of "23.5°C" the dashboard says "Comfortable". Instead of
"CPU 95%" it says "Your system is on fire 🔥". Instead of "45% RH"
it says "A bit dry, consider a humidifier".

Description packs are shareable bundles of personality:
  plain       — factual, professional ("Normal operating temperature")
  friendly    — conversational ("Nice and cozy in here")
  sweary      — explicit ("It's hot as f***")
  pirate      — arrr ("The hold be warm, cap'n")
  haiku       — poetic ("Warm silicon hums / electrons dance through copper / all systems are go")
  sysadmin    — dry IT humour ("Within SLA parameters. For now.")
  emoji       — pure emoji ("🌡️ 😌 👍")
  custom      — user-defined

Each description pack maps a sensor type + value range to a text string.
Multiple packs can be active simultaneously (e.g., plain for the dashboard,
sweary for Slack notifications, pirate for the Stream Deck).

The system is generic — it works on ANY metric, not just temperature:
  - Temperature: "Chilly" / "Comfortable" / "Hot AF!"
  - CPU usage: "Idle" / "Working hard" / "Maxed out, send help"
  - Disk usage: "Plenty of room" / "Getting tight" / "DANGER: almost full"
  - RAM: "Breathing easy" / "Loaded up" / "Swapping to disk, pray"
  - Network: "Quiet" / "Busy" / "Saturated"
  - Power: "Sipping" / "Nominal" / "Drawing serious power"
  - Humidity: "Desert dry" / "Just right" / "Tropical"
  - USB voltage: "Rock solid" / "A bit wobbly" / "PSU is dying"

Users can create, edit, share, and download description packs via
Ozma Connect, just like widget packs.
"""

from __future__ import annotations

import json
import logging
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.sensor_descriptions")


@dataclass
class DescriptionRange:
    """A value range with a description string."""
    min_val: float
    max_val: float
    text: str
    emoji: str = ""
    severity: str = "ok"  # ok, info, warn, danger


@dataclass
class MetricDescriptions:
    """Descriptions for one metric type (e.g., temperature, cpu_usage)."""
    metric_pattern: str          # Regex or exact match: "temperature", "cpu_usage", "*.humidity"
    unit: str = ""
    ranges: list[DescriptionRange] = field(default_factory=list)

    def describe(self, value: float) -> dict[str, str]:
        """Find the description for a value. Returns {"text": ..., "emoji": ..., "severity": ...}."""
        for r in self.ranges:
            if r.min_val <= value < r.max_val:
                return {"text": r.text, "emoji": r.emoji, "severity": r.severity}
        # Fallback
        return {"text": f"{value:.1f}", "emoji": "", "severity": "ok"}


@dataclass
class DescriptionPack:
    """A shareable bundle of metric descriptions with personality."""

    id: str
    name: str
    author: str = ""
    description: str = ""
    version: str = "1.0.0"
    tags: list[str] = field(default_factory=list)
    metrics: dict[str, MetricDescriptions] = field(default_factory=dict)

    def describe(self, metric_key: str, value: float) -> dict[str, str]:
        """Get a description for a metric value."""
        # Try exact match first
        md = self.metrics.get(metric_key)
        if md:
            return md.describe(value)
        # Try pattern match (e.g., "temperature" matches "sensor.bme280.temperature")
        for pattern, md in self.metrics.items():
            if metric_key.endswith(pattern) or pattern in metric_key:
                return md.describe(value)
        return {"text": f"{value:.1f}", "emoji": "", "severity": "ok"}

    def to_dict(self) -> dict[str, Any]:
        return {"id": self.id, "name": self.name, "author": self.author,
                "description": self.description, "version": self.version, "tags": self.tags}

    @classmethod
    def from_dict(cls, d: dict) -> "DescriptionPack":
        metrics = {}
        for key, mdata in d.get("metrics", {}).items():
            ranges = [DescriptionRange(**r) for r in mdata.get("ranges", [])]
            metrics[key] = MetricDescriptions(
                metric_pattern=key, unit=mdata.get("unit", ""), ranges=ranges,
            )
        return cls(
            id=d.get("id", ""), name=d.get("name", ""), author=d.get("author", ""),
            description=d.get("description", ""), version=d.get("version", "1.0.0"),
            tags=d.get("tags", []), metrics=metrics,
        )


# ── Built-in description packs ──────────────────────────────────────────────

def _r(lo: float, hi: float, text: str, emoji: str = "", severity: str = "ok") -> dict:
    return {"min_val": lo, "max_val": hi, "text": text, "emoji": emoji, "severity": severity}


_PLAIN = {
    "id": "plain", "name": "Plain", "author": "ozmalabs",
    "description": "Professional, factual descriptions",
    "tags": ["professional", "default"],
    "metrics": {
        "temperature": {"unit": "°C", "ranges": [
            _r(-40, 5, "Very cold", "🥶", "warn"), _r(5, 15, "Cold", "❄️"),
            _r(15, 22, "Cool", "🌤️"), _r(22, 26, "Normal", "✅"),
            _r(26, 30, "Warm", "☀️"), _r(30, 40, "Hot", "🔥", "warn"),
            _r(40, 100, "Extreme heat", "🚨", "danger"),
        ]},
        "humidity": {"unit": "%", "ranges": [
            _r(0, 20, "Very dry", "🏜️", "warn"), _r(20, 35, "Dry", "💨"),
            _r(35, 55, "Comfortable", "✅"), _r(55, 70, "Humid", "💧"),
            _r(70, 100, "Very humid", "🌊", "warn"),
        ]},
        "cpu_usage": {"unit": "%", "ranges": [
            _r(0, 10, "Idle", "😴"), _r(10, 40, "Light load", "✅"),
            _r(40, 70, "Moderate load", "⚙️"), _r(70, 90, "Heavy load", "🔥", "warn"),
            _r(90, 101, "Maxed out", "🚨", "danger"),
        ]},
        "cpu_temp": {"unit": "°C", "ranges": [
            _r(0, 40, "Cool", "❄️"), _r(40, 60, "Normal", "✅"),
            _r(60, 80, "Warm", "☀️", "info"), _r(80, 95, "Hot", "🔥", "warn"),
            _r(95, 120, "Critical", "🚨", "danger"),
        ]},
        "gpu_temp": {"unit": "°C", "ranges": [
            _r(0, 45, "Cool", "❄️"), _r(45, 65, "Normal", "✅"),
            _r(65, 85, "Warm", "☀️", "info"), _r(85, 100, "Hot", "🔥", "warn"),
            _r(100, 120, "Critical", "🚨", "danger"),
        ]},
        "ram_pct": {"unit": "%", "ranges": [
            _r(0, 30, "Plenty free", "✅"), _r(30, 60, "Normal", "⚙️"),
            _r(60, 85, "Getting full", "⚠️", "warn"), _r(85, 95, "Low memory", "🔥", "warn"),
            _r(95, 101, "Critical", "🚨", "danger"),
        ]},
        "disk_pct": {"unit": "%", "ranges": [
            _r(0, 50, "Plenty of space", "✅"), _r(50, 75, "Normal", "⚙️"),
            _r(75, 90, "Getting full", "⚠️", "warn"), _r(90, 95, "Low space", "🔥", "warn"),
            _r(95, 101, "Critical — almost full", "🚨", "danger"),
        ]},
        "voltage": {"unit": "V", "ranges": [
            _r(0, 4.5, "Low voltage", "⚠️", "danger"), _r(4.5, 4.8, "Below spec", "⚠️", "warn"),
            _r(4.8, 5.3, "Normal", "✅"), _r(5.3, 6.0, "High", "⚠️", "warn"),
        ]},
        "power_draw": {"unit": "W", "ranges": [
            _r(0, 5, "Sipping", "💤"), _r(5, 50, "Light", "✅"),
            _r(50, 150, "Moderate", "⚙️"), _r(150, 300, "Heavy", "🔥", "info"),
            _r(300, 1000, "Very high", "⚡", "warn"),
        ]},
    },
}

_FRIENDLY = {
    "id": "friendly", "name": "Friendly", "author": "ozmalabs",
    "description": "Conversational, warm descriptions",
    "tags": ["conversational", "casual"],
    "metrics": {
        "temperature": {"unit": "°C", "ranges": [
            _r(-40, 5, "Brrr! Bundle up!", "🥶", "warn"), _r(5, 15, "It's a bit chilly", "❄️"),
            _r(15, 22, "Nice and fresh", "🌤️"), _r(22, 26, "Perfect! Nice and cozy", "😊"),
            _r(26, 30, "Getting toasty", "☀️"), _r(30, 40, "It's really hot in here!", "🥵", "warn"),
            _r(40, 100, "Dangerously hot — do something!", "🚨", "danger"),
        ]},
        "cpu_usage": {"unit": "%", "ranges": [
            _r(0, 10, "Chilling out, nothing to do", "😴"), _r(10, 40, "Ticking along nicely", "😊"),
            _r(40, 70, "Working hard for you", "💪"), _r(70, 90, "Phew! This is intense", "😰", "warn"),
            _r(90, 101, "I'm giving it everything I've got!", "🤯", "danger"),
        ]},
        "humidity": {"unit": "%", "ranges": [
            _r(0, 20, "Dry as a bone — maybe get a humidifier?", "🏜️", "warn"),
            _r(20, 35, "A bit dry in here", "💨"),
            _r(35, 55, "Just right, Goldilocks", "😊"),
            _r(55, 70, "Getting a bit muggy", "💧"),
            _r(70, 100, "It's a tropical rainforest in here!", "🌴", "warn"),
        ]},
    },
}

_SWEARY = {
    "id": "sweary", "name": "Sweary", "author": "ozmalabs",
    "description": "For adults only. Strong language.",
    "tags": ["adult", "explicit", "fun"],
    "metrics": {
        "temperature": {"unit": "°C", "ranges": [
            _r(-40, 5, "It's bloody freezing!", "🥶", "warn"), _r(5, 15, "Cold as balls", "❄️"),
            _r(15, 22, "A bit nippy", "🌤️"), _r(22, 26, "Just right, no complaints", "👌"),
            _r(26, 30, "Getting warm, innit", "☀️"), _r(30, 40, "Hot as f***!", "🥵", "warn"),
            _r(40, 100, "What the f*** is happening?!", "🚨", "danger"),
        ]},
        "cpu_usage": {"unit": "%", "ranges": [
            _r(0, 10, "Doing sweet FA", "😴"), _r(10, 40, "Plodding along", "🤷"),
            _r(40, 70, "Working its arse off", "💪"), _r(70, 90, "Absolutely caning it", "😰", "warn"),
            _r(90, 101, "It's f***ed, mate", "💀", "danger"),
        ]},
    },
}

_PIRATE = {
    "id": "pirate", "name": "Pirate", "author": "ozmalabs",
    "description": "Arrr! Set sail with these descriptions, ye scallywag!",
    "tags": ["fun", "pirate", "themed"],
    "metrics": {
        "temperature": {"unit": "°C", "ranges": [
            _r(-40, 5, "Colder than Davy Jones' locker!", "🏴‍☠️", "warn"),
            _r(5, 15, "A brisk wind off the bow", "🌊"),
            _r(15, 22, "Fair weather, cap'n", "⛵"), _r(22, 26, "Smooth sailing", "😎"),
            _r(26, 30, "The sun be beatin' down", "☀️"),
            _r(30, 40, "Hotter than a cannon barrel!", "🔥", "warn"),
            _r(40, 100, "Abandon ship! She's ablaze!", "🚨", "danger"),
        ]},
        "cpu_usage": {"unit": "%", "ranges": [
            _r(0, 10, "All hands sleeping", "💤"), _r(10, 40, "Crew's on light duties", "⚓"),
            _r(40, 70, "All hands on deck!", "⛵"), _r(70, 90, "Battle stations!", "⚔️", "warn"),
            _r(90, 101, "We're takin' on water!", "🏴‍☠️", "danger"),
        ]},
    },
}

_SYSADMIN = {
    "id": "sysadmin", "name": "Sysadmin", "author": "ozmalabs",
    "description": "Dry IT humour. You've been there.",
    "tags": ["IT", "humour", "professional"],
    "metrics": {
        "temperature": {"unit": "°C", "ranges": [
            _r(-40, 5, "HVAC has opinions about efficiency", "❄️", "warn"),
            _r(5, 15, "Someone left the DC door open again", "🚪"),
            _r(15, 22, "Within spec. Don't touch anything.", "📋"),
            _r(22, 26, "Nominal. This won't last.", "✅"),
            _r(26, 30, "AC is struggling. Budget request incoming.", "💰", "info"),
            _r(30, 40, "Above threshold. Time to write an incident report.", "📝", "warn"),
            _r(40, 100, "P1 INCIDENT. Update the status page.", "🚨", "danger"),
        ]},
        "cpu_usage": {"unit": "%", "ranges": [
            _r(0, 10, "Overprovisioned. Don't tell management.", "🤫"),
            _r(10, 40, "Within SLA parameters. For now.", "📊"),
            _r(40, 70, "That cron job is running again", "⚙️"),
            _r(70, 90, "Someone deployed to prod on a Friday", "😱", "warn"),
            _r(90, 101, "It's always DNS. Except when it's this.", "💀", "danger"),
        ]},
        "disk_pct": {"unit": "%", "ranges": [
            _r(0, 50, "Plenty of room. Won't last.", "📁"),
            _r(50, 75, "Someone's logs aren't rotating", "📋"),
            _r(75, 90, "Remember when we said we'd archive?", "⚠️", "warn"),
            _r(90, 95, "JIRA ticket from 2019 finally relevant", "📝", "warn"),
            _r(95, 101, "SELECT * FROM panic WHERE disk_full = true", "🚨", "danger"),
        ]},
    },
}


# ── Description pack manager ─────────────────────────────────────────────────

class DescriptionPackManager:
    """Manages description packs — built-in + user-installed."""

    def __init__(self) -> None:
        self._packs: dict[str, DescriptionPack] = {}
        self._active_pack_id: str = "plain"
        self._packs_dir = Path(__file__).parent / "description_packs"

        # Load built-ins
        for data in [_PLAIN, _FRIENDLY, _SWEARY, _PIRATE, _SYSADMIN]:
            pack = DescriptionPack.from_dict(data)
            self._packs[pack.id] = pack

        # Load user packs from disk
        self._load_user_packs()

    def _load_user_packs(self) -> None:
        if not self._packs_dir.exists():
            return
        for f in self._packs_dir.glob("*.json"):
            try:
                data = json.loads(f.read_text())
                pack = DescriptionPack.from_dict(data)
                self._packs[pack.id] = pack
            except Exception as e:
                log.debug("Failed to load description pack %s: %s", f.name, e)

    @property
    def active_pack(self) -> DescriptionPack:
        return self._packs.get(self._active_pack_id, self._packs["plain"])

    def set_active(self, pack_id: str) -> bool:
        if pack_id in self._packs:
            self._active_pack_id = pack_id
            return True
        return False

    def describe(self, metric_key: str, value: float, pack_id: str = "") -> dict[str, str]:
        """Get a human description for a metric value."""
        pack = self._packs.get(pack_id) if pack_id else self.active_pack
        if not pack:
            pack = self.active_pack
        return pack.describe(metric_key, value)

    def describe_all(self, metrics: dict[str, float], pack_id: str = "") -> dict[str, dict[str, str]]:
        """Describe all metrics in a dict."""
        return {key: self.describe(key, val, pack_id) for key, val in metrics.items()}

    def list_packs(self) -> list[dict[str, Any]]:
        return [p.to_dict() for p in self._packs.values()]

    def install_pack(self, data: dict) -> bool:
        """Install a user description pack."""
        pack = DescriptionPack.from_dict(data)
        if not pack.id:
            return False
        self._packs[pack.id] = pack
        # Persist
        self._packs_dir.mkdir(parents=True, exist_ok=True)
        (self._packs_dir / f"{pack.id}.json").write_text(json.dumps(data, indent=2))
        return True

    def remove_pack(self, pack_id: str) -> bool:
        if pack_id in ("plain", "friendly", "sweary", "pirate", "sysadmin"):
            return False  # Can't remove built-ins
        self._packs.pop(pack_id, None)
        path = self._packs_dir / f"{pack_id}.json"
        if path.exists():
            path.unlink()
        return True
