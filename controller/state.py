# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
import asyncio
import time
from dataclasses import dataclass, field
from typing import Any


# Machine class — what kind of machine is this node plugged into?
# Determines security behaviour for remote access and agent control.
#   workstation — someone may be sitting here. Consent can be required,
#                 agent mutating actions default to "notify".
#   server      — headless / unattended. No consent, agent actions auto,
#                 privacy mode is a no-op (no physical display to blank).
#   kiosk       — has a display but no operator. No consent, no privacy.
MACHINE_CLASSES = ("workstation", "server", "kiosk", "camera")


@dataclass
class NodeInfo:
    id: str           # mDNS instance name, used as stable identifier
    host: str         # resolved IP address
    port: int         # UDP port (always 7331 per spec)
    role: str         # "compute", "presence", "room-mic", "display", etc.
    hw: str           # hardware type, e.g. "milkv-duos", "rpi-zero2w", "teensy41"
    fw_version: str   # firmware version string
    proto_version: int  # protocol version from TXT record
    capabilities: list[str] = field(default_factory=list)
    machine_class: str = "workstation"  # workstation | server | kiosk
    last_seen: float = field(default_factory=time.monotonic)
    # Multi-display outputs (one entry per display head)
    # Each: {"index": 0, "source_type": "dbus"|"vnc"|"ivshmem"|"agent",
    #        "capture_source_id": "vm1-display-0", "width": 1920, "height": 1080}
    display_outputs: list[dict] = field(default_factory=list)
    # Optional display/stream metadata (published via mDNS TXT)
    # Single-display fields kept for backward compatibility
    vnc_host: str | None = None
    vnc_port: int | None = None
    # Hardware node: serves its own HLS stream
    stream_port: int | None = None    # HTTP port on the node
    stream_path: str | None = None    # path, e.g. /stream/stream.m3u8
    # HTTP API port (health + /usb + HLS if video); same as stream_port when both present
    api_port: int | None = None
    # Audio routing (V0.3)
    audio_type: str | None = None        # "pipewire" | "vban" | None
    audio_sink: str | None = None        # PW null-sink name (pipewire nodes)
    audio_vban_port: int | None = None   # UDP port node emits VBAN on (vban nodes)
    mic_vban_port: int | None = None     # UDP port node listens for mic VBAN (vban nodes)
    # Virtual capture device (soft nodes with v4l2loopback)
    capture_device: str | None = None    # /dev/videoN path on the controller host
    # Camera node fields (machine_class="camera")
    # camera_streams: list of RTSP/HLS streams this camera node provides.
    # Each entry: {"name": "front_door", "rtsp_inbound": "rtsp://...",
    #              "backchannel": "rtsp://...", "hls": "http://..."}
    camera_streams: list[dict] = field(default_factory=list)
    # Frigate instance running on (or paired with) this camera node.
    frigate_host: str | None = None   # hostname/IP of the Frigate API
    frigate_port: int | None = None   # Frigate API port (default 5000)
    # Ownership — which user owns this node (empty = controller default owner)
    owner_user_id: str = ""
    # Registration source — direct HTTP nodes are not evicted by mDNS requery
    direct_registered: bool = False
    # Seat configuration — pushed to agents via config WebSocket
    seat_count: int = 1
    seat_config: dict = field(default_factory=dict)  # {seats: N, profiles: [...], ...}
    # Seat ownership and sharing
    owner_id: str = ""                                 # user ID who owns this node/seat
    shared_with: list[str] = field(default_factory=list)  # user IDs who have access
    share_permissions: dict[str, str] = field(default_factory=dict)  # user_id -> "use"|"manage"|"admin"
    parent_node_id: str = ""                           # if this is a seat, the machine it belongs to
    # Game streaming (V1.2) — Sunshine/Moonlight
    # Set by agent mDNS TXT record ("sunshine_port") or by SunshineManager on enable.
    sunshine_port: int | None = None   # Sunshine stream base port on this node's host

    @property
    def stream_url(self) -> str | None:
        """Full URL to the node's HLS manifest, if it serves one directly."""
        if self.stream_port and self.stream_path:
            return f"http://{self.host}:{self.stream_port}{self.stream_path}"
        return None

    def to_dict(self) -> dict[str, Any]:
        d: dict[str, Any] = {
            "id": self.id,
            "host": self.host,
            "port": self.port,
            "role": self.role,
            "hw": self.hw,
            "fw_version": self.fw_version,
            "proto_version": self.proto_version,
            "capabilities": self.capabilities,
            "machine_class": self.machine_class,
            "display_outputs": self.display_outputs,
            "last_seen": self.last_seen,
        }
        if self.owner_user_id:
            d["owner_user_id"] = self.owner_user_id
        if self.vnc_host:
            d["vnc_host"] = self.vnc_host
        if self.vnc_port:
            d["vnc_port"] = self.vnc_port
        if self.stream_url:
            d["stream_url"] = self.stream_url
        if self.api_port:
            d["api_port"] = self.api_port
        if self.audio_type:
            d["audio_type"] = self.audio_type
        if self.audio_sink:
            d["audio_sink"] = self.audio_sink
        if self.audio_vban_port:
            d["audio_vban_port"] = self.audio_vban_port
        if self.mic_vban_port:
            d["mic_vban_port"] = self.mic_vban_port
        if self.capture_device:
            d["capture_device"] = self.capture_device
        if self.camera_streams:
            d["camera_streams"] = self.camera_streams
        if self.frigate_host:
            d["frigate_host"] = self.frigate_host
        if self.frigate_port:
            d["frigate_port"] = self.frigate_port
        if self.seat_count != 1 or self.seat_config:
            d["seat_count"] = self.seat_count
            d["seat_config"] = self.seat_config
        if self.owner_id:
            d["owner_id"] = self.owner_id
        if self.shared_with:
            d["shared_with"] = self.shared_with
            d["share_permissions"] = self.share_permissions
        if self.parent_node_id:
            d["parent_node_id"] = self.parent_node_id
        if self.sunshine_port:
            d["sunshine_port"] = self.sunshine_port
        return d


