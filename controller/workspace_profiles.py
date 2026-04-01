# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Workspace profiles — hot-desk, smart rooms, environment-reactive scenarios.

A workspace profile is a complete configuration of the physical and
digital workspace that activates based on context:

  - Who: user identity (NFC badge, Bluetooth phone, login)
  - When: time of day (scheduler)
  - Where: which desk/room (Grid topology)
  - What: environmental conditions (temperature, CO2, occupancy)

Profile includes:
  - Scenario (machine + audio + video routing)
  - Motion presets (desk height, monitor position)
  - Bluetooth devices (headphones, controller)
  - Wallpaper + Wallpaper Engine
  - RGB colour + ambient effect
  - Display brightness (DDC/CI)
  - Screen layouts (what to show on displays/Stream Deck)
  - Description pack (personality)

Triggers:
  nfc        — NFC tag scanned on a node (badge-in)
  bluetooth  — specific phone detected nearby
  schedule   — time-based activation
  sensor     — environmental threshold crossed
  manual     — API / dashboard / control surface
"""

from __future__ import annotations

import asyncio
import logging
import time
from dataclasses import dataclass, field
from typing import Any

log = logging.getLogger("ozma.workspace")


@dataclass
class EnvironmentTrigger:
    """Trigger a profile when a sensor crosses a threshold."""
    sensor_key: str          # e.g., "temperature", "co2", "humidity"
    operator: str            # "above", "below"
    threshold: float
    source: str = ""         # Node/sensor source (empty = any)
    cooldown_s: float = 300  # Don't re-trigger within this window
    _last_fired: float = 0.0

    def check(self, value: float) -> bool:
        if time.monotonic() - self._last_fired < self.cooldown_s:
            return False
        if self.operator == "above" and value > self.threshold:
            self._last_fired = time.monotonic()
            return True
        if self.operator == "below" and value < self.threshold:
            self._last_fired = time.monotonic()
            return True
        return False

    def to_dict(self) -> dict[str, Any]:
        return {"sensor": self.sensor_key, "op": self.operator,
                "threshold": self.threshold, "source": self.source}


@dataclass
class WorkspaceProfile:
    """A complete workspace configuration."""

    id: str
    name: str
    user: str = ""                          # User identifier (for hot-desk)
    scenario_id: str = ""                   # Scenario to activate
    motion: dict | None = None              # Motion presets
    bluetooth: dict | None = None           # BT connect/disconnect
    wallpaper: dict | None = None           # Wallpaper config
    rgb_ambient: dict | None = None         # Ambient effect config
    display_brightness: dict | None = None  # Per-monitor brightness
    screen_layout: str = ""                 # Screen layout ID
    description_pack: str = ""              # Sensor description pack
    triggers: list[dict] = field(default_factory=list)  # Activation triggers
    env_triggers: list[EnvironmentTrigger] = field(default_factory=list)

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id, "name": self.name, "user": self.user,
            "scenario_id": self.scenario_id,
            "triggers": self.triggers,
            "env_triggers": [t.to_dict() for t in self.env_triggers],
        }

    @classmethod
    def from_dict(cls, d: dict) -> "WorkspaceProfile":
        env_triggers = [EnvironmentTrigger(**t) for t in d.pop("env_triggers", [])]
        return cls(env_triggers=env_triggers, **{k: v for k, v in d.items() if hasattr(cls, k)})


class WorkspaceManager:
    """
    Manages workspace profiles and their activation triggers.

    Watches for trigger conditions and activates profiles automatically.
    """

    def __init__(self, state: Any, scenarios: Any, metrics: Any = None) -> None:
        self._state = state
        self._scenarios = scenarios
        self._metrics = metrics
        self._profiles: dict[str, WorkspaceProfile] = {}
        self._active_profile: str = ""
        self._task: asyncio.Task | None = None

    async def start(self) -> None:
        self._task = asyncio.create_task(self._watch_loop(), name="workspace-watch")
        log.info("Workspace manager started (%d profiles)", len(self._profiles))

    async def stop(self) -> None:
        if self._task:
            self._task.cancel()

    def add_profile(self, profile: WorkspaceProfile) -> None:
        self._profiles[profile.id] = profile

    def list_profiles(self) -> list[dict]:
        return [p.to_dict() for p in self._profiles.values()]

    async def activate_profile(self, profile_id: str) -> bool:
        """Activate a workspace profile — sets everything up."""
        profile = self._profiles.get(profile_id)
        if not profile:
            return False

        self._active_profile = profile_id
        log.info("Workspace profile activated: %s", profile.name)

        # Activate the associated scenario (which handles HID, audio, RGB, motion, BT, wallpaper)
        if profile.scenario_id:
            try:
                await self._scenarios.activate(profile.scenario_id)
            except KeyError:
                log.warning("Profile %s: scenario %s not found", profile_id, profile.scenario_id)

        return True

    async def on_nfc_scan(self, tag_id: str) -> bool:
        """Handle an NFC badge scan — find and activate the matching profile."""
        for profile in self._profiles.values():
            for trigger in profile.triggers:
                if trigger.get("type") == "nfc" and trigger.get("tag_id") == tag_id:
                    return await self.activate_profile(profile.id)
        return False

    async def on_phone_detected(self, bt_address: str) -> bool:
        """Handle a Bluetooth phone detection — activate matching profile."""
        for profile in self._profiles.values():
            for trigger in profile.triggers:
                if trigger.get("type") == "bluetooth" and trigger.get("address") == bt_address:
                    return await self.activate_profile(profile.id)
        return False

    async def _watch_loop(self) -> None:
        """Watch for environmental triggers."""
        while True:
            try:
                if self._metrics:
                    for profile in self._profiles.values():
                        for env_trigger in profile.env_triggers:
                            # Check all metric sources for matching sensor key
                            for src in self._metrics.get_all():
                                metrics = src.get("metrics", {})
                                for key, info in metrics.items():
                                    if env_trigger.sensor_key in key:
                                        val = info if isinstance(info, (int, float)) else info.get("value", 0)
                                        if env_trigger.check(val):
                                            log.info("Environment trigger: %s %s %.1f → profile %s",
                                                     key, env_trigger.operator, env_trigger.threshold, profile.name)
                                            await self.activate_profile(profile.id)

                await asyncio.sleep(15)
            except asyncio.CancelledError:
                return
