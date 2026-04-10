# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
GraphQL type definitions for Ozma Controller.

This module defines Strawberry GraphQL types that correspond to internal
data structures used in the controller.

NOTE: This module should not import from state.py to avoid circular imports.
The conversion from internal types to GraphQL types is done in subscriptions.py
or at the schema level where both modules can be imported.
"""

import strawberry
from typing import Optional
from dataclasses import dataclass
from datetime import datetime


@strawberry.type
class NodeType:
    """GraphQL type for a node in the KVMA system."""
    id: str
    host: str
    port: int
    role: str
    hw: str
    fw_version: str
    proto_version: int
    capabilities: list[str]
    machine_class: str
    last_seen: float
    vnc_host: Optional[str] = None
    vnc_port: Optional[int] = None
    stream_port: Optional[int] = None
    stream_path: Optional[str] = None
    api_port: Optional[int] = None
    audio_type: Optional[str] = None
    audio_sink: Optional[str] = None
    audio_vban_port: Optional[int] = None
    mic_vban_port: Optional[int] = None
    capture_device: Optional[str] = None
    camera_streams: list[dict] = strawberry.field(default_factory=list)
    frigate_host: Optional[str] = None
    frigate_port: Optional[int] = None
    owner_user_id: str = ""
    owner_id: str = ""
    shared_with: list[str] = strawberry.field(default_factory=list)
    parent_node_id: str = ""
    sunshine_port: Optional[int] = None

    @staticmethod
    def from_node(node: "NodeInfo") -> "NodeType":
        """Convert internal NodeInfo to GraphQL NodeType."""
        return NodeType(
            id=node.id,
            host=node.host,
            port=node.port,
            role=node.role,
            hw=node.hw,
            fw_version=node.fw_version,
            proto_version=node.proto_version,
            capabilities=node.capabilities,
            machine_class=node.machine_class,
            last_seen=node.last_seen,
            vnc_host=node.vnc_host,
            vnc_port=node.vnc_port,
            stream_port=node.stream_port,
            stream_path=node.stream_path,
            api_port=node.api_port,
            audio_type=node.audio_type,
            audio_sink=node.audio_sink,
            audio_vban_port=node.audio_vban_port,
            mic_vban_port=node.mic_vban_port,
            capture_device=node.capture_device,
            camera_streams=node.camera_streams,
            frigate_host=node.frigate_host,
            frigate_port=node.frigate_port,
            owner_user_id=node.owner_user_id,
            owner_id=node.owner_id,
            shared_with=node.shared_with,
            parent_node_id=node.parent_node_id,
            sunshine_port=node.sunshine_port,
        )


@strawberry.type
class ScenarioType:
    """GraphQL type for a scenario."""
    id: str
    name: str
    node_id: Optional[str] = None
    created_at: Optional[float] = None
    updated_at: Optional[float] = None
    active: bool = True
    config: dict = strawberry.field(default_factory=dict)

    @staticmethod
    def from_scenario(scenario: "Scenario") -> "ScenarioType":
        """Convert internal Scenario to GraphQL ScenarioType."""
        return ScenarioType(
            id=scenario.id,
            name=scenario.name,
            node_id=scenario.node_id,
            created_at=scenario.created_at,
            updated_at=scenario.updated_at,
            active=scenario.active,
            config=scenario.config,
        )


@strawberry.type
class AlertType:
    """GraphQL type for an alert."""
    id: str
    type: str
    device_id: str
    message: str
    severity: str
    timestamp: float
    source: str
    acknowledged: bool = False
    resolved: bool = False
    data: dict = strawberry.field(default_factory=dict)

    @staticmethod
    def from_alert(alert: "Alert") -> "AlertType":
        """Convert internal alert to GraphQL AlertType."""
        return AlertType(
            id=alert.id,
            type=alert.type,
            device_id=alert.device_id,
            message=alert.message,
            severity=alert.severity,
            timestamp=alert.timestamp,
            source=alert.source,
            acknowledged=alert.acknowledged,
            resolved=alert.resolved,
            data=alert.data,
        )


@strawberry.type
class AudioLevelType:
    """GraphQL type for audio level data for a single node."""
    node_id: str
    levels: dict[str, float] = strawberry.field(
        description="Channel -> dB levels mapping"
    )
    timestamp: float

    @staticmethod
    def from_audio_data(node_id: str, levels: dict[str, float], timestamp: float) -> "AudioLevelType":
        """Create AudioLevelType from raw data."""
        return AudioLevelType(
            node_id=node_id,
            levels=levels,
            timestamp=timestamp,
        )


@strawberry.type
class SnapshotType:
    """GraphQL type for system snapshot."""
    nodes: list[NodeType]
    active_node_id: Optional[str] = None
