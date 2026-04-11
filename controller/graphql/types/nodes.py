# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
GraphQL types for Node-related data structures.
"""

from datetime import datetime
from enum import Enum
from typing import Optional

import strawberry
from strawberry import types as strawberry_types

from controller.state import NodeInfo


@strawberry.type
class DisplayOutputType:
    """Represents a display output from a node."""

    index: int
    source_type: str
    capture_source_id: str
    width: int
    height: int


@strawberry.type
class CameraStreamType:
    """Represents a camera stream from a node."""

    name: str
    rtsp_inbound: str
    backchannel: str
    hls: str


class MachineClassEnum(str, Enum):
    """Machine class enumeration."""

    WORKSTATION = "workstation"
    SERVER = "server"
    KIOSK = "kiosk"
    CAMERA = "camera"


MachineClass = strawberry.enum(MachineClassEnum)


@strawberry.type
class NodeType:
    """Represents a hardware or virtual node in the KVM network."""

    id: str
    host: str
    port: int
    role: str
    hw: str
    fw_version: str
    proto_version: int
    capabilities: list[str]
    machine_class: MachineClassEnum
    last_seen: datetime
    display_outputs: list[DisplayOutputType]
    vnc_host: Optional[str] = None
    vnc_port: Optional[int] = None
    stream_port: Optional[int] = None
    stream_path: Optional[str] = None
    audio_type: Optional[str] = None
    audio_sink: Optional[str] = None
    audio_vban_port: Optional[int] = None
    mic_vban_port: Optional[int] = None
    capture_device: Optional[str] = None
    camera_streams: list[CameraStreamType] = strawberry.field(default_factory=list)
    frigate_host: Optional[str] = None
    frigate_port: Optional[int] = None
    owner_user_id: str = ""
    owner_id: str = ""
    shared_with: list[str] = strawberry.field(default_factory=list)
    share_permissions: dict[str, str] = strawberry.field(default_factory=dict)
    parent_node_id: str = ""
    sunshine_port: Optional[int] = None
    seat_count: int = 1
    seat_config: dict = strawberry.field(default_factory=dict)

    @classmethod
    def from_nodeinfo(cls, node: NodeInfo) -> "NodeType":
        """Create a NodeType from a NodeInfo dataclass."""
        return cls(
            id=node.id,
            host=node.host,
            port=node.port,
            role=node.role,
            hw=node.hw,
            fw_version=node.fw_version,
            proto_version=node.proto_version,
            capabilities=node.capabilities,
            machine_class=MachineClass(node.machine_class),
            last_seen=datetime.fromtimestamp(node.last_seen)
            if node.last_seen
            else datetime.now(),
            display_outputs=[
                DisplayOutputType(
                    index=do["index"],
                    source_type=do["source_type"],
                    capture_source_id=do["capture_source_id"],
                    width=do["width"],
                    height=do["height"],
                )
                for do in node.display_outputs
            ],
            vnc_host=node.vnc_host,
            vnc_port=node.vnc_port,
            stream_port=node.stream_port,
            stream_path=node.stream_path,
            audio_type=node.audio_type,
            audio_sink=node.audio_sink,
            audio_vban_port=node.audio_vban_port,
            mic_vban_port=node.mic_vban_port,
            capture_device=node.capture_device,
            camera_streams=[
                CameraStreamType(
                    name=cs["name"],
                    rtsp_inbound=cs["rtsp_inbound"],
                    backchannel=cs["backchannel"],
                    hls=cs["hls"],
                )
                for cs in node.camera_streams
            ],
            frigate_host=node.frigate_host,
            frigate_port=node.frigate_port,
            owner_user_id=node.owner_user_id,
            owner_id=node.owner_id,
            shared_with=node.shared_with,
            share_permissions=node.share_permissions,
            parent_node_id=node.parent_node_id,
            sunshine_port=node.sunshine_port,
            seat_count=node.seat_count,
            seat_config=node.seat_config,
        )


# Type alias for NodeInfo (exposed for schema compatibility)
NodeInfoType = NodeType
