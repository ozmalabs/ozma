# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
V1.2 Moonlight: Scenario-to-app mapping.

This module enables Ozma scenarios to appear as Moonlight app list entries.
When a user selects an app in Moonlight, the corresponding scenario is
activated and streaming is started.

Architecture
────────────
Ozma scenarios (defined in scenarios.json) are mapped to Moonlight "apps".
Each scenario becomes one app in the Moonlight client's app list.

On app selection:
  1. Moonlight sends "launch app" request with app_id = scenario_id
  2. MoonlightAppMapper activates the scenario via ScenarioManager
  3. Scenario activation triggers Sunshine streaming for the bound node
  4. Moonlight receives video stream from Sunshine

Moonlight app format (simplified):
  {
    "apps": [
      {
        "appId": "work",
        "name": "Work",
        "icon": "https://.../icon.png",
        "launchUri": "/launch/work"
      }
    ]
  }

When Moonlight sends POST /launch/work:
  - The controller activates scenario "work"
  - Sunshine starts streaming if not already running

Node types and app generation:
  - Physical node (HDMI capture) → app with capture_source
  - VM node (VNC) → app with VNC configuration
  - Container (virtual desktop) → app with virtual desktop config
  - No bound node → disabled app entry (graceful degradation)
"""

from __future__ import annotations

import logging
from dataclasses import dataclass, field
from typing import Any

log = logging.getLogger("ozma.controller.gaming.moonlight_app_mapping")


@dataclass
class MoonlightApp:
    """
    Represents one app in the Moonlight app list.

    Each app corresponds to one Ozma scenario. The app_id matches the
    scenario_id, allowing Moonlight to launch the correct scenario.
    """
    app_id: str          # Matches scenario.id
    name: str            # Display name
    icon_url: str | None = None   # Optional icon URL for UI
    node_id: str | None = None    # Bound node for streaming
    capture_source: str | None = None  # HDMI capture source if applicable
    vnc_host: str | None = None   # VNC host for VMs
    vnc_port: int | None = None   # VNC port

    def to_dict(self) -> dict[str, Any]:
        """Export as Moonlight-compatible dict."""
        result = {
            "appId": self.app_id,
            "name": self.name,
        }
        if self.icon_url:
            result["icon"] = self.icon_url
        if self.node_id:
            result["node_id"] = self.node_id
        if self.capture_source:
            result["captureSource"] = self.capture_source
        if self.vnc_host:
            result["vncHost"] = self.vnc_host
        if self.vnc_port:
            result["vncPort"] = self.vnc_port
        return result


class MoonlightAppMapper:
    """
    Maps Ozma scenarios to Moonlight app list entries.

    This mapper reads the current scenario list and generates a Moonlight-
    compatible app list. Each scenario becomes one app entry.

    On app launch (via POST /api/v1/moonlight/launch/{app_id}):
      - Activates the corresponding scenario
      - Triggers streaming for the bound node
    """

    def __init__(self, scenarios: Any, sunshine: Any = None) -> None:
        """
        Initialize the app mapper.

        Args:
            scenarios: ScenarioManager instance
            sunshine: SunshineManager instance for starting streaming
        """
        self._scenarios = scenarios
        self._sunshine = sunshine
        self._apps_cache: list[MoonlightApp] = []
        self._cache_valid = False

    # ------------------------------------------------------------------
    # App list generation
    # ------------------------------------------------------------------

    def get_app_list(self) -> list[dict[str, Any]]:
        """
        Generate the Moonlight app list from current scenarios.

        Each scenario becomes one app entry. If a scenario has no bound
        node, it appears as a disabled/placeholder entry (Moonlight can
        show it as unavailable).

        Returns:
            List of Moonlight-compatible app dicts
        """
        # Check if cache needs refresh
        if not self._cache_valid:
            self._apps_cache = self._build_app_list()
            self._cache_valid = True

        return [app.to_dict() for app in self._apps_cache]

    def _build_app_list(self) -> list[MoonlightApp]:
        """Build the internal app list from scenarios."""
        apps: list[MoonlightApp] = []
        scenario_list = self._scenarios.list()

        log.debug("Building Moonlight app list from %d scenarios", len(scenario_list))

        for scenario_data in scenario_list:
            app = self._scenario_to_app(scenario_data)
            if app:
                apps.append(app)

        log.info("Generated %d Moonlight apps from scenarios", len(apps))
        return apps

    def _scenario_to_app(self, scenario_data: dict[str, Any]) -> MoonlightApp | None:
        """
        Convert a scenario dict to a Moonlight app.

        Args:
            scenario_data: Scenario dict from ScenarioManager.list()

        Returns:
            MoonlightApp if scenario is streamable, None otherwise
        """
        scenario_id = scenario_data.get("id", "")
        name = scenario_data.get("name", scenario_id)
        node_id = scenario_data.get("node_id")
        color = scenario_data.get("color", "#888888")
        capture_source = scenario_data.get("capture_source")
        capture_sources = scenario_data.get("capture_sources")

        # Build icon URL using scenario color
        # Moonlight clients can display custom icons; we use a placeholder
        # based on the scenario color
        icon_url = self._build_icon_url(scenario_id, color)

        # Determine app properties based on node type
        vnc_host = None
        vnc_port = None
        app_capture_source = capture_source

        # If there's a bound node, try to get VNC info
        if node_id and self._scenarios._state:
            node = self._scenarios._state.nodes.get(node_id)
            if node:
                # Check if this is a VNC-based node (VM)
                if node.vnc_host:
                    vnc_host = node.vnc_host
                    vnc_port = node.vnc_port or 5900
                    # For VNC nodes, prefer VNC over HDMI capture
                    app_capture_source = None

        return MoonlightApp(
            app_id=scenario_id,
            name=name,
            icon_url=icon_url,
            node_id=node_id,
            capture_source=app_capture_source,
            vnc_host=vnc_host,
            vnc_port=vnc_port,
        )

    def _build_icon_url(self, scenario_id: str, color: str) -> str:
        """
        Build a placeholder icon URL for a scenario.

        Moonlight clients can display custom app icons. We generate a
        simple SVG icon based on the scenario color.

        Args:
            scenario_id: The scenario identifier
            color: The scenario's accent color (hex)

        Returns:
            Data URI containing SVG icon
        """
        # Simple colored circle SVG
        svg = f'''<svg xmlns="http://www.w3.org/2000/svg" width="64" height="64" viewBox="0 0 64 64">
  <circle cx="32" cy="32" r="30" fill="{color}" fill-opacity="0.8"/>
  <text x="32" y="42" font-family="Arial" font-size="24" fill="white" text-anchor="middle" dominant-baseline="middle">🎮</text>
</svg>'''
        # URL encode the SVG (simple version)
        import urllib.parse
        encoded = urllib.parse.quote(svg.replace('\n', ' ').replace('"', "'"))
        return f"data:image/svg+xml,{encoded}"

    # ------------------------------------------------------------------
    # App launch handling
    # ------------------------------------------------------------------

    async def launch_app(self, app_id: str) -> dict[str, Any]:
        """
        Launch a Moonlight app by activating its scenario.

        When Moonlight sends a launch request for an app:
          1. Find the scenario with matching app_id
          2. Activate the scenario (switches HID, audio, RGB, etc.)
          3. If Sunshine is available, enable streaming for the node

        Args:
            app_id: The scenario ID to launch

        Returns:
            Result dict with success status and app details
        """
        scenario = self._scenarios.get(app_id)
        if not scenario:
            return {
                "ok": False,
                "error": f"Scenario not found: {app_id}",
            }

        # Activate the scenario
        try:
            await self._scenarios.activate(app_id)
        except Exception as e:
            return {
                "ok": False,
                "error": f"Failed to activate scenario: {e}",
            }

        # If Sunshine is available, start streaming
        result: dict[str, Any] = {"ok": True, "scenario_id": app_id}

        if self._sunshine and scenario.node_id:
            try:
                # Check if Sunshine is already enabled for this node
                current_config = self._sunshine.get_config(scenario.node_id)
                if current_config and current_config.enabled:
                    result["streaming_enabled"] = False
                    result["reason"] = "Streaming already enabled"
                else:
                    # No config or streaming not enabled — enable it
                    await self._sunshine.enable_node(scenario.node_id)
                    result["streaming_enabled"] = True
            except Exception as e:
                log.warning("Failed to enable streaming for %s: %s", scenario.node_id, e)
                result["streaming_warning"] = str(e)

        log.info("Launched app '%s' (scenario: %s)", app_id, app_id)
        return result

    # ------------------------------------------------------------------
    # Cache invalidation
    # ------------------------------------------------------------------

    def invalidate_cache(self) -> None:
        """Mark the app list cache as invalid. Called on scenario changes."""
        self._cache_valid = False

    # ------------------------------------------------------------------
    # Utility
    # ------------------------------------------------------------------

    def get_app_by_id(self, app_id: str) -> MoonlightApp | None:
        """
        Get a specific app by ID without rebuilding the list.

        Args:
            app_id: The app identifier

        Returns:
            MoonlightApp if found, None otherwise
        """
        for app in self._apps_cache:
            if app.app_id == app_id:
                return app
        return None


def create_app_mapper(scenarios: Any, sunshine: Any = None) -> MoonlightAppMapper:
    """
    Factory function to create a MoonlightAppMapper.

    Args:
        scenarios: ScenarioManager instance
        sunshine: SunshineManager instance (optional)

    Returns:
        Configured MoonlightAppMapper instance
    """
    return MoonlightAppMapper(scenarios, sunshine)
