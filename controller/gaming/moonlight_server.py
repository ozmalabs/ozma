# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Moonlight Server - presents Ozma scenarios as Moonlight apps.

Architecture:
  - Scenarios are presented as "apps" in Moonlight client
  - Each scenario = one "app" in Moonlight app list
  - Launch/quit app → activate/deactivate scenario
  - Client cert pinning for security

See: moonlight_protocol.py for the protocol implementation
"""

from __future__ import annotations

import asyncio
import json
import logging
import secrets
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.controller.gaming.moonlight_server")

# ─── Constants ───────────────────────────────────────────────────────────────

MOONLIGHT_SERVER_PORT = 47990
DEFAULT_APP_ID = 999999  # First app ID for custom apps


# ─── Data Models ─────────────────────────────────────────────────────────────

@dataclass
class MoonlightApp:
    """A scenario presented as a Moonlight app."""
    app_id: int
    scenario_id: str
    name: str
    description: str = ""
    icon: str = ""  # Base64-encoded icon
    is_game: bool = True
    launch_command: str = ""
    working_dir: str = ""
    parameters: str = ""
    created_at: float = field(default_factory=time.time)
    last_played: float = 0
    play_count: int = 0
    config: dict[str, Any] = field(default_factory=dict)

    def to_dict(self) -> dict[str, Any]:
        return {
            "appId": self.app_id,
            "name": self.name,
            "description": self.description,
            "icon": self.icon,
            "isGame": self.is_game,
            "launchCommand": self.launch_command,
            "workingDir": self.working_dir,
            "parameters": self.parameters,
            "scenarioId": self.scenario_id,
            "createdAt": self.created_at,
            "lastPlayed": self.last_played,
            "playCount": self.play_count,
            "config": self.config,
        }


@dataclass
class ClientCert:
    """A pinned client certificate."""
    client_id: str
    cert_hash: str  # SHA256 hex
    cert_pem: str = ""
    created_at: float = field(default_factory=time.time)
    last_used: float = field(default_factory=time.time)
    allowed_features: list[str] = field(default_factory=lambda: [
        "keyboard", "mouse", "gamepad", "touch", "haptics", "headset",
    ])

    def to_dict(self) -> dict[str, Any]:
        return {
            "clientId": self.client_id,
            "certHash": self.cert_hash,
            "createdAt": self.created_at,
            "lastUsed": self.last_used,
            "allowedFeatures": self.allowed_features,
        }


# ─── Moonlight Server ────────────────────────────────────────────────────────

class MoonlightServer:
    """
    Moonlight server that presents Ozma scenarios as apps.

    Features:
      - Scenario → App mapping
      - Client certificate pinning
      - Session management
      - App launch/quit handling
    """

    def __init__(
        self,
        state: Any = None,
        scenarios: Any = None,
        protocol_server: Any = None,
        data_dir: Path = Path("/var/lib/ozma/gaming"),
    ):
        self._state = state
        self._scenarios = scenarios
        self._protocol = protocol_server
        self._data_dir = data_dir
        self._data_dir.mkdir(parents=True, exist_ok=True)

        # Apps (scenarios as Moonlight apps)
        self._apps: dict[int, MoonlightApp] = {}
        self._app_by_scenario: dict[str, MoonlightApp] = {}
        self._next_app_id = DEFAULT_APP_ID

        # Client certificates (cert pinning)
        self._certs: dict[str, ClientCert] = {}
        self._cert_by_hash: dict[str, ClientCert] = {}

        # Session state
        self._active_sessions: dict[str, MoonlightApp] = {}

        # Load persisted state
        self._load_state()

    def _load_state(self) -> None:
        """Load server state from disk."""
        apps_file = self._data_dir / "apps.json"
        if apps_file.exists():
            try:
                data = json.loads(apps_file.read_text())
                for app_data in data.get("apps", []):
                    app = MoonlightApp(**app_data)
                    self._apps[app.app_id] = app
                    self._app_by_scenario[app.scenario_id] = app
                if data.get("next_app_id"):
                    self._next_app_id = data["next_app_id"]
                log.info("Loaded %d Moonlight apps", len(self._apps))
            except Exception as e:
                log.error("Failed to load apps: %s", e)

        certs_file = self._data_dir / "certs.json"
        if certs_file.exists():
            try:
                data = json.loads(certs_file.read_text())
                for cert_data in data.get("certs", []):
                    cert = ClientCert(**cert_data)
                    self._certs[cert.client_id] = cert
                    self._cert_by_hash[cert.cert_hash] = cert
                log.info("Loaded %d pinned client certificates", len(self._certs))
            except Exception as e:
                log.error("Failed to load certs: %s", e)

    def _save_apps(self) -> None:
        """Save apps to disk."""
        apps_file = self._data_dir / "apps.json"
        try:
            data = {
                "apps": [app.to_dict() for app in self._apps.values()],
                "next_app_id": self._next_app_id,
                "last_save": time.time(),
            }
            apps_file.write_text(json.dumps(data, indent=2))
        except Exception as e:
            log.error("Failed to save apps: %s", e)

    def _save_certs(self) -> None:
        """Save certificates to disk."""
        certs_file = self._data_dir / "certs.json"
        try:
            data = {
                "certs": [cert.to_dict() for cert in self._certs.values()],
                "last_save": time.time(),
            }
            certs_file.write_text(json.dumps(data, indent=2))
        except Exception as e:
            log.error("Failed to save certs: %s", e)

    # ── App management ────────────────────────────────────────────────────────

    def add_scenario_app(self, scenario_id: str) -> MoonlightApp:
        """Create a Moonlight app for a scenario."""
        scenario = self._scenarios.get(scenario_id) if self._scenarios else None
        if scenario is None:
            raise ValueError(f"Scenario not found: {scenario_id}")

        # Generate app ID
        app_id = self._next_app_id
        self._next_app_id += 1

        # Create app
        app = MoonlightApp(
            app_id=app_id,
            scenario_id=scenario_id,
            name=scenario.name if scenario else scenario_id,
            description=f"Launch {scenario.name} via Moonlight" if scenario else "",
            is_game=scenario.config.get("is_game", True) if scenario else True,
        )

        self._apps[app_id] = app
        self._app_by_scenario[scenario_id] = app
        self._save_apps()

        log.info("Added Moonlight app %d for scenario %s", app_id, scenario_id)
        return app

    def remove_scenario_app(self, scenario_id: str) -> bool:
        """Remove a Moonlight app for a scenario."""
        app = self._app_by_scenario.get(scenario_id)
        if not app:
            return False

        del self._apps[app.app_id]
        del self._app_by_scenario[scenario_id]
        self._save_apps()

        log.info("Removed Moonlight app for scenario %s", scenario_id)
        return True

    def get_app(self, app_id: int) -> MoonlightApp | None:
        """Get an app by ID."""
        return self._apps.get(app_id)

    def get_app_by_scenario(self, scenario_id: str) -> MoonlightApp | None:
        """Get an app by scenario ID."""
        return self._app_by_scenario.get(scenario_id)

    def list_apps(self) -> list[MoonlightApp]:
        """List all apps."""
        return list(self._apps.values())

    def get_apps_for_client(self, client_id: str) -> list[MoonlightApp]:
        """Get apps available to a specific client (with filtering)."""
        client_cert = self._certs.get(client_id)
        if not client_cert:
            return self.list_apps()

        # Filter based on allowed features
        apps = []
        for app in self._apps.values():
            # In production, filter based on client permissions
            apps.append(app)
        return apps

    # ── Client certificate management ─────────────────────────────────────────

    def add_client_cert(self, client_id: str, cert_hash: str) -> ClientCert:
        """Add a pinned client certificate."""
        cert = ClientCert(
            client_id=client_id,
            cert_hash=cert_hash,
        )
        self._certs[client_id] = cert
        self._cert_by_hash[cert_hash] = cert
        self._save_certs()

        log.info("Added cert pin for client %s", client_id)
        return cert

    def remove_client_cert(self, client_id: str) -> bool:
        """Remove a pinned client certificate."""
        if client_id in self._certs:
            cert = self._certs.pop(client_id)
            if cert.cert_hash in self._cert_by_hash:
                del self._cert_by_hash[cert.cert_hash]
            self._save_certs()
            log.info("Removed cert pin for client %s", client_id)
            return True
        return False

    def get_client_cert(self, client_id: str) -> ClientCert | None:
        """Get a client certificate by ID."""
        return self._certs.get(client_id)

    def get_client_cert_by_hash(self, cert_hash: str) -> ClientCert | None:
        """Get a client certificate by hash."""
        return self._cert_by_hash.get(cert_hash)

    # ── Session management ────────────────────────────────────────────────────

    def start_session(self, client_id: str, app_id: int) -> bool:
        """Start a streaming session."""
        app = self._apps.get(app_id)
        if not app:
            return False

        # Update last played
        app.last_played = time.time()
        app.play_count += 1
        self._save_apps()

        # Update cert usage
        if client_id in self._certs:
            self._certs[client_id].last_used = time.time()
            self._save_certs()

        # Activate scenario
        scenario_id = app.scenario_id
        if self._scenarios:
            asyncio.create_task(
                self._scenarios.activate(scenario_id),
                name=f"moonlight-activate-{scenario_id}"
            )

        self._active_sessions[client_id] = app
        log.info("Session started: client=%s, app=%s (%s)", client_id, app_id, scenario_id)
        return True

    def end_session(self, client_id: str) -> bool:
        """End a streaming session."""
        if client_id in self._active_sessions:
            app = self._active_sessions.pop(client_id)
            log.info("Session ended: client=%s, app=%s", client_id, app.app_id)
            return True
        return False

    def get_active_session(self, client_id: str) -> MoonlightApp | None:
        """Get the active session for a client."""
        return self._active_sessions.get(client_id)

    def get_all_sessions(self) -> list[tuple[str, MoonlightApp]]:
        """Get all active sessions."""
        return list(self._active_sessions.items())

    # ── API responses ─────────────────────────────────────────────────────────

    def get_server_info(self) -> dict[str, Any]:
        """Get server information for Moonlight clients."""
        return {
            "serverName": "Ozma Moonlight Server",
            "serverId": "ozma-" + secrets.token_hex(8),
            "version": "1.2.0",
            "port": MOONLIGHT_SERVER_PORT,
            "pairing": "required",
            "encryption": "required",
            "maxClients": 10,
            "maxApps": 100,
            "apps": [app.to_dict() for app in self._apps.values()],
            "features": [
                "keyboard", "mouse", "gamepad", "touch", "haptics",
                "headset", "hdr", "multitouch",
            ],
        }

    def get_app_list(self, client_id: str | None = None) -> list[dict[str, Any]]:
        """Get the app list for a client."""
        if client_id:
            apps = self.get_apps_for_client(client_id)
        else:
            apps = self.list_apps()
        return [app.to_dict() for app in apps]

    def get_client_info(self, client_id: str) -> dict[str, Any] | None:
        """Get information about a client."""
        cert = self._certs.get(client_id)
        if not cert:
            return None

        return cert.to_dict()
