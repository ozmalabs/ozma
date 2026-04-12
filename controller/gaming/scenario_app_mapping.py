# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Unified Moonlight app list across all machine types.

Maps every scenario — regardless of whether it targets a physical machine
(HDMI capture), a VM (VNC/SPICE), or a container (virtual desktop) — to a
single flat Moonlight "app" entry.  The Moonlight client sees one coherent
app list; selecting an app triggers a scenario switch and tells the streaming
pipeline which source adapter to use.

Source adapter types
--------------------
``hdmi_capture``
    Physical machine connected via a V4L2 capture card.
    Frame source: ``display_capture.DisplayCaptureManager``.

``vnc``
    Virtual machine exposed over VNC or SPICE.
    Frame source: VNC client → GStreamer appsrc (Tier 3 hybrid_streaming task).

``virtual_desktop``
    Container or headless Wayland session.
    Frame source: wlroots virtual compositor (Tier 1 headless_wayland task).

``sunshine``
    Legacy: Sunshine subprocess running on a remote node.
    Frame source: re-encoded RTSP from Sunshine.

``unknown``
    Scenario type could not be determined; app is listed but streaming
    will fail gracefully with an error to the client.

Integration points
------------------
- ``ScenarioAppMapper.build_app_list()`` — called by the Moonlight server
  (``moonlight_server`` task) to populate the GFE ``/applist`` response.
- ``ScenarioAppMapper.resolve_source()`` — called at stream-launch time to
  get the concrete ``AppSource`` descriptor the pipeline should connect to.
- ``ScenarioAppMapper.on_scenario_change()`` — hook for the scenario manager
  to notify the mapper when scenarios are added/removed/modified.
