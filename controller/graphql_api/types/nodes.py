# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""GraphQL types for Node representation."""

from datetime import datetime
from typing import Any

import strawberry
from strawberry import ID

from ..state import NodeInfo


@strawberry.type
class DisplayOutputType:
    """Represents a display output on a node."""

    index: int
    source_type: str
    capture_source_id: str
    width: int
    height: int


@strawberry.type
class CameraStreamType:
    """Represents a camera stream."""

    name: str
    rtsp_inbound: str
    backchannel: str
    hls: str


@strawberry.type
class NodeType:
    """Represents a hardware or virtual node in the KVM network."""

    id: ID
    name: str | None
    host: str
    port: int
    role: str
    hw: str
    fw_version: str
    proto_version: int
    capabilities: list[str]
    machine_class: str
    last_seen: datetime
    display_outputs: list[DisplayOutputType]
    vnc_host: str | None
    vnc_port: int | None
    stream_port: int | None
    stream_path: str | None
    api_port: int | None
    audio_type: str | None
    audio_sink: str | None
    audio_vban_port: int | None
    mic_vban_port: int | None
    capture_device: str | None
    camera_streams: list[CameraStreamType]
    frigate_host: str | None
    frigate_port: int | None
    owner_user_id: str
    owner_id: str
    shared_with: list[str]
    share_permissions: list[str]
    parent_node_id: str
    sunshine_port: int | None

    @classmethod
    def from_nodeinfo(cls, node: NodeInfo) -> "NodeType":
        """Create a NodeType from a NodeInfo instance."""
        display_outputs = [
            DisplayOutputType(
                index=do.get("index", 0),
                source_type=do.get("source_type", "unknown"),
                capture_source_id=do.get("capture_source_id", ""),
                width=do.get("width", 0),
                height=do.get("height", 0),
            )
            for do in node.display_outputs
        ]

        camera_streams = [
            CameraStreamType(
                name=cs.get("name", ""),
                rtsp_inbound=cs.get("rtsp_inbound", ""),
                backchannel=cs.get("backchannel", ""),
                hls=cs.get("hls", ""),
            )
            for cs in node.camera_streams
        ]

        return cls(
            id=strawberry.ID(str(node.id)),
            name=None,  # NodeInfo doesn't have a name field yet
            host=node.host,
            port=node.port,
            role=node.role,
            hw=node.hw,
            fw_version=node.fw_version,
            proto_version=node.proto_version,
            capabilities=node.capabilities,
            machine_class=node.machine_class,
            last_seen=datetime.fromtimestamp(node.last_seen),
            display_outputs=display_outputs,
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
            camera_streams=camera_streams,
            frigate_host=node.frigate_host,
            frigate_port=node.frigate_port,
            owner_user_id=node.owner_user_id,
            owner_id=node.owner_id,
            shared_with=node.shared_with,
            share_permissions=list(node.share_permissions.values()),
            parent_node_id=node.parent_node_id,
            sunshine_port=node.sunshine_port,
        )


@strawberry.type
class NodeInfoType:
    """Detailed node information type."""

    node_id: str
    host: str
    port: int
    role: str
    hw: str
    fw_version: str
    proto_version: int
    capabilities: list[str]
    machine_class: str
    last_seen: datetime

    @classmethod
    def from_nodeinfo(cls, node: NodeInfo) -> "NodeInfoType":
        """Create a NodeInfoType from a NodeInfo instance."""
        return cls(
            node_id=node.id,
            host=node.host,
            port=node.port,
            role=node.role,
            hw=node.hw,
            fw_version=node.fw_version,
            proto_version=node.proto_version,
            capabilities=node.capabilities,
            machine_class=node.machine_class,
            last_seen=datetime.fromtimestamp(node.last_seen),
        )
