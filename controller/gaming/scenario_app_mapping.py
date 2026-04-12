# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Scenario to Moonlight app mapping.

Implements Tier 3: scenario_app_mapping component.

Maps Ozma scenarios to Moonlight app list entries:

  Physical machine → HDMI capture → Moonlight RTP
  VM → VNC → Moonlight RTP
  Container → virtual desktop → Moonlight RTP

Each scenario appears as a "Moonlight app" that clients can launch.
Switching Moonlight app = scenario switch.
"""

from __future__ import annotations

import asyncio
import logging
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from scenarios import ScenarioManager, Scenario
from state import AppState, NodeInfo

from .capture_to_moonlight import CaptureToMoonlightManager
from .moonlight_protocol import MoonlightProtocol, SessionData

log = logging.getLogger("ozma.moonlight.scenario_mapping")


class ScenarioAppMapper:
    """
    Maps Ozma scenarios to Moonlight app list entries.

    Provides a unified interface where each scenario is presented as
    a Moonlight app that can be launched and streamed.
    """

    def __init__(
        self,
        scenario_manager: ScenarioManager,
        moonlight_protocol: MoonlightProtocol,
        capture_to_moonlight: CaptureToMoonlightManager | None = None,
    ) -> None:
        self._scenario_manager = scenario_manager
        self._moonlight = moonlight_protocol
        self._capture_to_moonlight = capture_to_moonlight

        self._app_cache: list[dict[str, Any]] = []
        self._cache_timestamp = 0.0

        # Track active streams per scenario
        self._active_streams: dict[str, str] = {}  # scenario_id -> client_id

    async def list_moonlight_apps(self) -> list[dict[str, Any]]:
        """
        Generate Moonlight app list from scenarios.

        Each scenario appears as a Moonlight app with appropriate metadata.
        """
        now = asyncio.get_event_loop().time()
        if now - self._cache_timestamp < 5.0:  # Cache for 5 seconds
            return self._app_cache

        apps = []

        # Get all scenarios
        scenarios = self._scenario_manager.list_scenarios()

        for scenario in scenarios:
            app = self._scenario_to_moonlight_app(scenario)
            if app:
                apps.append(app)

        # Add HDMI capture sources as apps
        if self._capture_to_moonlight:
            capture_apps = await self._capture_to_moonlight.list_moonlight_apps()
            apps.extend(capture_apps)

        self._app_cache = apps
        self._cache_timestamp = now

        return apps

    def _scenario_to_moonlight_app(self, scenario: Scenario) -> dict[str, Any] | None:
        """Convert a scenario to a Moonlight app definition."""
        # Determine node type and create appropriate app entry
        node_id = scenario.node_id
        if not node_id:
            return None

        node = self._scenario_manager._state_ref.get_node(node_id)
        if not node:
            return None

        # Build app entry based on node type
        if node.machine_class == "camera":
            # Camera nodes don't support streaming
            return None

        app = {
            "id": f"scenario:{scenario.id}",
            "name": scenario.name,
            "description": self._build_description(scenario, node),
            "icon": self._get_icon_for_node(node),
            "scenario_id": scenario.id,
            "node_id": node.id,
            "node_type": node.role,
            "machine_class": node.machine_class,
            "supports_streaming": True,
            "current_session": self._get_active_session(scenario.id),
        }

        # Add node-specific metadata
        if node.vnc_host:
            app["vnc_host"] = node.vnc_host
            app["vnc_port"] = node.vnc_port

        if node.stream_port and node.stream_path:
            app["hls_stream"] = f"http://{node.host}:{node.stream_port}{node.stream_path}"

        if node.capture_device:
            app["capture_device"] = node.capture_device

        return app

    def _build_description(self, scenario: Scenario, node: NodeInfo) -> str:
        """Build a user-friendly description for the app."""
        parts = [node.role.capitalize()]

        if node.machine_class:
            parts.append(node.machine_class.capitalize())

        if node.hw:
            parts.append(f"({node.hw})")

        if node.host:
            parts.append(f"@ {node.host}")

        return " ".join(parts)

    def _get_icon_for_node(self, node: NodeInfo) -> str:
        """Get appropriate icon for the node type."""
        icons = {
            "workstation": "desktop",
            "server": "server",
            "kiosk": "display",
            "camera": "video",
            "compute": "computer",
            "presence": "user",
            "display": "monitor",
            "room-mic": "mic",
        }
        return icons.get(node.role, "computer")

    def _get_active_session(self, scenario_id: str) -> dict | None:
        """Get active session info for a scenario."""
        if scenario_id in self._active_streams:
            return {
                "client_id": self._active_streams[scenario_id],
                "active": True,
            }
        return None

    async def launch_app(
        self,
        app_id: str,
        client_id: str,
    ) -> bool:
        """
        Launch a Moonlight app (start a stream).

        This handles:
          - Physical machines via HDMI capture
          - VMs via VNC
          - Containers via virtual desktop

        Returns True if launch succeeded, False otherwise.
        """
        if app_id.startswith("capture:"):
            # HDMI capture app
            if self._capture_to_moonlight:
                return await self._capture_to_moonlight.launch_moonlight_app(
                    app_id, client_id
                )
            return False

        if not app_id.startswith("scenario:"):
            return False

        scenario_id = app_id.split(":", 1)[1]
        return await self._launch_scenario(scenario_id, client_id)

    async def _launch_scenario(
        self,
        scenario_id: str,
        client_id: str,
    ) -> bool:
        """Launch a scenario for streaming."""
        scenario = self._scenario_manager.get_scenario(scenario_id)
        if not scenario:
            log.error("Scenario not found: %s", scenario_id)
            return False

        node_id = scenario.node_id
        node = self._scenario_manager._state_ref.get_node(node_id)
        if not node:
            log.error("Node not found for scenario: %s", scenario_id)
            return False

        # Activate the scenario (make node active)
        await self._scenario_manager.activate_scenario(scenario_id)

        # Update active stream tracking
        self._active_streams[scenario_id] = client_id

        log.info(
            "Launched scenario %s (%s) for client %s",
            scenario.name, scenario_id, client_id
        )

        return True

    async def quit_app(
        self,
        app_id: str,
        client_id: str,
    ) -> bool:
        """
        Quit a Moonlight app (stop a stream).

        Returns True if quit succeeded.
        """
        if app_id.startswith("capture:"):
            if self._capture_to_moonlight:
                return await self._capture_to_moonlight.quit_moonlight_app(
                    app_id, client_id
                )
            return False

        if not app_id.startswith("scenario:"):
            return False

        scenario_id = app_id.split(":", 1)[1]

        # Remove from active streams
        if scenario_id in self._active_streams:
            del self._active_streams[scenario_id]

        log.info(
            "Quit app %s (client %s)",
            app_id, client_id
        )

        return True

    def get_app(self, app_id: str) -> dict[str, Any] | None:
        """Get a specific app definition."""
        apps = self._app_cache or asyncio.get_event_loop().run_until_complete(
            self.list_moonlight_apps()
        )
        for app in apps:
            if app.get("id") == app_id:
                return app
        return None

    async def invalidate_cache(self) -> None:
        """Invalidate the app cache (call when scenarios change)."""
        self._cache_timestamp = 0

    def get_all_apps(self) -> list[dict[str, Any]]:
        """Get all apps (cached)."""
        return self._app_cache.copy()


class HybridStreamingManager:
    """
    Manages unified source adapters for different stream types.

    Provides a common interface for:
      - Physical capture (V4L2)
      - VM streaming (VNC)
      - Container desktop (Wayland/X11)
    """

    def __init__(self, data_dir: Path) -> None:
        self._data_dir = data_dir
        self._data_dir.mkdir(parents=True, exist_ok=True)

        self._source_adapters: dict[str, Any] = {}
        self._active_sources: dict[str, str] = {}  # session_id -> source_type

    async def register_source_adapter(
        self,
        source_type: str,
        adapter: Any,
    ) -> None:
        """Register a source adapter for a stream type."""
        self._source_adapters[source_type] = adapter

    async def get_frame_source(
        self,
        session_id: str,
        source_type: str,
    ) -> Any:
        """Get a frame source adapter for a session."""
        if source_type in self._source_adapters:
            return self._source_adapters[source_type]
        raise ValueError(f"Unknown source type: {source_type}")

    async def start_source(
        self,
        session_id: str,
        source_type: str,
        **kwargs,
    ) -> bool:
        """Start a source adapter for a session."""
        if source_type not in self._source_adapters:
            return False

        adapter = self._source_adapters[source_type]
        result = await adapter.start(session_id, **kwargs)
        if result:
            self._active_sources[source_type] = session_id
        return result

    async def stop_source(
        self,
        session_id: str,
        source_type: str,
    ) -> bool:
        """Stop a source adapter for a session."""
        if source_type not in self._source_adapters:
            return False

        adapter = self._source_adapters[source_type]
        result = await adapter.stop(session_id)
        if session_id in self._active_sources:
            del self._active_sources[session_id]
        return result

    def get_active_sources(self) -> list[str]:
        """Get list of active source types."""
        return list(self._active_sources.keys())


# ── Factory function ─────────────────────────────────────────────────────────

def create_scenario_app_mapper(
    scenario_manager: ScenarioManager,
    moonlight_protocol: MoonlightProtocol,
    capture_to_moonlight: CaptureToMoonlightManager | None = None,
) -> ScenarioAppMapper:
    """
    Factory function to create a ScenarioAppMapper.
    """
    return ScenarioAppMapper(
        scenario_manager, moonlight_protocol, capture_to_moonlight
    )


# ── Adapter base class ───────────────────────────────────────────────────────

class FrameSourceAdapter:
    """
    Base class for source adapters that provide frames to GStreamer pipelines.

    Subclasses implement:
      - start(session_id, **kwargs) → bool
      - stop(session_id) → bool
      - get_pipeline_source() → str
    """

    def __init__(self, source_type: str) -> None:
        self._source_type = source_type
        self._sessions: dict[str, Any] = {}

    @property
    def source_type(self) -> str:
        return self._source_type

    async def start(self, session_id: str, **kwargs) -> bool:
        """Start the source for a session."""
        self._sessions[session_id] = kwargs
        return True

    async def stop(self, session_id: str) -> bool:
        """Stop the source for a session."""
        if session_id in self._sessions:
            del self._sessions[session_id]
        return True

    def get_pipeline_source(self, session_id: str) -> str:
        """Get the GStreamer pipeline source string for a session."""
        raise NotImplementedError

    def get_active_sessions(self) -> list[str]:
        """Get active session IDs."""
        return list(self._sessions.keys())


class VNCAdapter(FrameSourceAdapter):
    """VNC source adapter for VM streaming."""

    def __init__(self) -> None:
        super().__init__("vnc")
        self._vnc_displays: dict[str, dict] = {}

    async def start(self, session_id: str, host: str, port: int) -> bool:
        self._vnc_displays[session_id] = {"host": host, "port": port}
        return True

    async def stop(self, session_id: str) -> bool:
        if session_id in self._vnc_displays:
            del self._vnc_displays[session_id]
        return True

    def get_pipeline_source(self, session_id: str) -> str:
        """Get VNC source for GStreamer pipeline."""
        display = self._vnc_displays.get(session_id)
        if not display:
            return ""

        host = display["host"]
        port = display["port"]
        return f"vncsrc location=127.0.0.1:{port} ! decodebin"


class VirtualDesktopAdapter(FrameSourceAdapter):
    """Virtual desktop adapter for container streaming."""

    def __init__(self, data_dir: Path) -> None:
        super().__init__("virtual-desktop")
        self._data_dir = data_dir
        self._wayland_displays: dict[str, str] = {}
        self._xwayland_procs: dict[str, Any] = {}

    async def start(self, session_id: str, display: str = ":99") -> bool:
        """Start a virtual desktop session."""
        import asyncio

        # Start XWayland
        self._xwayland_procs[session_id] = await asyncio.create_subprocess_exec(
            "Xwayland", display, "-rootless", "-noreset",
            stdout=asyncio.subprocess.DEVNULL,
            stderr=asyncio.subprocess.PIPE,
        )

        self._wayland_displays[session_id] = display
        return True

    async def stop(self, session_id: str) -> bool:
        """Stop a virtual desktop session."""
        if session_id in self._xwayland_procs:
            proc = self._xwayland_procs[session_id]
            proc.terminate()
            try:
                await asyncio.wait_for(proc.wait(), timeout=5.0)
            except asyncio.TimeoutError:
                proc.kill()
                await proc.wait()
            del self._xwayland_procs[session_id]

        if session_id in self._wayland_displays:
            del self._wayland_displays[session_id]

        return True

    def get_pipeline_source(self, session_id: str) -> str:
        """Get virtual desktop source for GStreamer pipeline."""
        display = self._wayland_displays.get(session_id)
        if not display:
            return ""

        return f"waylandsink ! video/x-raw, format=NV12"


class HDMIAdapter(FrameSourceAdapter):
    """HDMI capture adapter for physical machine streaming."""

    def __init__(self, capture_manager: Any) -> None:
        super().__init__("hdmi")
        self._capture_manager = capture_manager

    async def start(self, session_id: str, capture_source_id: str) -> bool:
        """Start HDMI capture for a session."""
        source = self._capture_manager.get_source(capture_source_id)
        if not source:
            return False

        self._sessions[session_id] = {"capture_source_id": capture_source_id}
        return True

    def get_pipeline_source(self, session_id: str) -> str:
        """Get HDMI source for GStreamer pipeline."""
        session = self._sessions.get(session_id)
        if not session:
            return ""

        capture_source_id = session["capture_source_id"]
        source = self._capture_manager.get_source(capture_source_id)
        if not source or not source.card:
            return ""

        return f"v4l2src device={source.card.path} ! video/x-raw, format=NV12"
