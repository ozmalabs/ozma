# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Scenario to Moonlight App mapping.

Maps Ozma scenarios to Moonlight apps so they appear in the Moonlight client.

Features:
  - Map each scenario type to Moonlight app list entry
  - Physical machine → HDMI capture → Moonlight RTP
  - VM → VNC → Moonlight RTP
  - Container → virtual desktop → Moonlight RTP
  - Switching Moonlight app = scenario switch
"""

from __future__ import annotations

import asyncio
import logging
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from .moonlight_server import MoonlightServer, MoonlightApp
from scenarios import Scenario, ScenarioManager

log = logging.getLogger("ozma.controller.gaming.scenario_app_mapping")


# ─── App Mapping Types ───────────────────────────────────────────────────────

class ScenarioType:
    """Scenario types for mapping."""
    PHYSICAL = "physical"     # Physical machine via HDMI capture
    VM = "vm"                 # Virtual machine via VNC
    CONTAINER = "container"   # Container via virtual desktop
    GAME = "game"             # Local game/app
    DESKTOP = "desktop"       # Desktop session


# ─── App Mapping Configuration ───────────────────────────────────────────────

@dataclass
class AppMapping:
    """Mapping between a scenario and a Moonlight app."""
    scenario_id: str
    app_id: int
    app_name: str
    app_type: str  # physical, vm, container, game, desktop
    description: str = ""
    icon_data: str = ""  # Base64-encoded icon
    launch_config: dict[str, Any] = field(default_factory=dict)
    created_at: float = field(default_factory=time.time)
    last_modified: float = field(default_factory=time.time)

    def to_dict(self) -> dict[str, Any]:
        return {
            "scenario_id": self.scenario_id,
            "app_id": self.app_id,
            "app_name": self.app_name,
            "app_type": self.app_type,
            "description": self.description,
            "icon_data": self.icon_data,
            "launch_config": self.launch_config,
            "created_at": self.created_at,
            "last_modified": self.last_modified,
        }


# ─── Scenario to App Mapper ──────────────────────────────────────────────────

class ScenarioAppMapper:
    """
    Maps Ozma scenarios to Moonlight apps.

    Features:
      - Automatic app creation for scenarios
      - Scenario type detection
      - Custom app configuration per scenario type
    """

    def __init__(
        self,
        scenarios: ScenarioManager,
        moonlight_server: MoonlightServer,
        data_dir: Path = Path("/var/lib/ozma/gaming"),
    ):
        self._scenarios = scenarios
        self._moonlight = moonlight_server
        self._data_dir = data_dir
        self._data_dir.mkdir(parents=True, exist_ok=True)

        # Mappings
        self._mappings: dict[str, AppMapping] = {}
        self._next_app_id = 999999

        # Load persisted state
        self._load_state()

    def _load_state(self) -> None:
        """Load mappings from disk."""
        mapping_file = self._data_dir / "mappings.json"
        if mapping_file.exists():
            try:
                import json
                data = json.loads(mapping_file.read_text())
                for mapping_data in data.get("mappings", []):
                    mapping = AppMapping(**mapping_data)
                    self._mappings[mapping.scenario_id] = mapping
                if data.get("next_app_id"):
                    self._next_app_id = data["next_app_id"]
                log.info("Loaded %d app mappings", len(self._mappings))
            except Exception as e:
                log.error("Failed to load mappings: %s", e)

    def _save_state(self) -> None:
        """Save mappings to disk."""
        mapping_file = self._data_dir / "mappings.json"
        try:
            import json
            data = {
                "mappings": [m.to_dict() for m in self._mappings.values()],
                "next_app_id": self._next_app_id,
                "last_save": time.time(),
            }
            mapping_file.write_text(json.dumps(data, indent=2))
        except Exception as e:
            log.error("Failed to save mappings: %s", e)

    def detect_scenario_type(self, scenario: Scenario) -> str:
        """Detect the scenario type based on configuration."""
        config = scenario.config

        # Check scenario type hints
        scenario_type = config.get("scenario_type")
        if scenario_type:
            return scenario_type

        # Detect from source configuration
        source = config.get("source", {})
        source_type = source.get("type")

        if source_type == "v4l2":
            return ScenarioType.PHYSICAL
        elif source_type == "vnc":
            return ScenarioType.VM
        elif source_type in ("container", "docker", "podman"):
            return ScenarioType.CONTAINER
        elif source_type in ("desktop", "wayland", "x11"):
            return ScenarioType.DESKTOP
        elif source_type in ("game", "app"):
            return ScenarioType.GAME

        # Default based on name patterns
        name = scenario.name.lower()
        if "game" in name or "steam" in name:
            return ScenarioType.GAME
        if "vm" in name or "virtual" in name:
            return ScenarioType.VM
        if "container" in name or "docker" in name:
            return ScenarioType.CONTAINER
        if "desktop" in name or "main" in name:
            return ScenarioType.DESKTOP

        return ScenarioType.GAME  # Default

    def create_app_for_scenario(self, scenario_id: str) -> AppMapping | None:
        """Create a Moonlight app for a scenario."""
        scenario = self._scenarios.get(scenario_id)
        if not scenario:
            return None

        scenario_type = self.detect_scenario_type(scenario)

        # Generate app ID
        app_id = self._next_app_id
        self._next_app_id += 1

        # Determine app name and description
        app_name = scenario.name
        description = scenario.description or f"Launch {scenario.name} via Moonlight"

        # Determine launch configuration based on type
        launch_config = self._build_launch_config(scenario, scenario_type)

        mapping = AppMapping(
            scenario_id=scenario_id,
            app_id=app_id,
            app_name=app_name,
            app_type=scenario_type,
            description=description,
            launch_config=launch_config,
        )

        self._mappings[scenario_id] = mapping
        self._save_state()

        # Create Moonlight app
        moonlight_app = MoonlightApp(
            app_id=app_id,
            scenario_id=scenario_id,
            name=app_name,
            description=description,
            is_game=scenario_type in (ScenarioType.GAME, ScenarioType.PHYSICAL),
            launch_command=launch_config.get("command", ""),
            working_dir=launch_config.get("working_dir", ""),
            parameters=launch_config.get("parameters", ""),
        )

        # Add to Moonlight server
        # Note: MoonlightServer.add_scenario_app would be called here
        # if it wasn't already handled by scenario registration

        log.info(
            "Created app %d for scenario %s (type: %s)",
            app_id, scenario_id, scenario_type
        )
        return mapping

    def _build_launch_config(self, scenario: Scenario, scenario_type: str) -> dict[str, Any]:
        """Build launch configuration based on scenario type."""
        source = scenario.config.get("source", {})

        if scenario_type == ScenarioType.VM:
            return {
                "type": "vnc",
                "address": source.get("address", "127.0.0.1"),
                "port": source.get("port", 5900),
                "password": source.get("password", ""),
                "width": source.get("width", 1920),
                "height": source.get("height", 1080),
                "fps": source.get("fps", 60),
                "codec": source.get("codec", "h265"),
            }

        elif scenario_type == ScenarioType.PHYSICAL:
            return {
                "type": "v4l2",
                "device": source.get("device", "/dev/video0"),
                "width": source.get("width", 1920),
                "height": source.get("height", 1080),
                "fps": source.get("fps", 60),
                "format": source.get("format", "NV12"),
            }

        elif scenario_type == ScenarioType.CONTAINER:
            container_cfg = source.get("container", {})
            return {
                "type": "container",
                "image": container_cfg.get("image", ""),
                "command": container_cfg.get("command", ""),
                "volumes": container_cfg.get("volumes", []),
                "gpu": container_cfg.get("gpu", False),
                "network": container_cfg.get("network", "bridge"),
            }

        elif scenario_type == ScenarioType.DESKTOP:
            return {
                "type": "desktop",
                "session": source.get("session", "wayland"),
                "width": source.get("width", 1920),
                "height": source.get("height", 1080),
            }

        else:  # Game/App
            return {
                "type": "game",
                "command": source.get("command", ""),
                "working_dir": source.get("working_dir", ""),
                "env": source.get("env", {}),
            }

    def get_mapping(self, scenario_id: str) -> AppMapping | None:
        """Get the mapping for a scenario."""
        return self._mappings.get(scenario_id)

    def get_mapping_by_app_id(self, app_id: int) -> AppMapping | None:
        """Get the mapping by Moonlight app ID."""
        for mapping in self._mappings.values():
            if mapping.app_id == app_id:
                return mapping
        return None

    def get_all_mappings(self) -> list[AppMapping]:
        """Get all mappings."""
        return list(self._mappings.values())

    def remove_mapping(self, scenario_id: str) -> bool:
        """Remove a mapping."""
        if scenario_id in self._mappings:
            del self._mappings[scenario_id]
            self._save_state()
            return True
        return False

    def update_launch_config(self, scenario_id: str, config: dict[str, Any]) -> bool:
        """Update launch configuration for a scenario."""
        mapping = self._mappings.get(scenario_id)
        if not mapping:
            return False

        mapping.launch_config.update(config)
        mapping.last_modified = time.time()
        self._save_state()
        return True


# ─── App List Provider ───────────────────────────────────────────────────────

class AppListProvider:
    """
    Provides app lists to Moonlight clients based on scenarios.

    Handles:
      - Filtering by user permissions
      - Custom app ordering
      - Dynamic app updates
    """

    def __init__(self, mapper: ScenarioAppMapper, scenarios: ScenarioManager):
        self._mapper = mapper
        self._scenarios = scenarios

    def get_app_list(self, user_id: str | None = None) -> list[dict[str, Any]]:
        """Get the app list for a client."""
        mappings = self._mapper.get_all_mappings()

        apps = []
        for mapping in mappings:
            scenario = self._scenarios.get(mapping.scenario_id)
            if not scenario:
                continue

            # Filter based on user permissions (simplified)
            if user_id and not self._can_user_access(user_id, scenario):
                continue

            app = {
                "appId": mapping.app_id,
                "name": mapping.app_name,
                "description": mapping.description,
                "icon": mapping.icon_data,
                "isGame": mapping.app_type in (ScenarioType.GAME, ScenarioType.PHYSICAL),
                "launchCommand": mapping.launch_config.get("command", ""),
                "workingDir": mapping.launch_config.get("working_dir", ""),
                "parameters": mapping.launch_config.get("parameters", ""),
                "scenarioId": mapping.scenario_id,
                "scenarioType": mapping.app_type,
                "createdAt": mapping.created_at,
            }
            apps.append(app)

        return apps

    def _can_user_access(self, user_id: str, scenario: Scenario) -> bool:
        """Check if a user can access a scenario."""
        # Check scenario visibility
        if scenario.config.get("hidden", False):
            return False

        # Check user permissions (simplified)
        # In production, check user roles and scenario access controls
        return True

    def on_scenario_updated(self, scenario_id: str) -> None:
        """Handle scenario update events."""
        mapping = self._mapper.get_mapping(scenario_id)
        if mapping:
            mapping.last_modified = time.time()
            self._mapper._save_state()
