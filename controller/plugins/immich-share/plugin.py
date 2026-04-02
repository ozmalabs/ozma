# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Immich Photo Sharing plugin.

Maps Ozma share grants to scoped Immich access.  When Alice shares an
album with Bob, this plugin:

  1. Creates a partner share or shared link via the Immich API.
  2. Intercepts proxied requests and injects the scoped API key.
  3. Blocks admin and upload endpoints.
  4. Removes the share when the grant is revoked.

Grant types:
  - ``immich_album``   — share a specific album
  - ``immich_library`` — share an entire library (partner sharing)
"""

from __future__ import annotations

import logging
import re
from typing import Any

log = logging.getLogger("ozma.plugins.immich-share")

# Allowed API paths for grantees
_ALLOWED_PATH_PATTERNS = [
    re.compile(r"^/api/assets/[^/]+$"),              # single asset metadata
    re.compile(r"^/api/assets/[^/]+/thumbnail"),     # thumbnails
    re.compile(r"^/api/assets/[^/]+/original"),      # original files
    re.compile(r"^/api/assets/[^/]+/video/playback"), # video playback
    re.compile(r"^/api/albums/[^/]+$"),               # album details
    re.compile(r"^/api/albums$"),                      # list albums (filtered by partner)
    re.compile(r"^/api/shared-links"),                 # shared link access
    re.compile(r"^/api/search"),                       # search within scope
    re.compile(r"^/api/server-info/ping"),            # health check
    re.compile(r"^/api/server-info/version"),         # version info
]

# Blocked paths
_BLOCKED_PATH_PATTERNS = [
    re.compile(r"^/api/users"),                # user management
    re.compile(r"^/api/admin"),                # admin endpoints
    re.compile(r"^/api/system"),               # system config
    re.compile(r"^/api/jobs"),                 # background jobs
    re.compile(r"^/api/assets$"),              # bulk asset operations (POST = upload)
    re.compile(r"^/api/libraries"),            # library management
    re.compile(r"^/api/oauth"),                # OAuth config
    re.compile(r"^/api/api-keys"),             # API key management
    re.compile(r"^/api/partners"),             # partner management
    re.compile(r"^/api/audit"),                # audit log
    re.compile(r"^/api/trash"),                # trash management
]


class ImmichShareFilter:
    """Service filter that enforces scoped Immich access for share grants."""

    service_type = "immich"

    def __init__(self) -> None:
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

        # Block uploads (POST to /api/assets)
        if path == "/api/assets" and request.method == "POST":
            if "upload" not in grant.permissions:
                return JSONResponse(
                    status_code=403,
                    content={"error": "Upload not permitted for this share"},
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

        # Inject scoped API key
        metadata = self._grant_metadata.get(grant.id)
        if metadata and metadata.get("api_key"):
            request.state.inject_headers = {
                "x-api-key": metadata["api_key"],
            }

        return request

    async def filter_response(self, response: Any, service: Any,
                              grant: Any | None = None) -> Any:
        return response

    async def setup_backend_access(self, service: Any, grant: Any) -> dict:
        """Create a scoped Immich API key or shared link for this grant."""
        import httpx

        base_url = f"{service.protocol}://{service.target_host}:{service.target_port}"
        admin_key = getattr(service, '_admin_api_key', '') or ''
        if not admin_key:
            log.warning("Immich service %s has no admin API key — "
                        "cannot create scoped access", service.name)
            return {}

        try:
            async with httpx.AsyncClient(timeout=10.0) as client:
                if grant.resource_type == "immich_album":
                    # Create a shared link for the album
                    resp = await client.post(
                        f"{base_url}/api/shared-links",
                        json={
                            "type": "ALBUM",
                            "albumId": grant.resource_id,
                            "allowDownload": "download" in grant.permissions,
                            "allowUpload": "upload" in grant.permissions,
                            "showMetadata": True,
                        },
                        headers={"x-api-key": admin_key},
                    )
                    if resp.status_code in (200, 201):
                        data = resp.json()
                        metadata = {
                            "shared_link_id": data.get("id", ""),
                            "shared_link_key": data.get("key", ""),
                            "api_key": admin_key,  # TODO: create scoped key
                        }
                        self._grant_metadata[grant.id] = metadata
                        log.info("Created Immich shared link for album %s (grant %s)",
                                 grant.resource_id, grant.id[:8])
                        return metadata

                elif grant.resource_type == "immich_library":
                    # Use partner sharing for full library access
                    # This requires creating an Immich user for the grantee
                    log.info("Immich library sharing via partner — not yet implemented")
                    return {}

        except Exception as e:
            log.error("Failed to set up Immich access for grant %s: %s",
                      grant.id[:8], e)
        return {}

    async def teardown_backend_access(self, service: Any, grant: Any,
                                       metadata: dict) -> None:
        """Remove the Immich shared link when the grant is revoked."""
        import httpx

        shared_link_id = metadata.get("shared_link_id")
        if not shared_link_id:
            return

        base_url = f"{service.protocol}://{service.target_host}:{service.target_port}"
        admin_key = metadata.get("api_key", "")
        if not admin_key:
            return

        try:
            async with httpx.AsyncClient(timeout=10.0) as client:
                resp = await client.delete(
                    f"{base_url}/api/shared-links/{shared_link_id}",
                    headers={"x-api-key": admin_key},
                )
                if resp.status_code in (200, 204):
                    log.info("Deleted Immich shared link (grant %s revoked)", grant.id[:8])
        except Exception as e:
            log.warning("Failed to delete Immich shared link: %s", e)

        self._grant_metadata.pop(grant.id, None)


def register(ozma):
    """Plugin entry point — register the Immich share filter."""
    ozma.register_service_filter(ImmichShareFilter())
    log.info("Immich Photo Sharing plugin loaded")