"""

from __future__ import annotations

import logging
from dataclasses import dataclass
from enum import Enum
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from controller.display_capture import DisplayCaptureManager

log = logging.getLogger("ozma.gaming.scenario_app_mapping")


# ── Source adapter type ───────────────────────────────────────────────────────

class SourceType(str, Enum):
    """The kind of frame source backing a Moonlight app."""

    HDMI_CAPTURE = "hdmi_capture"
    VNC = "vnc"
    VIRTUAL_DESKTOP = "virtual_desktop"
    SUNSHINE = "sunshine"
    UNKNOWN = "unknown"


# ── Data model ────────────────────────────────────────────────────────────────

@dataclass
class AppSource:
    """
    Concrete streaming source descriptor resolved for a scenario.

    The streaming pipeline (GStreamer / capture-to-Moonlight) uses this to
    know *where* to pull frames from.
    """

    source_type: SourceType

    # HDMI_CAPTURE fields
    capture_source_id: str = ""       # e.g. "hdmi-0"
    v4l2_device: str = ""             # e.g. "/dev/video0"

    # VNC fields
    vnc_host: str = ""
    vnc_port: int = 5900
    vnc_password: str = ""            # empty = no auth

    # VIRTUAL_DESKTOP fields
    wayland_display: str = ""         # e.g. "wayland-1"
    compositor_pid: int = 0

    # SUNSHINE fields
    sunshine_host: str = ""
    sunshine_port: int = 47989
    sunshine_app_id: str = ""

    # Generic metadata
    width: int = 1920
    height: int = 1080
    fps: int = 60

    def to_dict(self) -> dict[str, Any]:
        return {
            "source_type": self.source_type.value,
            "capture_source_id": self.capture_source_id,
            "v4l2_device": self.v4l2_device,
            "vnc_host": self.vnc_host,
            "vnc_port": self.vnc_port,
            "vnc_password": "***" if self.vnc_password else "",
            "wayland_display": self.wayland_display,
            "compositor_pid": self.compositor_pid,
            "sunshine_host": self.sunshine_host,
            "sunshine_port": self.sunshine_port,
            "sunshine_app_id": self.sunshine_app_id,
            "width": self.width,
            "height": self.height,
            "fps": self.fps,
        }


@dataclass
class MoonlightApp:
    """
    A single entry in the Moonlight GFE app list.

    The ``id`` is stable across restarts (derived from the scenario id) so
    that Moonlight clients can remember their last-played app.
    """

    id: int                           # Numeric GFE app id (Moonlight protocol)
    name: str                         # Display name shown in Moonlight client
    scenario_id: str                  # Ozma scenario this app maps to
    source_type: SourceType
    hdr_supported: bool = False
    has_custom_poster: bool = False   # True if a poster image exists on disk
    poster_url: str = ""              # URL to 460×215 poster image (optional)

    def to_gfe_dict(self) -> dict[str, Any]:
        """Serialise to the GFE /applist JSON format Moonlight expects."""
        return {
            "ID": self.id,
            "AppTitle": self.name,
            "IsHdrSupported": int(self.hdr_supported),
            "HasCustomBoxArt": int(self.has_custom_poster),
        }

    def to_dict(self) -> dict[str, Any]:
        """Full internal representation."""
        return {
            "id": self.id,
            "name": self.name,
            "scenario_id": self.scenario_id,
            "source_type": self.source_type.value,
            "hdr_supported": self.hdr_supported,
            "has_custom_poster": self.has_custom_poster,
            "poster_url": self.poster_url,
        }


# ── Scenario introspection helpers ────────────────────────────────────────────

def _stable_app_id(scenario_id: str) -> int:
    """
    Derive a stable numeric app id from a scenario id string.

    Moonlight uses 32-bit integers.  We hash the scenario id and clamp to
    the positive 31-bit range so it fits in a signed 32-bit int (some
    clients use signed comparison).  Collisions are astronomically unlikely
    for the number of scenarios a real deployment will have.
    """
    import hashlib
    digest = hashlib.sha256(scenario_id.encode()).digest()
    # Take first 4 bytes as big-endian unsigned int, clamp to [1, 2^31-1]
    raw = int.from_bytes(digest[:4], "big")
    return max(1, raw & 0x7FFF_FFFF)


def _infer_source_type(scenario: dict[str, Any]) -> SourceType:
    """
    Infer the streaming source type from a scenario dict.

    Checks (in priority order):
    1. Explicit ``streaming_source`` key set by the scenario author.
    2. Presence of ``capture_source`` → HDMI capture.
    3. Presence of ``vnc_host`` / ``vm_id`` → VNC.
    4. Presence of ``container_id`` / ``wayland_display`` → virtual desktop.
    5. Presence of ``sunshine_host`` → Sunshine.
    6. Fall back to UNKNOWN.
    """
    explicit = scenario.get("streaming_source", "")
    if explicit:
        try:
            return SourceType(explicit)
        except ValueError:
            log.warning("Unknown streaming_source value %r in scenario %r",
                        explicit, scenario.get("id", "?"))

    if scenario.get("capture_source"):
        return SourceType.HDMI_CAPTURE

    if scenario.get("vnc_host") or scenario.get("vm_id"):
        return SourceType.VNC

    if scenario.get("container_id") or scenario.get("wayland_display"):
        return SourceType.VIRTUAL_DESKTOP

    if scenario.get("sunshine_host"):
        return SourceType.SUNSHINE

    return SourceType.UNKNOWN


def _build_source(scenario: dict[str, Any],
                  source_type: SourceType,
                  capture_manager: "DisplayCaptureManager | None") -> AppSource:
    """Build an ``AppSource`` from a scenario dict."""

    src = AppSource(source_type=source_type)

    if source_type == SourceType.HDMI_CAPTURE:
        cap_id = scenario.get("capture_source", "")
        src.capture_source_id = cap_id
        if capture_manager:
            display_src = capture_manager.get_source(cap_id)
            if display_src:
                card = display_src.card
                res = card.current_resolution
                src.v4l2_device = card.path
                src.width = res.width if res else card.max_width
                src.height = res.height if res else card.max_height
                src.fps = res.fps if res else card.max_fps
            else:
                log.warning("Scenario %r references capture_source %r which is not registered",
                            scenario.get("id", "?"), cap_id)
        else:
            src.v4l2_device = scenario.get("v4l2_device", "")

    elif source_type == SourceType.VNC:
        src.vnc_host = scenario.get("vnc_host", "")
        src.vnc_port = int(scenario.get("vnc_port", 5900))
        src.vnc_password = scenario.get("vnc_password", "")
        src.width = int(scenario.get("width", 1920))
        src.height = int(scenario.get("height", 1080))
        src.fps = int(scenario.get("fps", 30))

    elif source_type == SourceType.VIRTUAL_DESKTOP:
        src.wayland_display = scenario.get("wayland_display", "")
        src.compositor_pid = int(scenario.get("compositor_pid", 0))
        src.width = int(scenario.get("width", 1920))
        src.height = int(scenario.get("height", 1080))
        src.fps = int(scenario.get("fps", 60))

    elif source_type == SourceType.SUNSHINE:
        src.sunshine_host = scenario.get("sunshine_host", "")
        src.sunshine_port = int(scenario.get("sunshine_port", 47989))
        src.sunshine_app_id = str(scenario.get("sunshine_app_id", ""))
        src.width = int(scenario.get("width", 1920))
        src.height = int(scenario.get("height", 1080))
        src.fps = int(scenario.get("fps", 60))

    return src


# ── Main mapper ───────────────────────────────────────────────────────────────

class ScenarioAppMapper:
    """
    Builds and maintains the unified Moonlight app list.

    Instantiate once and pass to the Moonlight server.  Call
    ``refresh(scenarios)`` whenever the scenario list changes.

    Parameters
    ----------
    capture_manager:
        Optional ``DisplayCaptureManager`` instance.  When provided, HDMI
        capture sources are enriched with live resolution/device info.
    poster_base_url:
        Base URL under which per-scenario poster images are served, e.g.
        ``"/static/posters"``.  The mapper appends ``/{scenario_id}.jpg``.
    """

    def __init__(self,
                 capture_manager: "DisplayCaptureManager | None" = None,
                 poster_base_url: str = "/static/posters") -> None:
        self._capture_manager = capture_manager
        self._poster_base_url = poster_base_url.rstrip("/")
        self._apps: dict[str, MoonlightApp] = {}   # keyed by scenario_id
        self._id_collision_map: dict[int, str] = {} # numeric_id → scenario_id

    # ── Public API ────────────────────────────────────────────────────────────

    def refresh(self, scenarios: list[dict[str, Any]]) -> None:
        """
        Rebuild the app list from a fresh scenario list.

        Safe to call at any time; the update is atomic (swap on completion).
        Scenarios that are not streamable (``streamable: false``) are skipped.
        """
        new_apps: dict[str, MoonlightApp] = {}
        new_id_map: dict[int, str] = {}

        for scenario in scenarios:
            sid = scenario.get("id", "")
            if not sid:
                continue

            # Allow scenarios to opt out of the Moonlight app list
            if not scenario.get("streamable", True):
                continue

            source_type = _infer_source_type(scenario)

            numeric_id = _stable_app_id(sid)
            # Resolve collisions by incrementing (extremely rare)
            while numeric_id in new_id_map and new_id_map[numeric_id] != sid:
                numeric_id = (numeric_id % 0x7FFF_FFFF) + 1

            name = scenario.get("name") or scenario.get("title") or sid
            poster_url = f"{self._poster_base_url}/{sid}.jpg"

            app = MoonlightApp(
                id=numeric_id,
                name=name,
                scenario_id=sid,
                source_type=source_type,
                hdr_supported=bool(scenario.get("hdr", False)),
                has_custom_poster=bool(scenario.get("has_poster", False)),
                poster_url=poster_url,
            )
            new_apps[sid] = app
            new_id_map[numeric_id] = sid

            log.debug("App mapped: [%d] %r → %s", numeric_id, name, source_type.value)

        self._apps = new_apps
        self._id_collision_map = new_id_map
        log.info("App list refreshed: %d apps (%s)",
                 len(self._apps),
                 ", ".join(f"{a.source_type.value}×{sum(1 for x in self._apps.values() if x.source_type == a.source_type)}"
                           for a in {a.source_type: a for a in self._apps.values()}.values()))

    def build_app_list(self) -> list[dict[str, Any]]:
        """
        Return the GFE-format app list for the Moonlight ``/applist`` endpoint.

        Sorted alphabetically by name so the Moonlight client displays them
        in a predictable order.
        """
        return [app.to_gfe_dict()
                for app in sorted(self._apps.values(), key=lambda a: a.name.lower())]

    def resolve_source(self, app_id: int) -> AppSource | None:
        """
        Resolve a numeric Moonlight app id to a concrete ``AppSource``.

        Returns ``None`` if the app id is not in the current list.
        Called by the Moonlight server at stream-launch time.
        """
        sid = self._id_collision_map.get(app_id)
        if not sid:
            log.warning("resolve_source: unknown app id %d", app_id)
            return None
        app = self._apps.get(sid)
        if not app:
            return None
        # We don't cache the full scenario dict, so we re-derive from the app.
        # For a full resolve we need the scenario dict — callers should use
        # resolve_source_for_scenario() when they have it.
        return AppSource(source_type=app.source_type)

    def resolve_source_for_scenario(self, scenario: dict[str, Any]) -> AppSource | None:
        """
        Resolve a full ``AppSource`` from a scenario dict.

        Preferred over ``resolve_source()`` when the caller already has the
        scenario dict (e.g. the scenario manager's launch hook).
        """
        sid = scenario.get("id", "")
        if not sid or sid not in self._apps:
            return None
        source_type = _infer_source_type(scenario)
        return _build_source(scenario, source_type, self._capture_manager)

    def get_app_by_scenario(self, scenario_id: str) -> MoonlightApp | None:
        """Look up a ``MoonlightApp`` by scenario id."""
        return self._apps.get(scenario_id)

    def get_app_by_id(self, app_id: int) -> MoonlightApp | None:
        """Look up a ``MoonlightApp`` by numeric Moonlight app id."""
        sid = self._id_collision_map.get(app_id)
        return self._apps.get(sid) if sid else None

    def get_scenario_id(self, app_id: int) -> str | None:
        """Return the scenario id for a numeric Moonlight app id."""
        return self._id_collision_map.get(app_id)

    def list_apps(self) -> list[dict[str, Any]]:
        """Full internal app list (richer than GFE format)."""
        return [app.to_dict()
                for app in sorted(self._apps.values(), key=lambda a: a.name.lower())]

    def on_scenario_change(self, scenarios: list[dict[str, Any]]) -> None:
        """
        Hook for the scenario manager to call when scenarios change.

        Alias for ``refresh()`` — exists so callers can use a more
        descriptive name when wiring up the callback.
        """
        self.refresh(scenarios)

    # ── Source-type summary ───────────────────────────────────────────────────

    def source_type_counts(self) -> dict[str, int]:
        """Return a count of apps per source type (for metrics/logging)."""
        counts: dict[str, int] = {t.value: 0 for t in SourceType}
        for app in self._apps.values():
            counts[app.source_type.value] += 1
        return counts
