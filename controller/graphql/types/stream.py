# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
GraphQL types for video streams and camera info.
"""

import logging
from typing import TYPE_CHECKING

from strawberry import type as graphql_type
from strawberry.types import Info

if TYPE_CHECKING:
    from state import AppState

log = logging.getLogger("ozma.graphql.stream")


@graphql_type
class StreamInfo:
    """
    Represents a video stream from a node.
    
    Fields:
        node_id: ID of the node
        type: Stream type (hls, mjpeg, none)
        url: URL to access the stream
        active: Whether the stream is currently active
        encoder: Encoder being used (e.g., "h264_nvenc", "libx265")
        hw_type: Hardware type (nvenc, vaapi, qsv, software)
        codec_family: Codec family (h264, h265, av1)
        width: Display width in pixels
        height: Display height in pixels
        fps_actual: Actual frames per second
        bitrate: Current bitrate in kbps
        uptime_s: Uptime in seconds
        restarts: Number of restarts
    """
    node_id: str
    type: str
    url: str
    active: bool
    encoder: str
    hw_type: str
    codec_family: str
    width: int
    height: int
    fps_actual: float
    bitrate: int
    uptime_s: float
    restarts: int


async def resolve_stream_info(
    info: Info,
    node_id: str,
) -> StreamInfo | None:
    """
    Get stream information for a node.
    
    Args:
        node_id: ID of the node
        
    Returns:
        StreamInfo: Current stream status, or None if no stream
        
    Raises:
        ValueError: If node_id is empty
    """
    if not node_id or not node_id.strip():
        raise ValueError("node_id cannot be empty")

    state: AppState = info.context["state"]
    
    if node_id not in state.nodes:
        return None

    node = state.nodes[node_id]
    
    # Get stream info from StreamManager
    streams = getattr(state, 'streams', None)
    
    if streams and hasattr(streams, '_captures'):
        entry = streams._captures.get(node_id)
        if entry:
            # Extract stream stats from the entry
            encoder = "unknown"
            hw_type = "software"
            codec_family = "h265"
            width = 1920
            height = 1080
            fps_actual = 0.0
            bitrate = 0
            uptime_s = 0.0
            restarts = 0
            active = False
            
            if hasattr(entry, 'stats'):
                stats = entry.stats
                encoder = getattr(stats, 'encoder', 'unknown')
                hw_type = getattr(stats, 'hw_type', 'software')
                codec_family = getattr(stats, 'codec_family', 'h265')
                fps_actual = getattr(stats, 'fps_actual', 0.0)
                restarts = getattr(stats, 'restarts', 0)
                uptime_s = getattr(stats, 'uptime_s', 0.0)
                active = getattr(stats, 'active', False)
            
            # Get dimensions from VNC if available
            if hasattr(streams, 'vnc_dimensions'):
                dims = streams.vnc_dimensions(node_id)
                if dims:
                    width, height = dims
            
            # Determine stream type and URL
            stream_type = streams.stream_type(node_id) if streams else "none"
            stream_url = streams.stream_url(node_id) if streams else None
            
            return StreamInfo(
                node_id=node_id,
                type=stream_type if stream_type else "none",
                url=stream_url or "",
                active=active,
                encoder=encoder,
                hw_type=hw_type,
                codec_family=codec_family,
                width=width,
                height=height,
                fps_actual=fps_actual,
                bitrate=bitrate,
                uptime_s=uptime_s,
                restarts=restarts,
            )
    
    return None


async def resolve_stream_info_for_active_node(
    info: Info,
) -> StreamInfo | None:
    """
    Get stream information for the currently active node.
    
    Returns:
        StreamInfo: Current stream status, or None if no active node
    """
    state: AppState = info.context["state"]
    
    if not state.active_node_id:
        return None
    
    return await resolve_stream_info(info, state.active_node_id)


async def resolve_all_streams(info: Info) -> list[StreamInfo]:
    """
    Get stream information for all nodes with streams.
    
    Returns:
        List[StreamInfo]: All active streams
    """
    state: AppState = info.context["state"]
    streams: list[StreamInfo] = []
    
    for node_id in state.nodes:
        stream = await resolve_stream_info(info, node_id)
        if stream:
            streams.append(stream)
    
    return streams


@graphql_type
class CameraInfo:
    """
    Represents a camera node and its streams.
    
    Fields:
        node_id: ID of the camera node
        name: Camera name (from mDNS or registration)
        streams: List of stream configurations
        frigate_host: Frigate API host (if configured)
        frigate_port: Frigate API port (default 5000)
        active: Whether this camera node is currently active
        last_seen: Timestamp of last mDNS heartbeat
        capabilities: List of camera capabilities
    """
    node_id: str
    name: str
    streams: list[dict]
    frigate_host: str | None
    frigate_port: int | None
    active: bool
    last_seen: float
    capabilities: list[str]


async def resolve_camera_info(
    info: Info,
    node_id: str,
) -> CameraInfo | None:
    """
    Get camera information for a node.
    
    Args:
        node_id: ID of the camera node
        
    Returns:
        CameraInfo: Camera details, or None if not a camera node
        
    Raises:
        ValueError: If node_id is empty
    """
    if not node_id or not node_id.strip():
        raise ValueError("node_id cannot be empty")

    state: AppState = info.context["state"]
    
    if node_id not in state.nodes:
        return None

    node = state.nodes[node_id]
    
    # Check if this is a camera node
    if node.machine_class != "camera":
        return None

    # Extract camera streams from node config
    streams = node.camera_streams or []
    
    # Get Frigate integration info
    frigate_host = node.frigate_host
    frigate_port = node.frigate_port or 5000
    
    return CameraInfo(
        node_id=node_id,
        name=node.id.split('.')[0] if '.' in node.id else node.id,  # Extract name from mDNS
        streams=streams,
        frigate_host=frigate_host,
        frigate_port=frigate_port,
        active=state.active_node_id == node_id,
        last_seen=node.last_seen,
        capabilities=list(node.capabilities) if node.capabilities else [],
    )


async def resolve_all_cameras(info: Info) -> list[CameraInfo]:
    """
    Get information for all camera nodes.
    
    Returns:
        List[CameraInfo]: All configured camera nodes
    """
    state: AppState = info.context["state"]
    cameras: list[CameraInfo] = []
    
    for node_id, node in state.nodes.items():
        if node.machine_class == "camera":
            camera = await resolve_camera_info(info, node_id)
            if camera:
                cameras.append(camera)
    
    return cameras
