# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Frigate NVR Sharing plugin.

Maps Ozma share grants to scoped Frigate camera access.  When Alice shares
her "Front Door" camera with Bob, this plugin:

  1. Intercepts proxied requests and restricts API access to only the
     granted cameras.
  2. Allows live feeds, recordings, events, and snapshots for those cameras.
  3. Blocks NVR configuration, object detection settings, and other cameras.

Grant types:
  - ``frigate_camera``  — share one or more cameras (live + recordings + events)
  - ``frigate_feed``    — share live feed only (no recordings or event history)

Frigate API overview:
  - No user/auth model — Frigate trusts its network boundary (proxy provides auth)
  - REST API for config, events, recordings, snapshots, thumbnails
  - MQTT for real-time events (person detected, motion, etc.)
  - Live feeds via MSE/WebRTC at /live/{camera}/mse or /api/{camera}/latest.jpg
  - Recordings at /api/{camera}/recordings
  - Events with thumbnails and clips
  - go2rtc handles the WebRTC/MSE streaming layer
"""

from __future__ import annotations

import logging
import re
from typing import Any

log = logging.getLogger("ozma.plugins.frigate-share")

# Allowed paths — parameterised by camera name (checked dynamically)
_ALLOWED_PATH_PATTERNS = [
    # Live feeds
    re.compile(r"^/api/[^/]+/latest\.jpg"),            # latest snapshot
    re.compile(r"^/api/[^/]+/thumbnail\.jpg"),          # thumbnail
    re.compile(r"^/api/[^/]+/recordings/"),             # recording segments
    re.compile(r"^/live/[^/]+/"),                        # MSE/WebRTC live stream
    re.compile(r"^/api/go2rtc/"),                        # go2rtc WebRTC signalling

    # Events (filtered by camera in response)
    re.compile(r"^/api/events$"),                        # event list (filtered)
    re.compile(r"^/api/events/[^/]+$"),                  # single event details
    re.compile(r"^/api/events/[^/]+/thumbnail\.jpg"),   # event thumbnail
    re.compile(r"^/api/events/[^/]+/clip\.mp4"),        # event clip
    re.compile(r"^/api/events/[^/]+/snapshot\.jpg"),    # event snapshot
    re.compile(r"^/api/events/summary$"),                # event summary (filtered)
    re.compile(r"^/api/timeline$"),                      # timeline (filtered)

    # Recording playback
    re.compile(r"^/api/[^/]+/start/"),                   # recording start time
    re.compile(r"^/api/[^/]+/end/"),                     # recording end time
    re.compile(r"^/vod/"),                                # video-on-demand HLS

    # Preview/review
    re.compile(r"^/api/preview/"),                        # preview thumbnails
    re.compile(r"^/api/review$"),                         # review items (filtered)
    re.compile(r"^/api/review/[^/]+$"),                  # single review item

    # Static assets (UI)
    re.compile(r"^/assets/"),                             # frontend assets
    re.compile(r"^/static/"),                             # static files
    re.compile(r"^/$"),                                   # frontend root
    re.compile(r"^/index\.html$"),                       # frontend entry
    re.compile(r"^/manifest\.json$"),                    # PWA manifest

    # Health
    re.compile(r"^/api/version$"),                        # version info
    re.compile(r"^/api/stats$"),                          # system stats (filtered)
]

# Blocked paths — NVR admin and configuration
_BLOCKED_PATH_PATTERNS = [
    re.compile(r"^/api/config$"),                        # full NVR config (contains all cameras, credentials, etc.)
    re.compile(r"^/api/config/"),                         # config subsections
    re.compile(r"^/api/[^/]+/detect$"),                  # toggle detection on/off
    re.compile(r"^/api/[^/]+/recordings$"),              # toggle recordings on/off (POST)
    re.compile(r"^/api/[^/]+/motion$"),                  # toggle motion detection
    re.compile(r"^/api/[^/]+/improve_contrast$"),        # toggle contrast
    re.compile(r"^/api/[^/]+/motion_threshold$"),        # set motion threshold
    re.compile(r"^/api/[^/]+/motion_contour_area$"),    # set contour area
    re.compile(r"^/api/restart$"),                        # restart Frigate
    re.compile(r"^/api/ffprobe$"),                        # probe media files
    re.compile(r"^/api/events/[^/]+/retain$"),           # toggle event retention
    re.compile(r"^/api/events/[^/]+(delete|sub_label)"), # modify events
    re.compile(r"^/api/export/"),                         # export recordings
    re.compile(r"^/api/logs"),                            # server logs
    re.compile(r"^/api/notifications/"),                  # notification management
]


class FrigateShareFilter:
    """Service filter that enforces scoped Frigate camera access."""

    service_type = "frigate"

    def __init__(self) -> None:
        # grant_id → {"cameras": ["front_door", "driveway"], "mode": "camera"|"feed"}
        self._grant_metadata: dict[str, dict] = {}

    def _extract_camera_from_path(self, path: str) -> str | None:
        """Extract camera name from Frigate API paths like /api/{camera}/latest.jpg."""
        # Paths that contain a camera name as the second segment
        parts = path.strip("/").split("/")
        if len(parts) >= 2 and parts[0] in ("api", "live"):
            candidate = parts[1]
            # Skip known non-camera segments
            if candidate not in ("events", "config", "go2rtc", "version", "stats",
                                  "restart", "ffprobe", "export", "logs",
                                  "preview", "review", "notifications"):
                return candidate
        if len(parts) >= 2 and parts[0] == "vod":
            return parts[1]
        return None

    async def filter_request(self, request: Any, service: Any,
                             grant: Any | None = None) -> Any:
        from fastapi.responses import JSONResponse

        path = request.url.path

        if grant is None:
            return request

        metadata = self._grant_metadata.get(grant.id, {})
        allowed_cameras = set(metadata.get("cameras", []))
        mode = metadata.get("mode", "camera")

        # Block admin/config paths
        for pattern in _BLOCKED_PATH_PATTERNS:
            if pattern.match(path):
                return JSONResponse(
                    status_code=403,
                    content={"error": "Access denied by sharing policy"},
                )

        # Block recording/event access for feed-only grants
        if mode == "feed":
            if any(seg in path for seg in ("/recordings/", "/events", "/clip.",
                                            "/review", "/timeline", "/vod/")):
                return JSONResponse(
                    status_code=403,
                    content={"error": "This share only includes live feeds"},
                )

        # Check if path references a specific camera
        camera = self._extract_camera_from_path(path)
        if camera and allowed_cameras and camera not in allowed_cameras:
            return JSONResponse(
                status_code=403,
                content={"error": f"Camera '{camera}' is not included in this share"},
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

        # For event/review list endpoints, inject camera filter as query param
        if path in ("/api/events", "/api/events/summary", "/api/review",
                     "/api/timeline") and allowed_cameras:
            # Frigate accepts ?cameras=cam1,cam2 to filter
            request.state.inject_query_params = {
                "cameras": ",".join(allowed_cameras),
            }

        return request

    async def filter_response(self, response: Any, service: Any,
                              grant: Any | None = None) -> Any:
        """Filter stats to only show granted cameras."""
        # TODO: for /api/stats, parse JSON and strip camera entries not in grant
        return response

    async def setup_backend_access(self, service: Any, grant: Any) -> dict:
        """Configure Frigate camera access for this grant.

        Frigate has no user/auth model — it trusts its network boundary.
        The Ozma proxy provides the authentication layer. We just need to
        record which cameras the grantee can see.

        resource_id format: comma-separated camera names
          "front_door"                    — single camera
          "front_door,driveway,backyard"  — multiple cameras
          "*"                             — all cameras
        """
        cameras = [c.strip() for c in grant.resource_id.split(",") if c.strip()]

        # Determine mode from grant type
        mode = "feed" if grant.resource_type == "frigate_feed" else "camera"

        metadata = {
            "cameras": cameras if cameras != ["*"] else [],  # empty = all
            "mode": mode,
        }
        self._grant_metadata[grant.id] = metadata

        cam_desc = ", ".join(cameras) if cameras != ["*"] else "all cameras"
        log.info("Configured Frigate access for grant %s (%s, mode=%s)",
                 grant.id[:8], cam_desc, mode)
        return metadata

    async def teardown_backend_access(self, service: Any, grant: Any,
                                       metadata: dict) -> None:
        """Remove Frigate access metadata."""
        self._grant_metadata.pop(grant.id, None)
        log.info("Removed Frigate access for grant %s", grant.id[:8])


def register(ozma):
    """Plugin entry point."""
    ozma.register_service_filter(FrigateShareFilter())
    log.info("Frigate NVR Sharing plugin loaded")
