# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
GraphQL API for Ozma Controller.

Provides mutations for managing nodes, scenarios, audio routing, and system state.
Uses Strawberry GraphQL for the schema definition and execution.

Authentication: All mutations require the 'write' scope in the JWT token.
"""

import logging
from typing import Any

import strawberry
from strawberry.extensions import Extension
from strawberry.types import Info

strawberry_type = strawberry.type
strawberry_field = strawberry.field
Schema = strawberry.Schema

from auth import AuthContext, has_scope, SCOPE_WRITE
from state import AppState, NodeInfo
from scenarios import ScenarioManager
from audio import AudioRouter
from wol import send_wol, get_mac_from_arp

log = logging.getLogger("ozma.graphql")


# ──────────────────────────────────────────────────────────────────────────────
# GraphQL Error Codes
# ──────────────────────────────────────────────────────────────────────────────

class GraphQLError(Exception):
    """Custom GraphQL error with additional metadata."""
    def __init__(self, message: str, code: str | None = None, extensions: dict | None = None):
        self.message = message
        self.code = code
        self.extensions = extensions or {}
        super().__init__(self.message)


# ──────────────────────────────────────────────────────────────────────────────
# GraphQL Types
# ──────────────────────────────────────────────────────────────────────────────

@strawberry_type
class Node:
    """Represents a hardware or virtual node in the KVM network."""
    id: str
    name: str | None
    host: str
    port: int
    role: str
    hw: str
    fw_version: str
    proto_version: int
    capabilities: list[str]
    machine_class: str
    last_seen: float
    display_outputs: list[dict]
    vnc_host: str | None
    vnc_port: int | None
    stream_port: int | None
    stream_path: str | None
    audio_type: str | None
    audio_sink: str | None
    audio_vban_port: int | None
    mic_vban_port: int | None
    capture_device: str | None
    camera_streams: list[dict]
    frigate_host: str | None
    frigate_port: int | None
    owner_user_id: str
    owner: str | None
    shared_with: list[str]
    share_permissions: dict[str, str]
    parent_id: str | None
    sunshine_port: int | None


@strawberry_type
class Scenario:
    """A named configuration that binds a compute node to a logical context."""
    id: str
    name: str
    node_id: str | None
    color: str
    transition_in: "TransitionConfig"
    motion: dict | None
    bluetooth: dict | None
    capture_source: str | None
    capture_sources: list[str] | None
    wallpaper: dict | None


@strawberry_type
class TransitionConfig:
    style: str
    duration_ms: int


@strawberry_type
class AudioNode:
    """Represents an audio-capable node with volume and mute controls."""
    node_id: str
    volume: float
    muted: bool
    audio_type: str | None
    audio_sink: str | None
    vban_port: int | None


@strawberry_type
class AudioRoute:
    """Represents an audio routing connection between source and target."""
    source_id: str
    target_id: str
    active: bool


@strawberry_type
class WakeOnLanResult:
    """Result of a Wake-on-LAN magic packet send."""
    success: bool
    mac: str | None
    broadcast: str
    message: str


@strawberry_type
class DeleteResult:
    """Result of a delete operation."""
    success: bool
    deleted_id: str
    message: str


@strawberry_type
class SystemInfo:
    """System-level information and status."""
    version: str
    active_node_id: str | None
    active_scenario_id: str | None
    node_count: int
    scenario_count: int
    audio_enabled: bool
    auth_enabled: bool
    uptime_seconds: int


# ──────────────────────────────────────────────────────────────────────────────
# Input Types
# ──────────────────────────────────────────────────────────────────────────────

@strawberry_type
class TransitionInput:
    style: str | None = None
    duration_ms: int | None = None


@strawberry_type
class MotionPresetInput:
    device_id: str
    axis: str
    position: float


@strawberry_type
class BluetoothConfigInput:
    connect: list[str] | None = None
    disconnect: list[str] | None = None


@strawberry_type
class WallpaperInput:
    mode: str | None = None
    color: str | None = None
    image: str | None = None
    url: str | None = None


@strawberry_type
class ScenarioInput:
    """Input for creating or updating a scenario."""
    id: str
    name: str
    node_id: str | None = None
    color: str | None = None
    transition_in: TransitionInput | None = None
    motion: list[MotionPresetInput] | None = None
    bluetooth: list[BluetoothConfigInput] | None = None
    capture_source: str | None = None
    capture_sources: list[str] | None = None
    wallpaper: WallpaperInput | None = None


# ──────────────────────────────────────────────────────────────────────────────
# Enums
# ──────────────────────────────────────────────────────────────────────────────

@strawberry_type
class MachineClass:
    """Machine class types."""
    WORKSTATION = "workstation"
    SERVER = "server"
    KIOSK = "kiosk"
    CAMERA = "camera"


@strawberry_type
class AudioType:
    """Audio routing types."""
    PIPEWIRE = "pipewire"
    VBAN = "vban"
    NONE = "none"


@strawberry_type
class ScenarioTransitionStyle:
    """Scenario transition styles."""
    CUT = "cut"
    WAVE_RIGHT = "wave_right"
    WAVE_LEFT = "wave_left"
    FADE = "fade"
    RIPPLE = "ripple"


# ──────────────────────────────────────────────────────────────────────────────
# Query Class
# ──────────────────────────────────────────────────────────────────────────────

@strawberry_type
class Query:
    """GraphQL queries for reading node, scenario, audio, and system state."""

    @strawberry_type.field
    def nodes(self, info: Info) -> list[Node]:
        """
        List all known nodes.

        Returns:
            list[Node]: All registered nodes
        """
        state = _get_state(info)
        if not state:
            return []
        return [_node_to_graphql(node) for node in state.nodes.values()]

    @strawberry_type.field
    def node(self, info: Info, id: str) -> Node | None:
        """
        Get a single node by ID.

        Args:
            id: The node ID

        Returns:
            Node | None: The node if found, None otherwise
        """
        state = _get_state(info)
        if not state:
            return None
        node = state.nodes.get(id)
        if node:
            return _node_to_graphql(node)
        return None

    @strawberry_type.field
    def active_node(self, info: Info) -> Node | None:
        """
        Get the currently active node.

        Returns:
            Node | None: The active node if set, None otherwise
        """
        state = _get_state(info)
        if not state:
            return None
        node_id = state.active_node_id
        if node_id and node_id in state.nodes:
            return _node_to_graphql(state.nodes[node_id])
        return None

    @strawberry_type.field
    def scenarios(self, info: Info) -> list[Scenario]:
        """
        List all scenarios.

        Returns:
            list[Scenario]: All registered scenarios
        """
        scenario_mgr = _get_scenario_manager(info)
        if not scenario_mgr:
            return []
        return [_scenario_to_graphql(s) for s in scenario_mgr.list()]

    @strawberry_type.field
    def scenario(self, info: Info, id: str) -> Scenario | None:
        """
        Get a single scenario by ID.

        Args:
            id: The scenario ID

        Returns:
            Scenario | None: The scenario if found, None otherwise
        """
        scenario_mgr = _get_scenario_manager(info)
        if not scenario_mgr:
            return None
        scenario = scenario_mgr.get(id)
        if scenario:
            return _scenario_to_graphql(scenario)
        return None

    @strawberry_type.field
    def active_scenario(self, info: Info) -> Scenario | None:
        """
        Get the currently active scenario.

        Returns:
            Scenario | None: The active scenario if set, None otherwise
        """
        scenario_mgr = _get_scenario_manager(info)
        if not scenario_mgr:
            return None
        active_id = scenario_mgr.active_id
        if active_id:
            scenario = scenario_mgr.get(active_id)
            if scenario:
                return _scenario_to_graphql(scenario)
        return None

    @strawberry_type.field
    def audio_nodes(self, info: Info) -> list[AudioNode]:
        """
        List all audio-capable nodes.

        Returns:
            list[AudioNode]: All nodes with audio capability
        """
        state = _get_state(info)
        if not state:
            return []
        return [_get_audio_node(state, node.id) for node in state.nodes.values()]

    @strawberry_type.field
    def system_info(self, info: Info) -> SystemInfo:
        """
        Get system-level information and status.

        Returns:
            SystemInfo: System status and configuration
        """
        state = _get_state(info)
        scenario_mgr = _get_scenario_manager(info)

        # Get version
        version = "0.1.0"  # TODO: Get from build_info

        # Check if auth is enabled
        auth_enabled = False  # TODO: Get from auth config

        # Calculate uptime
        uptime_seconds = 0  # TODO: Track controller start time

        return SystemInfo(
            version=version,
            active_node_id=state.active_node_id if state else None,
            active_scenario_id=scenario_mgr.active_id if scenario_mgr else None,
            node_count=len(state.nodes) if state else 0,
            scenario_count=len(scenario_mgr._scenarios) if scenario_mgr else 0,
            audio_enabled=True,  # TODO: Get from audio router config
            auth_enabled=auth_enabled,
            uptime_seconds=uptime_seconds,
        )


# ──────────────────────────────────────────────────────────────────────────────
# Mutation Class
# ──────────────────────────────────────────────────────────────────────────────

@strawberry_type
class Mutation:
    """GraphQL mutations for managing nodes, scenarios, audio, and system state."""

    # ── Node Mutations ────────────────────────────────────────────────────────

    @strawberry_type.field
    async def activate_node(self, info: Info, id: str) -> Node:
        """
        Makes a node active, routing all HID packets to it.

        Args:
            id: The node ID to activate

        Returns:
            Node: The activated node

        Raises:
            GraphQLError: If node not found or activation fails
        """
        _require_write_scope(info)

        state = _get_state(info)
        if not state:
            raise GraphQLError(
                "AppState not available",
                code="INTERNAL_ERROR",
                extensions={"detail": "AppState is not initialized"}
            )

        if id not in state.nodes:
            raise GraphQLError(
                f"Node not found: {id}",
                code="NODE_NOT_FOUND",
                extensions={"node_id": id}
            )

        try:
            await state.set_active_node(id)
            node = state.nodes[id]
            log.info("Node activated: %s", id)
            return _node_to_graphql(node)
        except Exception as e:
            log.error("Failed to activate node %s: %s", id, e)
            raise GraphQLError(
                f"Failed to activate node: {str(e)}",
                code="GRAPHQL_ERROR",
                extensions={"node_id": id, "error": str(e)}
            )

    @strawberry_type.field
    async def rename_node(self, info: Info, id: str, name: str) -> Node:
        """
        Renames a node.

        Args:
            id: The node ID to rename
            name: The new name for the node

        Returns:
            Node: The renamed node

        Raises:
            GraphQLError: If node not found or name is invalid
        """
        _require_write_scope(info)

        # Validate input
        if not name or len(name.strip()) == 0:
            raise GraphQLError(
                "Node name cannot be empty",
                code="INVALID_NAME",
                extensions={"field": "name"}
            )

        if len(name) > 255:
            raise GraphQLError(
                "Node name is too long (max 255 characters)",
                code="INVALID_NAME",
                extensions={"field": "name", "max_length": 255}
            )

        state = _get_state(info)
        if not state:
            raise GraphQLError(
                "AppState not available",
                code="INTERNAL_ERROR",
                extensions={"detail": "AppState is not initialized"}
            )

        if id not in state.nodes:
            raise GraphQLError(
                f"Node not found: {id}",
                code="NODE_NOT_FOUND",
                extensions={"node_id": id}
            )

        try:
            node = state.nodes[id]
            node.name = name.strip()
            log.info("Node renamed: %s → %s", id, name)
            return _node_to_graphql(node)
        except Exception as e:
            log.error("Failed to rename node %s: %s", id, e)
            raise GraphQLError(
                f"Failed to rename node: {str(e)}",
                code="GRAPHQL_ERROR",
                extensions={"node_id": id, "error": str(e)}
            )

    @strawberry_type.field
    async def wake_on_lan(self, info: Info, id: str) -> WakeOnLanResult:
        """
        Sends a Wake-on-LAN magic packet to a node.

        Args:
            id: The node ID to wake up

        Returns:
            WakeOnLanResult: The result of the WoL operation

        Raises:
            GraphQLError: If node not found or WoL not supported
        """
        _require_write_scope(info)

        state = _get_state(info)
        if not state:
            raise GraphQLError(
                "AppState not available",
                code="INTERNAL_ERROR",
                extensions={"detail": "AppState is not initialized"}
            )

        if id not in state.nodes:
            raise GraphQLError(
                f"Node not found: {id}",
                code="NODE_NOT_FOUND",
                extensions={"node_id": id}
            )

        node = state.nodes[id]

        # Try to get MAC from node metadata first
        mac = None
        broadcast = "255.255.255.255"

        # Check if node has MAC address in capabilities or metadata
        if "mac" in node.capabilities:
            # This is a placeholder - in practice, MAC would be in node metadata
            pass

        # Try to get MAC from ARP table using node host
        if node.host:
            mac = get_mac_from_arp(node.host)
            if mac:
                broadcast = _get_broadcast_address(node.host)

        if not mac:
            # WoL not supported - node doesn't have a known MAC address
            log.warning("Wake-on-LAN not supported for node %s (no MAC available)", id)
            return WakeOnLanResult(
                success=False,
                mac=None,
                broadcast=broadcast,
                message="Wake-on-LAN not supported: MAC address not available"
            )

        try:
            success = send_wol(mac, broadcast)
            if success:
                log.info("Wake-on-LAN sent to %s (%s)", id, mac)
                return WakeOnLanResult(
                    success=True,
                    mac=mac,
                    broadcast=broadcast,
                    message=f"Wake-on-LAN magic packet sent to {mac}"
                )
            else:
                log.warning("Wake-on-LAN failed for %s (%s)", id, mac)
                return WakeOnLanResult(
                    success=False,
                    mac=mac,
                    broadcast=broadcast,
                    message=f"Wake-on-LAN failed for {mac}"
                )
        except Exception as e:
            log.error("Wake-on-LAN error for %s: %s", id, e)
            return WakeOnLanResult(
                success=False,
                mac=mac,
                broadcast=broadcast,
                message=f"Wake-on-LAN error: {str(e)}"
            )

    # ── Scenario Mutations ────────────────────────────────────────────────────

    @strawberry_type.field
    async def create_scenario(self, info: Info, input: ScenarioInput) -> Scenario:
        """
        Creates a new scenario.

        Args:
            input: Scenario configuration

        Returns:
            Scenario: The created scenario

        Raises:
            GraphQLError: If scenario already exists or input is invalid
        """
        _require_write_scope(info)

        # Validate input
        if not input.name or len(input.name.strip()) == 0:
            raise GraphQLError(
                "Scenario name cannot be empty",
                code="INVALID_INPUT",
                extensions={"field": "name"}
            )

        if len(input.name) > 255:
            raise GraphQLError(
                "Scenario name is too long (max 255 characters)",
                code="INVALID_INPUT",
                extensions={"field": "name", "max_length": 255}
            )

        state = _get_state(info)
        scenario_mgr = _get_scenario_manager(info)

        if not scenario_mgr:
            raise GraphQLError(
                "ScenarioManager not available",
                code="INTERNAL_ERROR",
                extensions={"detail": "ScenarioManager is not initialized"}
            )

        try:
            # Create the scenario
            scenario = await scenario_mgr.create(
                scenario_id=input.id,
                name=input.name.strip(),
                node_id=input.node_id
            )

            log.info("Scenario created: %s", input.id)
            return _scenario_to_graphql(scenario)
        except ValueError as e:
            if "already exists" in str(e):
                raise GraphQLError(
                    str(e),
                    code="SCENARIO_EXISTS",
                    extensions={"scenario_id": input.id}
                )
            raise GraphQLError(
                f"Failed to create scenario: {str(e)}",
                code="INVALID_INPUT",
                extensions={"scenario_id": input.id, "error": str(e)}
            )
        except Exception as e:
            log.error("Failed to create scenario %s: %s", input.id, e)
            raise GraphQLError(
                f"Failed to create scenario: {str(e)}",
                code="GRAPHQL_ERROR",
                extensions={"scenario_id": input.id, "error": str(e)}
            )

    @strawberry_type.field
    async def update_scenario(self, info: Info, id: str, input: ScenarioInput) -> Scenario:
        """
        Updates an existing scenario.

        Args:
            id: The scenario ID to update
            input: New scenario configuration

        Returns:
            Scenario: The updated scenario

        Raises:
            GraphQLError: If scenario not found or input is invalid
        """
        _require_write_scope(info)

        # Validate input
        if not input.name or len(input.name.strip()) == 0:
            raise GraphQLError(
                "Scenario name cannot be empty",
                code="INVALID_INPUT",
                extensions={"field": "name"}
            )

        if len(input.name) > 255:
            raise GraphQLError(
                "Scenario name is too long (max 255 characters)",
                code="INVALID_INPUT",
                extensions={"field": "name", "max_length": 255}
            )

        scenario_mgr = _get_scenario_manager(info)

        if not scenario_mgr:
            raise GraphQLError(
                "ScenarioManager not available",
                code="INTERNAL_ERROR",
                extensions={"detail": "ScenarioManager is not initialized"}
            )

        try:
            scenario = scenario_mgr.get(id)
            if not scenario:
                raise GraphQLError(
                    f"Scenario not found: {id}",
                    code="SCENARIO_NOT_FOUND",
                    extensions={"scenario_id": id}
                )

            # Update fields
            scenario.name = input.name.strip()
            if input.node_id is not None:
                scenario.node_id = input.node_id

            if input.color:
                scenario.color = input.color

            if input.transition_in:
                scenario.transition_in.style = input.transition_in.style or "cut"
                scenario.transition_in.duration_ms = input.transition_in.duration_ms or 400

            if input.capture_source:
                scenario.capture_source = input.capture_source

            if input.capture_sources:
                scenario.capture_sources = input.capture_sources

            if input.wallpaper:
                scenario.wallpaper = {
                    "mode": input.wallpaper.mode,
                    "color": input.wallpaper.color,
                    "image": input.wallpaper.image,
                    "url": input.wallpaper.url,
                }

            # Persist changes
            scenario_mgr._save()
            log.info("Scenario updated: %s", id)

            return _scenario_to_graphql(scenario)
        except GraphQLError:
            raise
        except Exception as e:
            log.error("Failed to update scenario %s: %s", id, e)
            raise GraphQLError(
                f"Failed to update scenario: {str(e)}",
                code="GRAPHQL_ERROR",
                extensions={"scenario_id": id, "error": str(e)}
            )

    @strawberry_type.field
    async def delete_scenario(self, info: Info, id: str) -> DeleteResult:
        """
        Deletes a scenario.

        Args:
            id: The scenario ID to delete

        Returns:
            DeleteResult: The result of the delete operation

        Raises:
            GraphQLError: If scenario not found or it's the active scenario
        """
        _require_write_scope(info)

        scenario_mgr = _get_scenario_manager(info)

        if not scenario_mgr:
            raise GraphQLError(
                "ScenarioManager not available",
                code="INTERNAL_ERROR",
                extensions={"detail": "ScenarioManager is not initialized"}
            )

        try:
            scenario_mgr.delete(id)
            log.info("Scenario deleted: %s", id)
            return DeleteResult(
                success=True,
                deleted_id=id,
                message=f"Scenario '{id}' deleted successfully"
            )
        except KeyError:
            raise GraphQLError(
                f"Scenario not found: {id}",
                code="SCENARIO_NOT_FOUND",
                extensions={"scenario_id": id}
            )
        except ValueError as e:
            if "active scenario" in str(e).lower():
                raise GraphQLError(
                    str(e),
                    code="ACTIVE_SCENARIO",
                    extensions={"scenario_id": id}
                )
            raise GraphQLError(
                f"Failed to delete scenario: {str(e)}",
                code="GRAPHQL_ERROR",
                extensions={"scenario_id": id, "error": str(e)}
            )
        except Exception as e:
            log.error("Failed to delete scenario %s: %s", id, e)
            raise GraphQLError(
                f"Failed to delete scenario: {str(e)}",
                code="GRAPHQL_ERROR",
                extensions={"scenario_id": id, "error": str(e)}
            )

    @strawberry_type.field
    async def activate_scenario(self, info: Info, id: str) -> Scenario:
        """
        Activates a scenario, switching to its bound node.

        Args:
            id: The scenario ID to activate

        Returns:
            Scenario: The activated scenario

        Raises:
            GraphQLError: If scenario not found or activation fails
        """
        _require_write_scope(info)

        scenario_mgr = _get_scenario_manager(info)

        if not scenario_mgr:
            raise GraphQLError(
                "ScenarioManager not available",
                code="INTERNAL_ERROR",
                extensions={"detail": "ScenarioManager is not initialized"}
            )

        try:
            scenario = await scenario_mgr.activate(id)
            log.info("Scenario activated: %s", id)
            return _scenario_to_graphql(scenario)
        except KeyError:
            raise GraphQLError(
                f"Scenario not found: {id}",
                code="SCENARIO_NOT_FOUND",
                extensions={"scenario_id": id}
            )
        except Exception as e:
            log.error("Failed to activate scenario %s: %s", id, e)
            raise GraphQLError(
                f"Failed to activate scenario: {str(e)}",
                code="GRAPHQL_ERROR",
                extensions={"scenario_id": id, "error": str(e)}
            )

    # ── Audio Mutations ───────────────────────────────────────────────────────

    @strawberry_type.field
    async def set_audio_volume(self, info: Info, node_id: str, volume: float) -> AudioNode:
        """
        Sets the audio volume for a node.

        Args:
            node_id: The node ID
            volume: Volume level (0.0 to 1.0)

        Returns:
            AudioNode: The updated audio node

        Raises:
            GraphQLError: If node not found or volume is invalid
        """
        _require_write_scope(info)

        # Validate volume range
        if volume < 0.0 or volume > 1.0:
            raise GraphQLError(
                "Volume must be between 0.0 and 1.0",
                code="INVALID_VOLUME",
                extensions={
                    "field": "volume",
                    "min": 0.0,
                    "max": 1.0,
                    "value": volume
                }
            )

        state = _get_state(info)

        if not state:
            raise GraphQLError(
                "AppState not available",
                code="INTERNAL_ERROR",
                extensions={"detail": "AppState is not initialized"}
            )

        if node_id not in state.nodes:
            raise GraphQLError(
                f"Node not found: {node_id}",
                code="NODE_NOT_FOUND",
                extensions={"node_id": node_id}
            )

        audio = _get_audio_router(info)

        try:
            if audio:
                # Convert node_id to node name for audio router
                node = state.nodes[node_id]
                # Node name is typically the first part of the mDNS instance name
                node_name = node_id.split(".")[0]
                await audio.set_volume(node_name, volume)

            log.info("Audio volume set for %s: %s", node_id, volume)
            return _get_audio_node(state, node_id)
        except Exception as e:
            log.error("Failed to set audio volume for %s: %s", node_id, e)
            raise GraphQLError(
                f"Failed to set audio volume: {str(e)}",
                code="GRAPHQL_ERROR",
                extensions={"node_id": node_id, "error": str(e)}
            )

    @strawberry_type.field
    async def mute_node(self, info: Info, node_id: str, muted: bool) -> AudioNode:
        """
        Mutes or unmutes a node's audio.

        Args:
            node_id: The node ID
            muted: True to mute, False to unmute

        Returns:
            AudioNode: The updated audio node

        Raises:
            GraphQLError: If node not found
        """
        _require_write_scope(info)

        state = _get_state(info)

        if not state:
            raise GraphQLError(
                "AppState not available",
                code="INTERNAL_ERROR",
                extensions={"detail": "AppState is not initialized"}
            )

        if node_id not in state.nodes:
            raise GraphQLError(
                f"Node not found: {node_id}",
                code="NODE_NOT_FOUND",
                extensions={"node_id": node_id}
            )

        audio = _get_audio_router(info)

        try:
            if audio:
                node = state.nodes[node_id]
                node_name = node_id.split(".")[0]
                await audio.set_mute(node_name, muted)

            log.info("Audio mute set for %s: %s", node_id, muted)
            return _get_audio_node(state, node_id)
        except Exception as e:
            log.error("Failed to set audio mute for %s: %s", node_id, e)
            raise GraphQLError(
                f"Failed to set audio mute: {str(e)}",
                code="GRAPHQL_ERROR",
                extensions={"node_id": node_id, "error": str(e)}
            )

    @strawberry_type.field
    async def set_audio_route(self, info: Info, source: str, target: str, active: bool) -> AudioRoute:
        """
        Configures an audio routing connection.

        Args:
            source: Source node ID
            target: Target node/output ID
            active: True to enable routing, False to disable

        Returns:
            AudioRoute: The updated route configuration

        Raises:
            GraphQLError: If source or target node not found
        """
        _require_write_scope(info)

        state = _get_state(info)

        if not state:
            raise GraphQLError(
                "AppState not available",
                code="INTERNAL_ERROR",
                extensions={"detail": "AppState is not initialized"}
            )

        # Validate nodes exist
        if source not in state.nodes:
            raise GraphQLError(
                f"Source node not found: {source}",
                code="NODE_NOT_FOUND",
                extensions={"node_id": source}
            )

        if target not in state.nodes:
            raise GraphQLError(
                f"Target node not found: {target}",
                code="NODE_NOT_FOUND",
                extensions={"node_id": target}
            )

        audio = _get_audio_router(info)

        try:
            if audio:
                # Route configuration would go here
                # This is a placeholder - actual routing depends on audio backend
                log.info("Audio route %s: %s → %s", "enabled" if active else "disabled", source, target)

            log.info("Audio route set for %s → %s: %s", source, target, active)
            return AudioRoute(
                source_id=source,
                target_id=target,
                active=active
            )
        except Exception as e:
            log.error("Failed to set audio route %s → %s: %s", source, target, e)
            raise GraphQLError(
                f"Failed to set audio route: {str(e)}",
                code="GRAPHQL_ERROR",
                extensions={
                    "source_id": source,
                    "target_id": target,
                    "error": str(e)
                }
            )


# ──────────────────────────────────────────────────────────────────────────────
# Helper Functions
# ──────────────────────────────────────────────────────────────────────────────

def _require_write_scope(info: Info) -> None:
    """Require write scope for mutation operations."""
    auth = getattr(info.context, "auth", None)
    if not auth or not hasattr(auth, "authenticated") or not auth.authenticated:
        raise GraphQLError(
            "Authentication required",
            code="UNAUTHENTICATED",
            extensions={"scope": "write"}
        )

    if not has_scope(auth, SCOPE_WRITE):
        raise GraphQLError(
            f"Scope '{SCOPE_WRITE}' required",
            code="UNAUTHORIZED",
            extensions={"required_scope": SCOPE_WRITE}
        )


def _get_state(info: Info) -> AppState | None:
    """Get AppState from context."""
    context = getattr(info.context, "state", None)
    return context


def _get_scenario_manager(info: Info) -> ScenarioManager | None:
    """Get ScenarioManager from context."""
    context = getattr(info.context, "scenario_manager", None)
    return context


def _get_audio_router(info: Info) -> AudioRouter | None:
    """Get AudioRouter from context."""
    context = getattr(info.context, "audio_router", None)
    return context


def _node_to_graphql(node: NodeInfo) -> Node:
    """Convert NodeInfo to GraphQL Node type."""
    return Node(
        id=node.id,
        name=node.name,
        host=node.host,
        port=node.port,
        role=node.role,
        hw=node.hw,
        fw_version=node.fw_version,
        proto_version=node.proto_version,
        capabilities=node.capabilities,
        machine_class=node.machine_class,
        last_seen=node.last_seen,
        display_outputs=node.display_outputs,
        vnc_host=node.vnc_host,
        vnc_port=node.vnc_port,
        stream_port=node.stream_port,
        stream_path=node.stream_path,
        audio_type=node.audio_type,
        audio_sink=node.audio_sink,
        audio_vban_port=node.audio_vban_port,
        mic_vban_port=node.mic_vban_port,
        capture_device=node.capture_device,
        camera_streams=node.camera_streams,
        frigate_host=node.frigate_host,
        frigate_port=node.frigate_port,
        owner_user_id=node.owner_user_id,
        owner=None,  # Would need user manager lookup
        shared_with=node.shared_with,
        share_permissions=node.share_permissions,
        parent_id=node.parent_node_id,
        sunshine_port=node.sunshine_port,
    )


def _scenario_to_graphql(scenario: Any) -> Scenario:
    """Convert Scenario to GraphQL Scenario type."""
    return Scenario(
        id=scenario.id,
        name=scenario.name,
        node_id=scenario.node_id,
        color=scenario.color,
        transition_in=TransitionConfig(
            style=scenario.transition_in.style,
            duration_ms=scenario.transition_in.duration_ms
        ),
        motion=scenario.motion,
        bluetooth=scenario.bluetooth,
        capture_source=scenario.capture_source,
        capture_sources=scenario.capture_sources,
        wallpaper=scenario.wallpaper,
    )


def _get_audio_node(state: AppState, node_id: str) -> AudioNode:
    """Get AudioNode for a given node ID."""
    node = state.nodes[node_id]

    # Get volume and mute state from audio router if available
    volume = 0.5  # Default
    muted = False  # Default

    audio = getattr(state, "audio", None)
    if audio and hasattr(audio, "watcher"):
        # These would need to be implemented in AudioRouter
        pass

    return AudioNode(
        node_id=node_id,
        volume=volume,
        muted=muted,
        audio_type=node.audio_type,
        audio_sink=node.audio_sink,
        vban_port=node.audio_vban_port,
    )


def _get_broadcast_address(ip: str) -> str:
    """Calculate broadcast address from IP."""
    # Simple approach: use 255.255.255.255 for any IP
    # In production, this would calculate based on subnet
    return "255.255.255.255"


def create_schema() -> Schema:
    """Create the GraphQL schema with Query and Mutation."""
    return Schema(query=Query, mutation=Mutation, extensions=[_AuthExtension])


class _AuthExtension(Extension):
    """Custom extension to handle authentication context."""

    def __init__(self):
        self._info = None

    def request_started(self, info: Info) -> None:
        """Store context for authentication checking."""
        self._info = info

    def resolve(self, next_resolve: Any, info: Info) -> Any:
        """Resolve with auth context available."""
        return next_resolve(info)
