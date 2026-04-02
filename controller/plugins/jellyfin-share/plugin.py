# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Jellyfin Media Sharing plugin.

Maps Ozma share grants to scoped Jellyfin guest users.  When Alice shares
her "Movies" library with Bob, this plugin:

  1. Creates a Jellyfin user ``ozma-bob-<grant_id[:8]>`` with access to
     only the "Movies" library.
  2. Stores the Jellyfin user ID and auth token in grant metadata.
  3. Intercepts proxied requests and injects the scoped auth token.
  4. Blocks admin/management API endpoints.
  5. Deletes the Jellyfin user when the grant is revoked.

Grant types:
  - ``jellyfin_library`` — share one or more libraries
  - ``jellyfin_item``    — share a specific item or playlist

The plugin runs on the controller, not inside Jellyfin.  No Jellyfin
plugin installation required on the media server.
"""

from __future__ import annotations

import json
import logging
import re
from typing import Any

log = logging.getLogger("ozma.plugins.jellyfin-share")

# API paths that grantees are allowed to access (read-only media consumption)
_ALLOWED_PATH_PATTERNS = [
    re.compile(r"^/Users/[^/]+/Items"),              # browse items
    re.compile(r"^/Users/[^/]+/Views"),               # library views
    re.compile(r"^/Items/[^/]+$"),                     # single item metadata
    re.compile(r"^/Items/[^/]+/Images"),               # artwork
    re.compile(r"^/Items/[^/]+/Similar"),              # recommendations
    re.compile(r"^/Videos/[^/]+/"),                     # video streaming
    re.compile(r"^/Audio/[^/]+/"),                      # audio streaming
    re.compile(r"^/Shows/[^/]+/Episodes"),             # TV episodes
    re.compile(r"^/Shows/[^/]+/Seasons"),              # TV seasons
    re.compile(r"^/Artists"),                            # music artists
    re.compile(r"^/Albums"),                             # music albums
    re.compile(r"^/Genres"),                             # genre listing
    re.compile(r"^/Studios"),                            # studio listing
    re.compile(r"^/Persons"),                            # people listing
    re.compile(r"^/Search/Hints"),                       # search
    re.compile(r"^/DisplayPreferences"),                 # UI prefs
    re.compile(r"^/UserItems/[^/]+/UserData"),          # watch state (read)
    re.compile(r"^/Sessions/Playing"),                   # playback reporting
    re.compile(r"^/Sessions/Playing/Progress"),          # playback progress
    re.compile(r"^/Sessions/Playing/Stopped"),           # playback stopped
    re.compile(r"^/web/"),                               # web UI assets
    re.compile(r"^/System/Info/Public$"),                # public server info
    re.compile(r"^/Branding/"),                          # branding assets
]

# Paths explicitly blocked (admin, management, other users)
_BLOCKED_PATH_PATTERNS = [
    re.compile(r"^/System/(?!Info/Public)"),    # system admin (except public info)
    re.compile(r"^/Library/"),                   # library management
    re.compile(r"^/Plugins"),                    # plugin management
    re.compile(r"^/Packages"),                   # package management
    re.compile(r"^/Notifications/Admin"),        # admin notifications
    re.compile(r"^/Users$"),                     # user listing
    re.compile(r"^/Users/[^/]+/Password"),       # password management
    re.compile(r"^/Users/[^/]+/Policy"),         # user policy
    re.compile(r"^/Users/New"),                  # user creation
    re.compile(r"^/Users/[^/]+/Delete"),         # user deletion
    re.compile(r"^/Startup"),                    # setup wizard
    re.compile(r"^/Environment"),                # server environment
    re.compile(r"^/Log/"),                       # server logs
    re.compile(r"^/ScheduledTasks"),             # scheduled tasks
    re.compile(r"^/LiveTv/"),                    # live TV management
]


class JellyfinShareFilter:
    """Service filter that enforces scoped Jellyfin access for share grants."""

    service_type = "jellyfin"

    def __init__(self) -> None:
        # grant_id → {"jf_user_id": "...", "jf_token": "...", "jf_username": "..."}
        self._grant_metadata: dict[str, dict] = {}

    async def filter_request(self, request: Any, service: Any,
                             grant: Any | None = None) -> Any:
        """Intercept requests to Jellyfin and enforce grant scope."""
        from fastapi.responses import JSONResponse

        path = request.url.path

        # No grant = full access (owner accessing their own service)
        if grant is None:
            return request

        # Check blocked paths first
        for pattern in _BLOCKED_PATH_PATTERNS:
            if pattern.match(path):
                return JSONResponse(
                    status_code=403,
                    content={"error": "Access denied by sharing policy"},
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
                content={"error": "This endpoint is not available for shared access"},
            )

        # Inject the scoped Jellyfin auth token
        metadata = self._grant_metadata.get(grant.id)
        if metadata and metadata.get("jf_token"):
            # Jellyfin accepts auth via X-Emby-Token header or query param
            request.state.inject_headers = {
                "X-Emby-Token": metadata["jf_token"],
            }
            # Rewrite user ID in path if the path references a user
            jf_user_id = metadata.get("jf_user_id", "")
            if jf_user_id and "/Users/" in path:
                # Replace any user ID in the path with the grant's scoped user
                request.state.rewrite_path = re.sub(
                    r"/Users/[^/]+/", f"/Users/{jf_user_id}/", path
                )

        return request

    async def filter_response(self, response: Any, service: Any,
                              grant: Any | None = None) -> Any:
        """Pass through — Jellyfin's per-user scoping handles content filtering."""
        return response

    async def setup_backend_access(self, service: Any, grant: Any) -> dict:
        """Create a scoped Jellyfin user for this grant.

        Uses the Jellyfin API to:
          1. Create a user with a random password
          2. Set library access to only the granted libraries
          3. Generate an auth token
          4. Return the metadata for future requests
        """
        import httpx
        import secrets

        base_url = f"{service.protocol}://{service.target_host}:{service.target_port}"

        # We need an admin API key to create users.
        # The service owner must configure this in the service metadata.
        admin_key = getattr(service, '_admin_api_key', '') or ''
        if not admin_key:
            log.warning("Jellyfin service %s has no admin API key configured — "
                        "cannot create scoped users for sharing", service.name)
            return {}

        grant_short = grant.id[:8]
        grantee_short = grant.grantee_user_id[:8]
        username = f"ozma-{grantee_short}-{grant_short}"
        password = secrets.token_urlsafe(24)

        try:
            async with httpx.AsyncClient(timeout=10.0) as client:
                # 1. Create user
                resp = await client.post(
                    f"{base_url}/Users/New",
                    json={"Name": username, "Password": password},
                    headers={"X-Emby-Token": admin_key},
                )
                if resp.status_code not in (200, 201):
                    log.error("Failed to create Jellyfin user %s: %d %s",
                              username, resp.status_code, resp.text)
                    return {}

                user_data = resp.json()
                jf_user_id = user_data.get("Id", "")

                # 2. Set user policy (disable admin, limit library access)
                policy = {
                    "IsAdministrator": False,
                    "IsDisabled": False,
                    "EnableAllFolders": False,
                    "EnabledFolders": [],  # will be set per grant type
                    "EnableRemoteAccess": True,
                    "EnableLiveTvManagement": False,
                    "EnableLiveTvAccess": False,
                    "EnableContentDeletion": False,
                    "EnableContentDownloading": "download" in grant.permissions,
                }

                # Set library access based on grant type
                if grant.resource_type == "jellyfin_library":
                    # resource_id is a comma-separated list of library IDs
                    library_ids = [lid.strip() for lid in grant.resource_id.split(",") if lid.strip()]
                    policy["EnabledFolders"] = library_ids
                elif grant.resource_type == "jellyfin_item":
                    # For single items, we enable the parent library
                    # The actual item filtering happens at the API level
                    policy["EnableAllFolders"] = True  # TODO: restrict to parent library

                await client.post(
                    f"{base_url}/Users/{jf_user_id}/Policy",
                    json=policy,
                    headers={"X-Emby-Token": admin_key},
                )

                # 3. Authenticate as the new user to get a token
                auth_resp = await client.post(
                    f"{base_url}/Users/AuthenticateByName",
                    json={"Username": username, "Pw": password},
                    headers={
                        "X-Emby-Authorization": (
                            'MediaBrowser Client="Ozma", Device="Controller", '
                            'DeviceId="ozma-share", Version="1.0.0"'
                        ),
                    },
                )
                if auth_resp.status_code != 200:
                    log.error("Failed to authenticate Jellyfin user %s: %d",
                              username, auth_resp.status_code)
                    return {"jf_user_id": jf_user_id, "jf_username": username}

                auth_data = auth_resp.json()
                jf_token = auth_data.get("AccessToken", "")

                metadata = {
                    "jf_user_id": jf_user_id,
                    "jf_token": jf_token,
                    "jf_username": username,
                }
                self._grant_metadata[grant.id] = metadata

                log.info("Created Jellyfin user %s for grant %s (libraries: %s)",
                         username, grant.id[:8], grant.resource_id)
                return metadata

        except Exception as e:
            log.error("Failed to set up Jellyfin access for grant %s: %s",
                      grant.id[:8], e)
            return {}

    async def teardown_backend_access(self, service: Any, grant: Any,
                                       metadata: dict) -> None:
        """Delete the Jellyfin user when the grant is revoked."""
        import httpx

        jf_user_id = metadata.get("jf_user_id")
        if not jf_user_id:
            return

        base_url = f"{service.protocol}://{service.target_host}:{service.target_port}"
        admin_key = getattr(service, '_admin_api_key', '') or ''
        if not admin_key:
            return

        try:
            async with httpx.AsyncClient(timeout=10.0) as client:
                resp = await client.delete(
                    f"{base_url}/Users/{jf_user_id}",
                    headers={"X-Emby-Token": admin_key},
                )
                if resp.status_code in (200, 204):
                    log.info("Deleted Jellyfin user %s (grant %s revoked)",
                             metadata.get("jf_username", jf_user_id), grant.id[:8])
                else:
                    log.warning("Failed to delete Jellyfin user %s: %d",
                                jf_user_id, resp.status_code)
        except Exception as e:
            log.warning("Failed to delete Jellyfin user %s: %s", jf_user_id, e)

        self._grant_metadata.pop(grant.id, None)


def register(ozma):
    """Plugin entry point — register the Jellyfin share filter."""
    ozma.register_service_filter(JellyfinShareFilter())
    log.info("Jellyfin Media Sharing plugin loaded")
