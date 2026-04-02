# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Audiobookshelf Library Sharing plugin.

Maps Ozma share grants to scoped Audiobookshelf users.  Same pattern as
the Jellyfin plugin — when Alice shares her "Audiobooks" library with Bob:

  1. Creates an Audiobookshelf user with access to only the shared library.
  2. Intercepts proxied requests and injects the scoped auth token.
  3. Blocks admin and server management endpoints.
  4. Bob gets his own listening progress, bookmarks, and playback position.
  5. Deletes the user when the grant is revoked.

Grant types:
  - ``abs_library`` — share a specific library (audiobooks or podcasts)

Audiobookshelf API:
  - REST API with Bearer token auth
  - Users have per-library access permissions
  - /api/me/listening-sessions tracks progress per-user
  - Libraries are the primary access control boundary
"""

from __future__ import annotations

import logging
import re
from typing import Any

log = logging.getLogger("ozma.plugins.audiobookshelf-share")

# Allowed paths for library consumers
_ALLOWED_PATH_PATTERNS = [
    # Library browsing
    re.compile(r"^/api/libraries/[^/]+$"),                 # library metadata
    re.compile(r"^/api/libraries/[^/]+/items"),            # list items in library
    re.compile(r"^/api/libraries/[^/]+/series"),           # list series
    re.compile(r"^/api/libraries/[^/]+/authors"),          # list authors
    re.compile(r"^/api/libraries/[^/]+/search"),           # search within library
    re.compile(r"^/api/libraries/[^/]+/personalized"),     # recommendations

    # Item access
    re.compile(r"^/api/items/[^/]+$"),                      # item metadata
    re.compile(r"^/api/items/[^/]+/cover"),                 # cover art
    re.compile(r"^/api/items/[^/]+/play"),                  # start playback session
    re.compile(r"^/api/items/[^/]+/chapters"),              # chapter list
    re.compile(r"^/api/items/[^/]+/file/"),                 # audio file streaming

    # Series and authors
    re.compile(r"^/api/series/[^/]+$"),                     # series metadata
    re.compile(r"^/api/authors/[^/]+$"),                    # author metadata
    re.compile(r"^/api/authors/[^/]+/image"),               # author image

    # User progress and sessions
    re.compile(r"^/api/me$"),                               # current user profile
    re.compile(r"^/api/me/listening-sessions"),             # progress/history
    re.compile(r"^/api/me/items-in-progress"),              # continue listening
    re.compile(r"^/api/me/progress/"),                      # per-item progress
    re.compile(r"^/api/session/"),                           # playback session management

    # Collections and playlists
    re.compile(r"^/api/collections/[^/]+$"),                # collection details
    re.compile(r"^/api/playlists/[^/]+$"),                  # playlist details

    # Podcast episodes
    re.compile(r"^/api/podcasts/[^/]+/episode/"),           # episode details

    # Static/web
    re.compile(r"^/public/"),                               # public assets
    re.compile(r"^/api/misc/"),                             # misc (covers, etc.)
]

# Blocked admin/management paths
_BLOCKED_PATH_PATTERNS = [
    re.compile(r"^/api/users"),                             # user management
    re.compile(r"^/api/libraries$"),                         # list all libraries / create
    re.compile(r"^/api/libraries/[^/]+/scan"),              # trigger library scan
    re.compile(r"^/api/libraries/[^/]+/match"),             # match library items
    re.compile(r"^/api/backups"),                            # backup management
    re.compile(r"^/api/notifications"),                      # notification management
    re.compile(r"^/api/settings"),                           # server settings
    re.compile(r"^/api/logs"),                               # server logs
    re.compile(r"^/api/cache"),                              # cache management
    re.compile(r"^/api/items/[^/]+/scan"),                  # rescan item
    re.compile(r"^/api/items/[^/]+/match"),                 # match item metadata
    re.compile(r"^/api/upload"),                             # file upload
    re.compile(r"^/api/authorize"),                          # auth management
    re.compile(r"^/api/rss"),                                # RSS feed management
    re.compile(r"^/api/share"),                              # ABS native sharing
    re.compile(r"^/api/filesystem"),                         # filesystem browsing
]


class AudiobookshelfShareFilter:
    """Service filter that enforces scoped Audiobookshelf access."""

    service_type = "audiobookshelf"

    def __init__(self) -> None:
        # grant_id → {"abs_user_id": "...", "abs_token": "...", "abs_username": "...",
        #             "library_id": "..."}
        self._grant_metadata: dict[str, dict] = {}

    async def filter_request(self, request: Any, service: Any,
                             grant: Any | None = None) -> Any:
        from fastapi.responses import JSONResponse

        path = request.url.path

        if grant is None:
            return request

        for pattern in _BLOCKED_PATH_PATTERNS:
            if pattern.match(path):
                return JSONResponse(
                    status_code=403,
                    content={"error": "Access denied by sharing policy"},
                )

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

        # Restrict to granted library
        metadata = self._grant_metadata.get(grant.id, {})
        library_id = metadata.get("library_id", "")
        if library_id and "/api/libraries/" in path:
            # Extract the library ID from the path
            match = re.match(r"^/api/libraries/([^/]+)", path)
            if match and match.group(1) != library_id:
                return JSONResponse(
                    status_code=403,
                    content={"error": "Access limited to the shared library"},
                )

        # Inject scoped auth token
        abs_token = metadata.get("abs_token", "")
        if abs_token:
            request.state.inject_headers = {
                "Authorization": f"Bearer {abs_token}",
            }

        return request

    async def filter_response(self, response: Any, service: Any,
                              grant: Any | None = None) -> Any:
        return response

    async def setup_backend_access(self, service: Any, grant: Any) -> dict:
        """Create a scoped Audiobookshelf user for this grant."""
        import httpx
        import secrets

        base_url = f"{service.protocol}://{service.target_host}:{service.target_port}"
        admin_token = getattr(service, '_admin_api_key', '') or ''
        if not admin_token:
            log.warning("Audiobookshelf service %s has no admin token — "
                        "cannot create scoped users", service.name)
            return {}

        grant_short = grant.id[:8]
        grantee_short = grant.grantee_user_id[:8]
        username = f"ozma-{grantee_short}-{grant_short}"
        password = secrets.token_urlsafe(24)
        library_id = grant.resource_id

        try:
            async with httpx.AsyncClient(timeout=10.0) as client:
                # 1. Create user with library access
                resp = await client.post(
                    f"{base_url}/api/users",
                    json={
                        "username": username,
                        "password": password,
                        "type": "user",
                        "isActive": True,
                        "permissions": {
                            "download": "download" in grant.permissions,
                            "upload": False,
                            "delete": False,
                            "update": False,
                            "accessAllLibraries": False,
                            "accessAllTags": True,
                        },
                        "librariesAccessible": [library_id],
                    },
                    headers={"Authorization": f"Bearer {admin_token}"},
                )
                if resp.status_code not in (200, 201):
                    log.error("Failed to create ABS user %s: %d %s",
                              username, resp.status_code, resp.text)
                    return {}

                user_data = resp.json()
                abs_user_id = user_data.get("id", "")

                # 2. Login as the new user to get a token
                auth_resp = await client.post(
                    f"{base_url}/login",
                    json={"username": username, "password": password},
                )
                if auth_resp.status_code != 200:
                    log.error("Failed to authenticate ABS user %s: %d",
                              username, auth_resp.status_code)
                    return {"abs_user_id": abs_user_id, "abs_username": username}

                auth_data = auth_resp.json()
                abs_token = auth_data.get("user", {}).get("token", "")

                metadata = {
                    "abs_user_id": abs_user_id,
                    "abs_token": abs_token,
                    "abs_username": username,
                    "library_id": library_id,
                }
                self._grant_metadata[grant.id] = metadata

                log.info("Created ABS user %s for grant %s (library: %s)",
                         username, grant.id[:8], library_id)
                return metadata

        except Exception as e:
            log.error("Failed to set up ABS access for grant %s: %s",
                      grant.id[:8], e)
            return {}

    async def teardown_backend_access(self, service: Any, grant: Any,
                                       metadata: dict) -> None:
        """Delete the Audiobookshelf user when the grant is revoked."""
        import httpx

        abs_user_id = metadata.get("abs_user_id")
        if not abs_user_id:
            return

        base_url = f"{service.protocol}://{service.target_host}:{service.target_port}"
        admin_token = getattr(service, '_admin_api_key', '') or ''
        if not admin_token:
            return

        try:
            async with httpx.AsyncClient(timeout=10.0) as client:
                resp = await client.delete(
                    f"{base_url}/api/users/{abs_user_id}",
                    headers={"Authorization": f"Bearer {admin_token}"},
                )
                if resp.status_code in (200, 204):
                    log.info("Deleted ABS user %s (grant %s revoked)",
                             metadata.get("abs_username", abs_user_id),
                             grant.id[:8])
                else:
                    log.warning("Failed to delete ABS user %s: %d",
                                abs_user_id, resp.status_code)
        except Exception as e:
            log.warning("Failed to delete ABS user %s: %s", abs_user_id, e)

        self._grant_metadata.pop(grant.id, None)


def register(ozma):
    """Plugin entry point."""
    ozma.register_service_filter(AudiobookshelfShareFilter())
    log.info("Audiobookshelf Library Sharing plugin loaded")
