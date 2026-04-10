# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
GraphQL Schema for Ozma Controller.

This module defines the GraphQL schema using Strawberry GraphQL.
It includes queries for:
- Audio routing (AudioRoute, AudioVolume)
- VBAN streams (VBANStream)
- Video streams (StreamInfo, CameraInfo)
- Control surfaces (ControlSurface, Binding)
- System health (SystemHealth)
"""

import asyncio
import logging
from typing import TYPE_CHECKING, Any

import strawberry
from strawberry.types import Info

from .types import (
    AudioRoute,
    AudioVolume,
    VBANStream,
    StreamInfo,
    CameraInfo,
    ControlSurface,
    Binding,
    SystemHealth,
)

if TYPE_CHECKING:
    from state import AppState

log = logging.getLogger("ozma.graphql.schema")


@strawberry.type
class Query:
    """
    GraphQL root query type.
    
    All queries in the Ozma GraphQL API are defined here.
    Each query method receives an Info context containing the AppState.
    """
    
    # --- Audio Routes ---
    
    @strawberry.field
    def audio_route(
        self,
        info: Info,
        source_id: str,
        target_id: str,
    ) -> AudioRoute:
        """
        Get the audio route between a source and target.
        
        Args:
            source_id: ID of the source node or audio sink
            target_id: ID of the target output or node
        """
        # Run async resolver in event loop
        loop = asyncio.get_event_loop()
        return loop.run_until_complete(resolve_audio_route(info, source_id, target_id))
    
    @strawberry.field
    def audio_volume(
        self,
        info: Info,
        node_id: str,
    ) -> AudioVolume:
        """
        Get the audio volume for a node.
        
        Args:
            node_id: ID of the node
        """
        loop = asyncio.get_event_loop()
        return loop.run_until_complete(resolve_audio_volume(info, node_id))
    
    @strawberry.field
    def audio_routes(
        self,
        info: Info,
    ) -> list[AudioRoute]:
        """
        Get all active audio routes.
        """
        loop = asyncio.get_event_loop()
        return loop.run_until_complete(resolve_audio_routes(info))
    
    # --- VBAN Streams ---
    
    @strawberry.field
    def vban_stream(
        self,
        info: Info,
        node_id: str,
    ) -> VBANStream | None:
        """
        Get the VBAN stream status for a node.
        
        Args:
            node_id: ID of the node
        """
        loop = asyncio.get_event_loop()
        return loop.run_until_complete(resolve_vban_stream(info, node_id))
    
    @strawberry.field
    def vban_streams(
        self,
        info: Info,
    ) -> list[VBANStream]:
        """
        Get all VBAN audio streams.
        """
        loop = asyncio.get_event_loop()
        return loop.run_until_complete(resolve_vban_streams(info))
    
    # --- Stream Info ---
    
    @strawberry.field
    def stream_info(
        self,
        info: Info,
        node_id: str,
    ) -> StreamInfo | None:
        """
        Get stream information for a node.
        
        Args:
            node_id: ID of the node
        """
        loop = asyncio.get_event_loop()
        return loop.run_until_complete(resolve_stream_info(info, node_id))
    
    @strawberry.field
    def stream_info_for_active_node(
        self,
        info: Info,
    ) -> StreamInfo | None:
        """
        Get stream information for the currently active node.
        """
        loop = asyncio.get_event_loop()
        return loop.run_until_complete(resolve_stream_info_for_active_node(info))
    
    @strawberry.field
    def all_streams(
        self,
        info: Info,
    ) -> list[StreamInfo]:
        """
        Get stream information for all nodes with streams.
        """
        loop = asyncio.get_event_loop()
        return loop.run_until_complete(resolve_all_streams(info))
    
    # --- Camera Info ---
    
    @strawberry.field
    def camera_info(
        self,
        info: Info,
        node_id: str,
    ) -> CameraInfo | None:
        """
        Get camera information for a node.
        
        Args:
            node_id: ID of the camera node
        """
        loop = asyncio.get_event_loop()
        return loop.run_until_complete(resolve_camera_info(info, node_id))
    
    @strawberry.field
    def all_cameras(
        self,
        info: Info,
    ) -> list[CameraInfo]:
        """
        Get information for all camera nodes.
        """
        loop = asyncio.get_event_loop()
        return loop.run_until_complete(resolve_all_cameras(info))
    
    # --- Control Surfaces ---
    
    @strawberry.field
    def control_surface(
        self,
        info: Info,
        surface_id: str,
    ) -> ControlSurface | None:
        """
        Get a specific control surface by ID.
        
        Args:
            surface_id: ID of the control surface
        """
        loop = asyncio.get_event_loop()
        return loop.run_until_complete(resolve_control_surface(info, surface_id))
    
    @strawberry.field
    def all_control_surfaces(
        self,
        info: Info,
    ) -> list[ControlSurface]:
        """
        Get all registered control surfaces.
        """
        loop = asyncio.get_event_loop()
        return loop.run_until_complete(resolve_all_control_surfaces(info))
    
    @strawberry.field
    def active_control_surface(
        self,
        info: Info,
    ) -> ControlSurface | None:
        """
        Get the currently active control surface.
        """
        loop = asyncio.get_event_loop()
        return loop.run_until_complete(resolve_active_control_surface(info))
    
    # --- System Health ---
    
    @strawberry.field
    def system_health(
        self,
        info: Info,
    ) -> SystemHealth:
        """
        Get overall system health status.
        """
        loop = asyncio.get_event_loop()
        return loop.run_until_complete(resolve_system_health(info))


async def resolve_audio_route(
    info: Info,
    source_id: str,
    target_id: str,
) -> AudioRoute:
    """Async resolver for audio_route."""
    from .types.audio import resolve_audio_route
    return await resolve_audio_route(info, source_id, target_id)


async def resolve_audio_volume(
    info: Info,
    node_id: str,
) -> AudioVolume:
    """Async resolver for audio_volume."""
    from .types.audio import resolve_audio_volume
    return await resolve_audio_volume(info, node_id)


async def resolve_audio_routes(
    info: Info,
) -> list[AudioRoute]:
    """Async resolver for audio_routes."""
    from .types.audio import resolve_audio_routes
    return await resolve_audio_routes(info)


async def resolve_vban_stream(
    info: Info,
    node_id: str,
) -> VBANStream | None:
    """Async resolver for vban_stream."""
    from .types.vban import resolve_vban_stream
    return await resolve_vban_stream(info, node_id)


async def resolve_vban_streams(
    info: Info,
) -> list[VBANStream]:
    """Async resolver for vban_streams."""
    from .types.vban import resolve_vban_streams
    return await resolve_vban_streams(info)


async def resolve_stream_info(
    info: Info,
    node_id: str,
) -> StreamInfo | None:
    """Async resolver for stream_info."""
    from .types.stream import resolve_stream_info
    return await resolve_stream_info(info, node_id)


async def resolve_stream_info_for_active_node(
    info: Info,
) -> StreamInfo | None:
    """Async resolver for stream_info_for_active_node."""
    from .types.stream import resolve_stream_info_for_active_node
    return await resolve_stream_info_for_active_node(info)


async def resolve_all_streams(
    info: Info,
) -> list[StreamInfo]:
    """Async resolver for all_streams."""
    from .types.stream import resolve_all_streams
    return await resolve_all_streams(info)


async def resolve_camera_info(
    info: Info,
    node_id: str,
) -> CameraInfo | None:
    """Async resolver for camera_info."""
    from .types.stream import resolve_camera_info
    return await resolve_camera_info(info, node_id)


async def resolve_all_cameras(
    info: Info,
) -> list[CameraInfo]:
    """Async resolver for all_cameras."""
    from .types.stream import resolve_all_cameras
    return await resolve_all_cameras(info)


async def resolve_control_surface(
    info: Info,
    surface_id: str,
) -> ControlSurface | None:
    """Async resolver for control_surface."""
    from .types.controls import resolve_control_surface
    return await resolve_control_surface(info, surface_id)


async def resolve_all_control_surfaces(
    info: Info,
) -> list[ControlSurface]:
    """Async resolver for all_control_surfaces."""
    from .types.controls import resolve_all_control_surfaces
    return await resolve_all_control_surfaces(info)


async def resolve_active_control_surface(
    info: Info,
) -> ControlSurface | None:
    """Async resolver for active_control_surface."""
    from .types.controls import resolve_active_control_surface
    return await resolve_active_control_surface(info)


async def resolve_system_health(
    info: Info,
) -> SystemHealth:
    """Async resolver for system_health."""
    from .types.system import resolve_system_health
    return await resolve_system_health(info)


# Create the GraphQL schema
schema = strawberry.Schema(
    query=Query,
    # Add mutation and subscription types here when needed
)
