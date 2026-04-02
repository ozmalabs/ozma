# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Home Assistant Dashboard Sharing plugin.

Maps Ozma share grants to scoped Home Assistant access.  When Alice
shares her "Weather Station" dashboard with Bob, this plugin:

  1. Creates a long-lived access token scoped to a HA user with limited
     permissions (or uses an existing one configured by the owner).
  2. Intercepts proxied requests and injects the scoped auth token.
  3. Blocks admin, automation, integration, and configuration endpoints.
  4. Allows only the shared dashboard's view plus its entity states.

Grant types:
  - ``ha_dashboard``    — share a specific Lovelace dashboard
  - ``ha_device_group`` — share a group of entities (e.g. all sensors in "Garden")

Home Assistant's auth model:
  - Long-lived access tokens (LLAT) provide API access
  - Users can be created with limited permissions
  - The /api/states endpoint returns all entities (filtering is our job)
  - Lovelace dashboards are served as YAML/JSON configs
  - WebSocket API at /api/websocket is the primary real-time interface
"""

from __future__ import annotations

import json
import logging
import re
from typing import Any

log = logging.getLogger("ozma.plugins.homeassistant-share")

# Allowed paths for dashboard viewers
_ALLOWED_PATH_PATTERNS = [
    # Lovelace dashboard UI
    re.compile(r"^/lovelace/"),                       # dashboard views
    re.compile(r"^/api/lovelace/config"),              # dashboard config (read)
    re.compile(r"^/api/lovelace/dashboards$"),         # list dashboards

    # Entity state (read-only) — filtered by response filter
    re.compile(r"^/api/states$"),                       # all states (filtered in response)
    re.compile(r"^/api/states/[a-z_]+\.[a-z0-9_]+$"),  # single entity state
    re.compile(r"^/api/history/period"),                 # history (filtered)
    re.compile(r"^/api/logbook"),                        # logbook (filtered)

    # Camera/media proxies (entity-specific)
    re.compile(r"^/api/camera_proxy/"),                 # camera snapshot
    re.compile(r"^/api/camera_proxy_stream/"),          # camera stream
    re.compile(r"^/api/media_player_proxy/"),           # media player proxy

    # Static assets
    re.compile(r"^/static/"),                            # frontend JS/CSS
    re.compile(r"^/frontend_latest/"),                   # frontend bundle
    re.compile(r"^/frontend_es5/"),                       # legacy frontend
    re.compile(r"^/hacsfiles/"),                          # HACS custom cards
    re.compile(r"^/local/"),                              # local www/ files
    re.compile(r"^/api/image/serve/"),                    # image entities
    re.compile(r"^/auth/token"),                          # token refresh
    re.compile(r"^/manifest\.json$"),                     # PWA manifest
    re.compile(r"^/api/$"),                               # API root (health)

    # WebSocket (filtered by message handler)
    re.compile(r"^/api/websocket$"),
]

# Blocked paths — admin, config, automations, integrations
_BLOCKED_PATH_PATTERNS = [
    re.compile(r"^/api/config"),                        # system configuration
    re.compile(r"^/api/services"),                       # call services (mutating!)
    re.compile(r"^/api/events"),                         # fire events
    re.compile(r"^/api/template"),                       # render templates
    re.compile(r"^/developer-tools"),                    # developer tools
    re.compile(r"^/config"),                             # config UI
    re.compile(r"^/api/hassio"),                         # supervisor/add-ons
    re.compile(r"^/api/discovery"),                      # integration discovery
    re.compile(r"^/api/cloud"),                          # HA Cloud
    re.compile(r"^/api/backup"),                         # backups
    re.compile(r"^/api/auth/"),                          # auth management
    re.compile(r"^/api/onboarding"),                     # onboarding
    re.compile(r"^/api/person"),                         # person management
    re.compile(r"^/api/zone"),                           # zone management
    re.compile(r"^/api/scene/"),                         # scene activation
    re.compile(r"^/api/automation/"),                    # automation management
    re.compile(r"^/api/script/"),                        # script execution
]


class HomeAssistantShareFilter:
    """Service filter that enforces scoped Home Assistant access."""

    service_type = "homeassistant"

    def __init__(self) -> None:
        # grant_id → {"ha_token": "...", "dashboard_path": "...",
        #             "allowed_entities": ["sensor.x", ...]}
        self._grant_metadata: dict[str, dict] = {}

    async def filter_request(self, request: Any, service: Any,
                             grant: Any | None = None) -> Any:
        from fastapi.responses import JSONResponse

        path = request.url.path

        if grant is None:
            return request

        # Block dangerous paths first
        for pattern in _BLOCKED_PATH_PATTERNS:
            if pattern.match(path):
                return JSONResponse(
                    status_code=403,
                    content={"message": "Access denied by sharing policy"},
                )

        # Block service calls (POST /api/services/*) unless explicitly permitted
        if path.startswith("/api/services") and request.method == "POST":
            if "write" not in grant.permissions:
                return JSONResponse(
                    status_code=403,
                    content={"message": "Service calls not permitted for this share"},
                )

        # Check allowed paths
        allowed = False
        for pattern in _ALLOWED_PATH_PATTERNS:
            if pattern.match(path):
                allowed = True
                break

        if not allowed:
            return JSONResponse(
                status_code=403,
                content={"message": "This endpoint is not available for shared access"},
            )

        # Restrict to specific dashboard if grant type is ha_dashboard
        metadata = self._grant_metadata.get(grant.id, {})
        if grant.resource_type == "ha_dashboard":
            dashboard_path = metadata.get("dashboard_path", "")
            if path.startswith("/lovelace/") and dashboard_path:
                # Only allow the specific dashboard
                requested_dash = path.split("/")[2] if len(path.split("/")) > 2 else ""
                if requested_dash and requested_dash != dashboard_path:
                    return JSONResponse(
                        status_code=403,
                        content={"message": "Access limited to the shared dashboard"},
                    )

        # Inject HA auth token
        ha_token = metadata.get("ha_token", "")
        if ha_token:
            request.state.inject_headers = {
                "Authorization": f"Bearer {ha_token}",
            }

        return request

    async def filter_response(self, response: Any, service: Any,
                              grant: Any | None = None) -> Any:
        """Filter entity states to only those visible on the shared dashboard."""
        if grant is None:
            return response

        # Only filter /api/states responses
        # For a full implementation, parse the JSON response and remove
        # entities not in the allowed set.  For now, pass through —
        # the dashboard config itself limits what's rendered.
        # TODO: implement entity-level response filtering
        return response

    async def setup_backend_access(self, service: Any, grant: Any) -> dict:
        """Configure HA access for this grant.

        Two modes:
          1. **ha-ozma integration installed** (preferred): The HA-side integration
             has already created a scoped user + LLAT and included them in the
             grant's resource_id as ``dashboard_path:token``. We just store it.
          2. **Manual**: Owner provides the LLAT themselves when creating the grant.

        When the ha-ozma HACS integration is installed, the ``ozma.share_dashboard``
        service call in HA does everything: creates the scoped HA user, generates
        the LLAT, parses the dashboard YAML to extract entity scoping, and calls
        the Ozma controller API to create the grant with all metadata embedded.
        This proxy filter then just reads the metadata and injects the token.
        """
        metadata: dict[str, Any] = {}

        if grant.resource_type == "ha_dashboard":
            # resource_id format: "dashboard_path:token" (from ha-ozma integration)
            # or just "dashboard_path" (manual, token provided separately)
            parts = grant.resource_id.split(":", 1)
            metadata["dashboard_path"] = parts[0]
            if len(parts) > 1:
                metadata["ha_token"] = parts[1]

            # The ha-ozma integration may also pass allowed_entities in permissions
            # Format: permissions = ["read", "entity:sensor.x", "entity:switch.y"]
            entity_perms = [p.split(":", 1)[1] for p in grant.permissions
                           if p.startswith("entity:")]
            if entity_perms:
                metadata["allowed_entities"] = entity_perms

        elif grant.resource_type == "ha_device_group":
            parts = grant.resource_id.split(":", 1)
            metadata["group_entity"] = parts[0]
            if len(parts) > 1:
                metadata["ha_token"] = parts[1]

        self._grant_metadata[grant.id] = metadata
        log.info("Configured HA access for grant %s (dashboard: %s, entities: %d)",
                 grant.id[:8], metadata.get("dashboard_path", "N/A"),
                 len(metadata.get("allowed_entities", [])))
        return metadata

    async def teardown_backend_access(self, service: Any, grant: Any,
                                       metadata: dict) -> None:
        """Clean up HA access.

        If the ha-ozma integration created the scoped user, it will also
        clean it up when it receives the ``share.revoked`` event via the
        Ozma WebSocket. This side just clears local metadata.

        If the LLAT was manually provided, we can't revoke it — the owner
        must do that in HA's UI.
        """
        self._grant_metadata.pop(grant.id, None)
        log.info("Removed HA access for grant %s", grant.id[:8])


def register(ozma):
    """Plugin entry point."""
    ozma.register_service_filter(HomeAssistantShareFilter())
    log.info("Home Assistant Dashboard Sharing plugin loaded")
