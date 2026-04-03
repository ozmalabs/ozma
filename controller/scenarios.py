# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Scenario management.

A scenario is a named configuration that binds a compute node to a logical
"context" (e.g. "work", "gaming", "media"). Switching scenarios switches the
active HID target, and in future will also trigger RGB transitions and audio
crossfades.

Scenarios are defined in scenarios.json (or scenarios.yaml) next to main.py.
The file is watched for changes and reloaded automatically.

Schema (scenarios.json):
  {
    "scenarios": [
      {
        "id": "work",
        "name": "Work",
        "node_id": "milkv-work._ozma._udp.local.",   // mDNS instance name
        "color": "#4A90D9",                           // UI accent colour (optional)
        "transition_in": {                            // optional
          "style": "wave_right",
          "duration_ms": 400
        }
      }
    ],
    "default": "work"   // activated on startup if present (optional)
  }

node_id may also be null / omitted — scenario exists but has no bound node yet.
"""

import asyncio
import json
import logging
import os
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, TYPE_CHECKING

if TYPE_CHECKING:
    from rgb import RGBEngine
    from audio import AudioRouter
    from rgb_outputs import RGBOutputManager
    from motion import MotionManager
    from bluetooth import BluetoothManager

log = logging.getLogger("ozma.scenarios")


@dataclass
class TransitionConfig:
    style: str = "cut"       # cut | wave_right | wave_left | fade | ripple
    duration_ms: int = 400

    @classmethod
    def from_dict(cls, d: dict) -> "TransitionConfig":
        return cls(
            style=d.get("style", "cut"),
            duration_ms=int(d.get("duration_ms", 400)),
        )

    def to_dict(self) -> dict:
        return {"style": self.style, "duration_ms": self.duration_ms}


@dataclass
class Scenario:
    id: str
    name: str
    node_id: str | None = None
    color: str = "#888888"
    transition_in: TransitionConfig = field(default_factory=TransitionConfig)
    motion: dict | None = None         # Motion presets: {"device_id": {"axis": position}}
    bluetooth: dict | None = None      # BT config: {"connect": ["AA:BB:..."], "disconnect": ["XX:XX:..."]}
    capture_source: str | None = None     # Primary display capture source ID: "hdmi-0"
    capture_sources: list[str] | None = None  # Multi-monitor: ["hdmi-0", "hdmi-1"]
    wallpaper: dict | None = None        # Desktop wallpaper: {"mode": "gradient", "color": "#..."}
                                          # Modes: image, color, gradient, url, restore

    @classmethod
    def from_dict(cls, d: dict) -> "Scenario":
        return cls(
            id=d["id"],
            name=d.get("name", d["id"]),
            node_id=d.get("node_id") or None,
            color=d.get("color", "#888888"),
            transition_in=TransitionConfig.from_dict(d.get("transition_in", {})),
            motion=d.get("motion"),
            bluetooth=d.get("bluetooth"),
            capture_source=d.get("capture_source"),
            capture_sources=d.get("capture_sources"),
            wallpaper=d.get("wallpaper"),
        )

    def to_dict(self) -> dict:
        d = {
            "id": self.id,
            "name": self.name,
            "node_id": self.node_id,
            "color": self.color,
            "transition_in": self.transition_in.to_dict(),
        }
        if self.motion:
            d["motion"] = self.motion
        if self.bluetooth:
            d["bluetooth"] = self.bluetooth
        if self.capture_source:
            d["capture_source"] = self.capture_source
        if self.capture_sources:
            d["capture_sources"] = self.capture_sources
        if self.wallpaper:
            d["wallpaper"] = self.wallpaper
        return d


class ScenarioManager:
    def __init__(
        self,
        config_path: Path,
        state: "AppState",  # type: ignore[name-defined]
        rgb_engine: "RGBEngine | None" = None,
        rgb_fps: int = 30,
        audio_router: "AudioRouter | None" = None,
        rgb_outputs: "RGBOutputManager | None" = None,
        motion_manager: "MotionManager | None" = None,
        bluetooth: "BluetoothManager | None" = None,
    ) -> None:
        self._path = config_path
        self._state = state
        self._rgb = rgb_engine
        self._rgb_fps = rgb_fps
        self._audio = audio_router
        self._rgb_outputs = rgb_outputs
        self._motion = motion_manager
        self._bluetooth = bluetooth
        self._scenarios: dict[str, Scenario] = {}
        self._default_id: str | None = None
        self._active_id: str | None = None
        self._active_color: str = "#888888"   # color of currently active scenario
        self._watch_task: asyncio.Task | None = None
        self._transition_task: asyncio.Task | None = None
        self._mtime: float = 0.0
        self._active_path: Path = config_path  # updated by _load() to whichever file was read

    # --- Lifecycle ---

    async def start(self) -> None:
        self._load()
        if self._default_id and self._default_id in self._scenarios:
            await self._activate(self._default_id, announce=False)
        self._watch_task = asyncio.create_task(self._watch_loop(), name="scenario-watch")

    async def stop(self) -> None:
        if self._transition_task:
            self._transition_task.cancel()
        if self._watch_task:
            self._watch_task.cancel()

    # --- Public API ---

    def list(self) -> list[dict]:
        return [s.to_dict() for s in self._scenarios.values()]

    def get(self, scenario_id: str) -> Scenario | None:
        return self._scenarios.get(scenario_id)

    @property
    def active_id(self) -> str | None:
        return self._active_id

    async def activate(self, scenario_id: str) -> Scenario:
        if scenario_id not in self._scenarios:
            raise KeyError(f"Unknown scenario: {scenario_id}")
        return await self._activate(scenario_id, announce=True)

    async def bind_node(self, scenario_id: str, node_id: str | None) -> Scenario:
        """Bind (or unbind) a node to a scenario and persist the change."""
        s = self._scenarios.get(scenario_id)
        if s is None:
            raise KeyError(f"Unknown scenario: {scenario_id}")
        s.node_id = node_id
        self._save()
        log.info("Scenario '%s' bound to node %s", scenario_id, node_id)
        await self._state.events.put({"type": "scenario.updated", "scenario": s.to_dict()})
        return s

    async def create(self, scenario_id: str, name: str, node_id: str | None = None) -> Scenario:
        if scenario_id in self._scenarios:
            raise ValueError(f"Scenario already exists: {scenario_id}")
        s = Scenario(id=scenario_id, name=name, node_id=node_id)
        self._scenarios[scenario_id] = s
        self._save()
        await self._state.events.put({"type": "scenario.created", "scenario": s.to_dict()})
        return s

    async def delete(self, scenario_id: str) -> None:
        if scenario_id not in self._scenarios:
            raise KeyError(f"Unknown scenario: {scenario_id}")
        if scenario_id == self._active_id:
            raise ValueError("Cannot delete the active scenario")
        del self._scenarios[scenario_id]
        self._save()
        await self._state.events.put({"type": "scenario.deleted", "scenario_id": scenario_id})

    # --- Internal ---

    async def _activate(self, scenario_id: str, announce: bool) -> Scenario:
        from rgb import hex_to_rgb, run_transition

        s = self._scenarios[scenario_id]
        prev_id = self._active_id
        prev_color = self._active_color
        self._active_id = scenario_id
        self._active_color = s.color

        # HID switches immediately at t=0
        if s.node_id:
            try:
                await self._state.set_active_node(s.node_id)
            except KeyError:
                log.warning(
                    "Scenario '%s' bound to node '%s' which is not online",
                    scenario_id, s.node_id,
                )
        else:
            log.info("Scenario '%s' has no bound node", scenario_id)

        # Audio follows HID immediately
        if self._audio:
            asyncio.create_task(
                self._audio.on_scenario_activated(s.node_id),
                name="audio-switch",
            )

        # RGB follows scenario colour → push to all zones (nodes, WLED, Art-Net)
        if self._rgb_outputs:
            asyncio.create_task(
                self._rgb_outputs.on_scenario_switch(
                    scenario_color=s.color,
                    active_node_id=s.node_id,
                    all_scenarios=self.list(),
                    effect=s.transition_in.style if s.transition_in.style != "cut" else "solid",
                ),
                name="rgb-output-switch",
            )

        # Motion devices follow scenario presets (desk height, monitor position)
        if self._motion and hasattr(s, 'motion') and s.motion:
            asyncio.create_task(
                self._motion.on_scenario_switch(s.motion),
                name="motion-switch",
            )

        # Bluetooth: connect/disconnect devices per scenario
        if self._bluetooth and hasattr(s, 'bluetooth') and s.bluetooth:
            asyncio.create_task(
                self._bluetooth.on_scenario_switch(s.bluetooth),
                name="bt-switch",
            )

        if not announce:
            log.info("Scenario activated (silent): %s (node: %s)", scenario_id, s.node_id)
            return s

        duration_ms = s.transition_in.duration_ms
        color_from = hex_to_rgb(prev_color)
        color_to = hex_to_rgb(s.color)

        # Fire transitioning event (UI starts its animation)
        await self._state.events.put({
            "type": "scenario.transitioning",
            "scenario": s.to_dict(),
            "previous_id": prev_id,
            "transition": {
                "style": s.transition_in.style,
                "duration_ms": duration_ms,
                "color_from": list(color_from),
                "color_to": list(color_to),
            },
        })

        # Cancel any in-progress transition
        if self._transition_task and not self._transition_task.done():
            self._transition_task.cancel()

        if self._rgb and s.transition_in.style != "cut":
            # Run RGB frames in background; fire scenario.activated when done
            async def _run() -> None:
                try:
                    await run_transition(
                        self._rgb,
                        self._state.events,
                        color_from,
                        color_to,
                        duration_ms,
                        self._rgb_fps,
                    )
                except asyncio.CancelledError:
                    pass
                finally:
                    await self._state.events.put({
                        "type": "scenario.activated",
                        "scenario": s.to_dict(),
                        "previous_id": prev_id,
                    })

            self._transition_task = asyncio.create_task(_run(), name="rgb-transition")
        else:
            # Cut: no RGB transition, just signal done
            await self._state.events.put({
                "type": "scenario.activated",
                "scenario": s.to_dict(),
                "previous_id": prev_id,
            })

        log.info(
            "Scenario activated: %s (node: %s, transition: %s %dms)",
            scenario_id, s.node_id, s.transition_in.style, duration_ms,
        )
        return s

    def _load(self) -> None:
        # Support both .json and .yaml — try the alternate extension if primary missing
        path = self._path
        if not path.exists():
            alt = path.with_suffix('.yaml') if path.suffix == '.json' else path.with_suffix('.json')
            if alt.exists():
                path = alt
            else:
                log.info("No scenarios file at %s — starting empty", self._path)
                return
        try:
            text = path.read_text()
            if path.suffix in ('.yaml', '.yml'):
                import yaml
                data = yaml.safe_load(text) or {}
            else:
                data = json.loads(text)
            self._scenarios = {
                d["id"]: Scenario.from_dict(d)
                for d in data.get("scenarios", [])
            }
            self._default_id = data.get("default")
            self._mtime = path.stat().st_mtime
            self._active_path = path  # remember which file is live
            log.info("Loaded %d scenario(s) from %s", len(self._scenarios), path)
        except Exception as e:
            log.error("Failed to load scenarios from %s: %s", path, e)

    def _save(self) -> None:
        """Save scenarios. Input may be .yaml or .json; saves always write .json."""
        data = {
            "scenarios": [s.to_dict() for s in self._scenarios.values()],
        }
        if self._default_id:
            data["default"] = self._default_id
        # Always save to the .json path regardless of which file was loaded
        save_path = self._path.with_suffix('.json')
        try:
            save_path.write_text(json.dumps(data, indent=2))
            self._mtime = save_path.stat().st_mtime
            self._active_path = save_path
        except Exception as e:
            log.error("Failed to save scenarios to %s: %s", save_path, e)

    async def _watch_loop(self) -> None:
        """Reload scenarios file if it changes on disk."""
        while True:
            await asyncio.sleep(2.0)
            try:
                watch_path = getattr(self, '_active_path', self._path)
                if watch_path.exists():
                    mtime = watch_path.stat().st_mtime
                    if mtime != self._mtime:
                        log.info("%s changed, reloading", watch_path.name)
                        self._load()
            except Exception as e:
                log.debug("Scenario watch error: %s", e)
