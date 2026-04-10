# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
GraphQL types for audio routing.
"""

import logging
from typing import TYPE_CHECKING

from strawberry import type as graphql_type
from strawberry.types import Info

if TYPE_CHECKING:
    from state import AppState

log = logging.getLogger("ozma.graphql.audio")


@graphql_type
class AudioRoute:
    """
    Represents an audio routing connection between source and target.
    
    Fields:
        source_id: ID of the source node or audio sink
        target_id: ID of the target output or node
        active: Whether this route is currently active
    """
    source_id: str
    target_id: str
    active: bool


@graphql_type
class AudioVolume:
    """
    Represents audio volume control for a node.
    
    Fields:
        node_id: ID of the node
        volume: Volume level (0.0 to 1.0, where 1.0 is 0dB)
        muted: Whether audio is muted
        audio_type: Type of audio routing (pipewire, vban, or none)
    """
    node_id: str
    volume: float
    muted: bool
    audio_type: str


async def resolve_audio_route(
    info: Info,
    source_id: str,
    target_id: str,
) -> AudioRoute:
    """
    Get the audio route between a source and target.
    
    Args:
        source_id: ID of the source node or audio sink
        target_id: ID of the target output or node
        
    Returns:
        AudioRoute: The current route configuration
        
    Raises:
        ValueError: If source or target ID is empty
    """
    if not source_id or not source_id.strip():
        raise ValueError("source_id cannot be empty")
    if not target_id or not target_id.strip():
        raise ValueError("target_id cannot be empty")

    state: AppState = info.context["state"]
    
    # Check if the route is active by checking if the source node is active
    # and if it's routed to the target
    active = False
    
    if state.active_node_id == source_id:
        # The source node is active; check if target matches
        node = state.nodes.get(source_id)
        if node and node.audio_sink and node.audio_sink == target_id:
            active = True
    
    return AudioRoute(
        source_id=source_id,
        target_id=target_id,
        active=active,
    )


async def resolve_audio_volume(
    info: Info,
    node_id: str,
) -> AudioVolume:
    """
    Get the audio volume for a node.
    
    Args:
        node_id: ID of the node
        
    Returns:
        AudioVolume: Current volume settings
        
    Raises:
        ValueError: If node_id is empty
        LookupError: If node not found
    """
    if not node_id or not node_id.strip():
        raise ValueError("node_id cannot be empty")

    state: AppState = info.context["state"]
    
    if node_id not in state.nodes:
        raise LookupError(f"Node '{node_id}' not found")

    node = state.nodes[node_id]
    
    # Get volume and mute state from audio router
    volume = 0.5  # default
    muted = False
    audio_type = node.audio_type or "none"
    
    return AudioVolume(
        node_id=node_id,
        volume=volume,
        muted=muted,
        audio_type=audio_type,
    )


async def resolve_audio_routes(info: Info) -> list[AudioRoute]:
    """
    Get all active audio routes.
    
    Returns:
        List[AudioRoute]: All currently active audio routes
    """
    state: AppState = info.context["state"]
    routes: list[AudioRoute] = []
    
    active_node_id = state.active_node_id
    if not active_node_id:
        return routes
    
    node = state.nodes.get(active_node_id)
    if node and node.audio_sink:
        routes.append(AudioRoute(
            source_id=active_node_id,
            target_id=node.audio_sink,
            active=True,
        ))
    
    return routes
