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

# Import resolvers (module-level to avoid circular import issues)
from .types.audio import (
    resolve_audio_route,
    resolve_audio_volume,
    resolve_audio_routes,
)
from .types.vban import resolve_vban_stream, resolve_vban_streams
from .types.stream import (
    resolve_stream_info,
    resolve_stream_info_for_active_node,
    resolve_all_streams,
    resolve_camera_info,
    resolve_all_cameras,
)
from .types.controls import (
    resolve_control_surface,
    resolve_all_control_surfaces,
    resolve_active_control_surface,
)
from .types.system import resolve_system_health

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
    async def audio_route(
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
        return await resolve_audio_route(info, source_id, target_id)

    @strawberry.field
    async def audio_volume(
        self,
        info: Info,
        node_id: str,
    ) -> AudioVolume:
        """
        Get the audio volume for a node.

        Args:
            node_id: ID of the node
        """
        return await resolve_audio_volume(info, node_id)

    @strawberry.field
    async def audio_routes(
        self,
        info: Info,
    ) -> list[AudioRoute]:
        """
        Get all active audio routes.
        """
        return await resolve_audio_routes(info)

    # --- VBAN Streams ---

    @strawberry.field
    async def vban_stream(
        self,
        info: Info,
        node_id: str,
    ) -> VBANStream | None:
        """
        Get the VBAN stream status for a node.

        Args:
            node_id: ID of the node
        """
        return await resolve_vban_stream(info, node_id)

    @strawberry.field
    async def vban_streams(
        self,
        info: Info,
    ) -> list[VBANStream]:
        """
        Get all VBAN audio streams.
        """
        return await resolve_vban_streams(info)

    # --- Stream Info ---

    @strawberry.field
    async def stream_info(
        self,
        info: Info,
        node_id: str,
    ) -> StreamInfo | None:
        """
        Get stream information for a node.

        Args:
            node_id: ID of the node
        """
        return await resolve_stream_info(info, node_id)

    @strawberry.field
    async def stream_info_for_active_node(
        self,
        info: Info,
    ) -> StreamInfo | None:
        """
        Get stream information for the currently active node.
        """
        return await resolve_stream_info_for_active_node(info)

    @strawberry.field
    async def all_streams(
        self,
        info: Info,
    ) -> list[StreamInfo]:
        """
        Get stream information for all nodes with streams.
        """
        return await resolve_all_streams(info)

    # --- Camera Info ---

    @strawberry.field
    async def camera_info(
        self,
        info: Info,
        node_id: str,
    ) -> CameraInfo | None:
        """
        Get camera information for a node.

        Args:
            node_id: ID of the camera node
        """
        return await resolve_camera_info(info, node_id)

    @strawberry.field
    async def all_cameras(
        self,
        info: Info,
    ) -> list[CameraInfo]:
        """
        Get information for all camera nodes.
        """
        return await resolve_all_cameras(info)

    # --- Control Surfaces ---

    @strawberry.field
    async def control_surface(
        self,
        info: Info,
        surface_id: str,
    ) -> ControlSurface | None:
        """
        Get a specific control surface by ID.

        Args:
            surface_id: ID of the control surface
        """
        return await resolve_control_surface(info, surface_id)

    @strawberry.field
    async def all_control_surfaces(
        self,
        info: Info,
    ) -> list[ControlSurface]:
        """
        Get all registered control surfaces.
        """
        return await resolve_all_control_surfaces(info)

    @strawberry.field
    async def active_control_surface(
        self,
        info: Info,
    ) -> ControlSurface | None:
        """
        Get the currently active control surface.
        """
        return await resolve_active_control_surface(info)

    # --- System Health ---

    @strawberry.field
    async def system_health(
        self,
        info: Info,
    ) -> SystemHealth:
        """
        Get overall system health status.
        """
        return await resolve_system_health(info)


# Create the GraphQL schema
schema = strawberry.Schema(
    query=Query,
    # Add mutation and subscription types here when needed
)