class AppState:
    def __init__(self) -> None:
        self.nodes: dict[str, NodeInfo] = {}
        self.active_node_id: str | None = None
        self._lock = asyncio.Lock()

        # Broadcast queue — api.py drains this for WebSocket clients
        self.events: asyncio.Queue[dict[str, Any]] = asyncio.Queue()

        # User manager — set by main.py after UserManager is created
        self.user_manager: Any | None = None

        # Vaultwarden manager — set by main.py if OZMA_VAULTWARDEN=1
        self.vaultwarden_manager: Any | None = None

        # Routing graph (Phase 1: observational)
        from routing import RoutingGraph, GraphBuilder
        self.routing_graph: RoutingGraph = RoutingGraph()
        self._graph_builder: GraphBuilder = GraphBuilder(self.routing_graph)

    async def add_node(self, node: NodeInfo) -> None:
        async with self._lock:
            is_new = node.id not in self.nodes
            if not is_new:
                # Merge: keep richer data from either the existing or incoming node.
                # On busy hosts (many Docker/Podman bridges), mDNS resolves
                # the same service on multiple interfaces. An early resolution
                # may arrive before the node has finished starting (missing
                # optional fields like capture_device). A later resolution
                # arrives with the full data. We keep whichever value is set.
                existing = self.nodes[node.id]
                node.capture_device = node.capture_device or existing.capture_device
                node.vnc_host = node.vnc_host or existing.vnc_host
                node.vnc_port = node.vnc_port or existing.vnc_port
                node.audio_type = node.audio_type or existing.audio_type
                node.audio_sink = node.audio_sink or existing.audio_sink
                node.audio_vban_port = node.audio_vban_port or existing.audio_vban_port
                node.mic_vban_port = node.mic_vban_port or existing.mic_vban_port
                node.stream_port = node.stream_port or existing.stream_port
                node.stream_path = node.stream_path or existing.stream_path
                node.api_port = node.api_port or existing.api_port
                # Preserve machine_class if the incoming registration doesn't set one
                if node.machine_class == "workstation" and existing.machine_class != "workstation":
                    node.machine_class = existing.machine_class
                node.owner_user_id = node.owner_user_id or existing.owner_user_id
                node.display_outputs = node.display_outputs or existing.display_outputs
                node.camera_streams = node.camera_streams or existing.camera_streams
                node.frigate_host = node.frigate_host or existing.frigate_host
                node.frigate_port = node.frigate_port or existing.frigate_port
                # Preserve seat config from existing node (controller-owned)
                node.seat_count = existing.seat_count
                node.seat_config = existing.seat_config or node.seat_config
                # Preserve ownership/sharing (controller-owned, survives re-registration)
                node.owner_id = existing.owner_id or node.owner_id
                node.shared_with = existing.shared_with or node.shared_with
                node.share_permissions = existing.share_permissions or node.share_permissions
                node.parent_node_id = existing.parent_node_id or node.parent_node_id
                if existing.capabilities and not node.capabilities:
                    node.capabilities = existing.capabilities
                elif node.capabilities and existing.capabilities:
                    # Union of capabilities
                    node.capabilities = list(set(node.capabilities) | set(existing.capabilities))
            self.nodes[node.id] = node
        self._graph_builder.apply_node_added(node, self)
        if is_new:
            await self.events.put({"type": "node.online", "node": node.to_dict()})

    async def remove_node(self, node_id: str) -> None:
        async with self._lock:
            removed = self.nodes.pop(node_id, None)
            if self.active_node_id == node_id:
                self.active_node_id = None
        if removed:
            self._graph_builder.apply_node_removed(node_id)
            await self.events.put({"type": "node.offline", "node_id": node_id})

    async def set_active_node(self, node_id: str) -> None:
        async with self._lock:
            if node_id not in self.nodes:
                raise KeyError(f"Unknown node: {node_id}")
            self.active_node_id = node_id
        # Rebuild graph so link statuses reflect the new active node
        self._graph_builder.rebuild(self)
        await self.events.put({"type": "node.switched", "node_id": node_id})

    def get_active_node(self) -> NodeInfo | None:
        nid = self.active_node_id
        if nid is None:
            return None
        return self.nodes.get(nid)

    def snapshot(self) -> dict[str, Any]:
        return {
            "nodes": {k: v.to_dict() for k, v in self.nodes.items()},
            "active_node_id": self.active_node_id,
        }
