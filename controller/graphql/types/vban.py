# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
GraphQL types for VBAN audio streams.
"""

import logging
from datetime import datetime
from typing import TYPE_CHECKING

from strawberry import type as graphql_type
from strawberry.types import Info

if TYPE_CHECKING:
    from state import AppState

log = logging.getLogger("ozma.graphql.vban")


@graphql_type
class VBANStream:
    """
    Represents a VBAN audio stream connection.

    Fields:
        node_id: ID of the node
        node_name: Human-readable name of the node
        port: UDP port used for VBAN transmission
        stream_name: Name of the VBAN stream
        sample_rate: Sample rate in Hz (48000, 44100, etc.)
        channels: Number of audio channels (1=mono, 2=stereo)
        active: Whether the stream is currently active
        frames_received: Total frames received since start
        last_frame_at: Timestamp of last received frame
        last_frame_at_iso: ISO 8601 formatted timestamp
    """
    node_id: str
    node_name: str | None
    port: int
    stream_name: str
    sample_rate: int
    channels: int
    active: bool
    frames_received: int
    last_frame_at: float
    last_frame_at_iso: str | None


async def resolve_vban_stream(
    info: Info,
    node_id: str,
) -> VBANStream | None:
    """
    Get the VBAN stream status for a node.

    Args:
        node_id: ID of the node

    Returns:
        VBANStream: Current stream status, or None if not configured

    Raises:
        ValueError: If node_id is empty
    """
    if not node_id or not node_id.strip():
        raise ValueError("node_id cannot be empty")

    state: AppState = info.context["state"]

    if node_id not in state.nodes:
        return None

    node = state.nodes[node_id]

    # Check if node has VBAN audio configured
    if node.audio_type != "vban":
        return None

    # Get VBAN port from node config
    port = node.audio_vban_port or 6980  # default VBAN port

    # Get stream info from audio router
    stream_name = f"ozma-{node_id}"
    sample_rate = 48000
    channels = 2
    active = state.active_node_id == node_id
    frames_received = 0  # Would need to track this in audio router
    last_frame_at = 0.0
    last_frame_at_iso = None

    # Note: To get actual frame counts, we would need to access the VBANReceiver
    # instance from the AudioRouter, which requires exposing internal state

    if last_frame_at > 0:
        try:
            dt = datetime.fromtimestamp(last_frame_at)
            last_frame_at_iso = dt.isoformat()
        except Exception:
            pass

    node_name = node.id.split('.')[0] if '.' in node.id else node.id

    return VBANStream(
        node_id=node_id,
        node_name=node_name,
        port=port,
        stream_name=stream_name,
        sample_rate=sample_rate,
        channels=channels,
        active=active,
        frames_received=frames_received,
        last_frame_at=last_frame_at,
        last_frame_at_iso=last_frame_at_iso,
    )


async def resolve_vban_streams(info: Info) -> list[VBANStream]:
    """
    Get all VBAN audio streams.

    Returns:
        List[VBANStream]: All configured VBAN streams
    """
    state: AppState = info.context["state"]
    streams: list[VBANStream] = []

    for node_id, node in state.nodes.items():
        if node.audio_type == "vban":
            stream = await resolve_vban_stream(info, node_id)
            if stream:
                streams.append(stream)

    return streams
