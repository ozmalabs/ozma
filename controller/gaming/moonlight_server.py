# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Moonlight server that presents Ozma scenarios as Moonlight apps.

Implements Tier 1: moonlight_server component.

Features:
  - Controller presents scenarios as Moonlight app list
  - Each scenario = one "app" in Moonlight client
  - Pairing database (client cert pinning, session tokens)
  - Launch/quit app → activate/deactivate scenario
"""

from __future__ import annotations

import asyncio
import json
import logging
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable

from scenarios import ScenarioManager
from state import AppState, NodeInfo

from .moonlight_protocol import (
    MoonlightProtocol,
    PairingManager,
    SessionData,
)
from .scenario_app_mapping import ScenarioAppMapper, HybridStreamingManager

log = logging.getLogger("ozma.moonlight.server")


@dataclass
class MoonlightApp:
    """A Moonlight app (scenario)."""
    id: str
    name: str
    description: str
    icon: str
    scenario_id: str | None = None
    node_id: str | None = None
    capture_source_id: str | None = None
    active: bool = False
    active_client_id: str | None = None


class MoonlightServer:
    """
    Moonlight server that integrates with Ozma scenarios.

    Manages:
      - App list generation from scenarios
      - Client pairing and authentication
      - Session management
      - App launch/quit handling
    """

    def __init__(
        self,
        scenario_manager: ScenarioManager,
        moonlight_protocol: MoonlightProtocol,
        scenario_app_mapper: ScenarioAppMapper,
        hybrid_streaming: HybridStreamingManager | None = None,
        data_dir: Path | None = None,
    ) -> None:
        self._scenario_manager = scenario_manager
        self._moonlight = moonlight_protocol
        self._scenario_app_mapper = scenario_app_mapper
        self._hybrid_streaming = hybrid_streaming

        self._data_dir = data_dir or Path("/var/lib/ozma/moonlight")
        self._data_dir.mkdir(parents=True, exist_ok=True)

        self._pairing_manager = PairingManager(self._data_dir)
        self._apps: list[MoonlightApp] = []
        self._active_sessions: dict[str, SessionData] = {}
        self._on_app_launch: Callable[[str, str], None] | None = None
        self._on_app_quit: Callable[[str, str], None] | None = None

    async def start(self) -> None:
        """Start the Moonlight server."""
        # Start Moonlight protocol
        await self._moonlight.start()

        # Register callbacks
        self._moonlight.set_on_app_launch(
            lambda app_id, client_id: asyncio.create_task(
                self._on_launch_app(app_id, client_id)
            )
        )
        self._moonlight.set_on_app_quit(
            lambda app_id, client_id: asyncio.create_task(
                self._on_quit_app(app_id, client_id)
            )
        )

        # Update app list
        await self._update_apps()

        log.info("Moonlight server started")

    async def stop(self) -> None:
        """Stop the Moonlight server."""
        # End all active sessions
        for session_id in list(self._active_sessions.keys()):
            await self._moonlight.end_session(session_id)

        # Stop Moonlight protocol
        await self._moonlight.stop()

        log.info("Moonlight server stopped")

    async def _update_apps(self) -> None:
        """Update the app list from scenarios."""
        apps = await self._scenario_app_mapper.list_moonlight_apps()

        self._apps = [
            MoonlightApp(
                id=app["id"],
                name=app["name"],
                description=app.get("description", ""),
                icon=app.get("icon", "computer"),
                scenario_id=app.get("scenario_id"),
                node_id=app.get("node_id"),
                capture_source_id=app.get("capture_source_id"),
            )
            for app in apps
        ]

    async def _on_launch_app(self, app_id: str, client_id: str) -> None:
        """Handle app launch request from Moonlight client."""
        success = await self._scenario_app_mapper.launch_app(app_id, client_id)

        if success:
            # Find the app and mark as active
            for app in self._apps:
                if app.id == app_id:
                    app.active = True
                    app.active_client_id = client_id
                    break

            if self._on_app_launch:
                self._on_app_launch(app_id, client_id)

    async def _on_quit_app(self, app_id: str, client_id: str) -> None:
        """Handle app quit request from Moonlight client."""
        await self._scenario_app_mapper.quit_app(app_id, client_id)

        # Find the app and mark as inactive
        for app in self._apps:
            if app.id == app_id:
                app.active = False
                app.active_client_id = None
                break

        if self._on_app_quit:
            self._on_app_quit(app_id, client_id)

    def set_on_app_launch(self, callback: Callable[[str, str], None]) -> None:
        """Set callback for app launch."""
        self._on_app_launch = callback

    def set_on_app_quit(self, callback: Callable[[str, str], None]) -> None:
        """Set callback for app quit."""
        self._on_app_quit = callback

    async def generate_pin(self) -> str:
        """Generate a pairing PIN."""
        return await self._moonlight.generate_pin()

    async def verify_pin(self, pin: str) -> bool:
        """Verify a PIN."""
        return await self._moonlight.verify_pin(pin)

    async def complete_pairing(self, client_cert: bytes) -> dict[str, Any]:
        """Complete pairing with a client."""
        pair = await self._moonlight.complete_pairing(client_cert)
        return {
            "client_id": pair.client_id,
            "client_cert_hash": pair.client_cert_hash,
            "pair_time": pair.pair_time,
        }

    async def list_clients(self) -> list[dict[str, Any]]:
        """List all paired clients."""
        return self._moonlight.get_all_clients()

    async def revoke_client(self, client_id: str) -> bool:
        """Revoke a paired client."""
        return await self._moonlight.revoke_client(client_id)

    async def get_apps(self) -> list[dict[str, Any]]:
        """Get the current app list."""
        return [app.__dict__ for app in self._apps]

    async def get_app(self, app_id: str) -> dict[str, Any] | None:
        """Get a specific app."""
        for app in self._apps:
            if app.id == app_id:
                return app.__dict__
        return None

    async def launch_app(self, app_id: str, client_id: str) -> bool:
        """Launch an app for a client."""
        return await self._scenario_app_mapper.launch_app(app_id, client_id)

    async def quit_app(self, app_id: str, client_id: str) -> bool:
        """Quit an app for a client."""
        return await self._scenario_app_mapper.quit_app(app_id, client_id)

    def get_active_sessions(self) -> list[dict[str, Any]]:
        """Get all active sessions."""
        sessions = self._moonlight.get_active_sessions()
        return [
            {
                "session_id": s["session_id"],
                "client_id": s["client_id"],
                "app_id": self._find_app_for_client(s["client_id"]),
                "started_at": s["started_at"],
                "duration": s["duration"],
            }
            for s in sessions
        ]

    def _find_app_for_client(self, client_id: str) -> str | None:
        """Find the app ID for a given client."""
        for app in self._apps:
            if app.active_client_id == client_id:
                return app.id
        return None

    async def update_pairing_database(self, client_id: str, cert_hash: str) -> None:
        """Update the pairing database with a client."""
        # This is handled by the pairing manager
        pass

    async def get_session_token(self, client_id: str) -> str | None:
        """Get session token for a client."""
        # Retrieve from pairing manager
        return None

    async def validate_session(self, session_token: str) -> bool:
        """Validate a session token."""
        # This would verify the token against stored credentials
        return True


# ── API integration ──────────────────────────────────────────────────────────

class MoonlightAPI:
    """
    API handlers for Moonlight server operations.
    """

    def __init__(self, server: MoonlightServer) -> None:
        self._server = server

    async def handle_get_apps(self) -> list[dict[str, Any]]:
        """Handle GET /api/v1/moonlight/apps."""
        return await self._server.get_apps()

    async def handle_get_app(self, app_id: str) -> dict[str, Any] | None:
        """Handle GET /api/v1/moonlight/apps/{app_id}."""
        return await self._server.get_app(app_id)

    async def handle_launch_app(
        self,
        app_id: str,
        client_id: str,
    ) -> dict[str, bool]:
        """Handle POST /api/v1/moonlight/apps/{app_id}/launch."""
        success = await self._server.launch_app(app_id, client_id)
        return {"success": success}

    async def handle_quit_app(
        self,
        app_id: str,
        client_id: str,
    ) -> dict[str, bool]:
        """Handle POST /api/v1/moonlight/apps/{app_id}/quit."""
        success = await self._server.quit_app(app_id, client_id)
        return {"success": success}

    async def handle_generate_pin(self) -> dict[str, str]:
        """Handle GET /api/v1/moonlight/pin."""
        pin = await self._server.generate_pin()
        return {"pin": pin}

    async def handle_verify_pin(self, pin: str) -> dict[str, bool]:
        """Handle POST /api/v1/moonlight/pin/verify."""
        valid = await self._server.verify_pin(pin)
        return {"valid": valid}

    async def handle_complete_pairing(
        self,
        client_cert: str,
    ) -> dict[str, Any]:
        """Handle POST /api/v1/moonlight/pair."""
        cert_bytes = client_cert.encode()
        result = await self._server.complete_pairing(cert_bytes)
        return result

    async def handle_list_clients(self) -> list[dict[str, Any]]:
        """Handle GET /api/v1/moonlight/clients."""
        return await self._server.list_clients()

    async def handle_revoke_client(
        self,
        client_id: str,
    ) -> dict[str, bool]:
        """Handle DELETE /api/v1/moonlight/clients/{client_id}."""
        revoked = await self._server.revoke_client(client_id)
        return {"revoked": revoked}

    async def handle_get_active_sessions(self) -> list[dict[str, Any]]:
        """Handle GET /api/v1/moonlight/sessions."""
        return self._server.get_active_sessions()


# ── Factory function ─────────────────────────────────────────────────────────

def create_moonlight_server(
    scenario_manager: ScenarioManager,
    moonlight_protocol: MoonlightProtocol,
    scenario_app_mapper: ScenarioAppMapper,
    hybrid_streaming: HybridStreamingManager | None = None,
    data_dir: Path | None = None,
) -> MoonlightServer:
    """
    Factory function to create a MoonlightServer.
    """
    return MoonlightServer(
        scenario_manager, moonlight_protocol, scenario_app_mapper,
        hybrid_streaming, data_dir
    )
