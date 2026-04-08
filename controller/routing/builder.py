# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
GraphBuilder — populates the RoutingGraph from AppState.

Phase 1: observational, read-only population from existing discovery.
No routing decisions are made. All link metrics are `spec` or `assumed`.

Devices created per NodeInfo:
  - One `node` device (the Ozma hardware/soft node itself)
  - One `target` device (the machine the node is wired to)
  - Links: HID (UDP), audio (PipeWire or VBAN), video (capture stream)

The controller itself is added as a `controller` device.

IDs are stable across restarts — they match the mDNS instance name for nodes
(e.g. "vm1._ozma._udp.local.") and "controller" for the controller.
"""

from __future__ import annotations

import logging
import socket

from .graph import RoutingGraph
from .model import (
    ActivationTimeSpec,
    BandwidthSpec,
    Device,
    DeviceType,
    InfoQuality,
    LatencySpec,
    Link,
    LinkState,
    LinkStatus,
    Location,
    LossSpec,
    MediaType,
    Port,
    PortDirection,
    PortRef,
    PortState,
)

log = logging.getLogger("ozma.routing.builder")

# Typical UDP HID link metrics (assumed, will be replaced by measured in Phase 5)
_HID_LATENCY = LatencySpec(min_ms=0.5, typical_ms=1.0, max_ms=5.0, quality=InfoQuality.spec)
_HID_BANDWIDTH = BandwidthSpec(
    capacity_bps=1_000_000, available_bps=990_000, used_bps=10_000,
    quality=InfoQuality.spec,
)
_HID_LOSS = LossSpec(rate=0.0001, window_seconds=10, quality=InfoQuality.spec)
_HID_ACTIVATION = ActivationTimeSpec(
    cold_to_warm_ms=0, warm_to_active_ms=0,
    active_to_warm_ms=0, warm_to_standby_ms=0,
    quality=InfoQuality.spec,
)

# VBAN audio link metrics
_VBAN_LATENCY = LatencySpec(min_ms=2.0, typical_ms=5.0, max_ms=20.0, quality=InfoQuality.spec)
_VBAN_BANDWIDTH = BandwidthSpec(
    capacity_bps=10_000_000, available_bps=9_000_000, used_bps=192_000,
    quality=InfoQuality.spec,
)
_VBAN_ACTIVATION = ActivationTimeSpec(
    cold_to_warm_ms=100, warm_to_active_ms=10,
    active_to_warm_ms=10, warm_to_standby_ms=100,
    quality=InfoQuality.spec,
)

# PipeWire audio link metrics (same-machine, essentially zero latency)
_PW_LATENCY = LatencySpec(min_ms=0.1, typical_ms=0.5, max_ms=2.0, quality=InfoQuality.spec)
_PW_BANDWIDTH = BandwidthSpec(
    capacity_bps=100_000_000, available_bps=99_000_000, used_bps=192_000,
    quality=InfoQuality.spec,
)

# HLS/MJPEG video stream metrics
_VIDEO_LATENCY = LatencySpec(min_ms=50, typical_ms=200, max_ms=2000, quality=InfoQuality.spec)
_VIDEO_BANDWIDTH = BandwidthSpec(
    capacity_bps=100_000_000, available_bps=90_000_000, used_bps=4_000_000,
    quality=InfoQuality.spec,
)
_VIDEO_ACTIVATION = ActivationTimeSpec(
    cold_to_warm_ms=500, warm_to_active_ms=100,
    active_to_warm_ms=100, warm_to_standby_ms=200,
    quality=InfoQuality.spec,
)


class GraphBuilder:
    """
    Builds (or rebuilds) the RoutingGraph from current AppState.

    Call `rebuild(state)` whenever the node set changes. The graph is
    fully replaced — do not hold references to old Device/Port/Link objects
    across a rebuild.
    """

    def __init__(self, graph: RoutingGraph) -> None:
        self._graph = graph

    def rebuild(self, state: object) -> None:
        """
        Rebuild the entire graph from AppState.

        This clears the existing graph and repopulates it. Designed for
        Phase 1 where full rebuilds are cheap (small graphs).
        """
        self._graph.clear()
        self._add_controller(state)
        for node_info in state.nodes.values():  # type: ignore[union-attr]
            self._add_node(node_info, state)
        log.debug("Graph rebuilt: %r", self._graph)

    def apply_node_added(self, node_info: object, state: object) -> None:
        """Incremental update: add or replace a single node."""
        self._remove_node_devices(node_info.id)  # type: ignore[union-attr]
        self._add_node(node_info, state)

    def apply_node_removed(self, node_id: str) -> None:
        """Incremental update: remove a node and its target device."""
        self._remove_node_devices(node_id)

    # ── Private ──────────────────────────────────────────────────────────────

    def _controller_device_id(self) -> str:
        return "controller"

    def _node_device_id(self, node_id: str) -> str:
        return f"node:{node_id}"

    def _target_device_id(self, node_id: str) -> str:
        return f"target:{node_id}"

    def _remove_node_devices(self, node_id: str) -> None:
        self._graph.remove_device(self._node_device_id(node_id))
        self._graph.remove_device(self._target_device_id(node_id))

    def _add_controller(self, state: object) -> None:
        try:
            hostname = socket.gethostname()
        except Exception:
            hostname = "controller"

        controller = Device(
            id=self._controller_device_id(),
            name=f"Ozma Controller ({hostname})",
            type=DeviceType.controller,
            location=Location(machine_id=hostname),
            ports=[
                # HID source — sends HID packets to the active node
                Port(
                    id="hid_out",
                    device_id=self._controller_device_id(),
                    direction=PortDirection.source,
                    media_type=MediaType.hid,
                    label="HID out (UDP to active node)",
                ),
                # Control API — WebSocket/REST for scenario switching
                Port(
                    id="api",
                    device_id=self._controller_device_id(),
                    direction=PortDirection.source,
                    media_type=MediaType.control,
                    label="REST/WebSocket API",
                    properties={"port": 7380},
                ),
            ],
            properties={"active_node_id": getattr(state, "active_node_id", None)},
        )
        self._graph.add_device(controller)

    def _add_node(self, node_info: object, state: object) -> None:
        """Add node device + target device + links for one NodeInfo."""
        nid: str = node_info.id  # type: ignore[union-attr]
        host: str = node_info.host  # type: ignore[union-attr]
        node_dev_id = self._node_device_id(nid)
        target_dev_id = self._target_device_id(nid)

        # ── Node device ───────────────────────────────────────────────────
        node_ports: list[Port] = [
            # Receives HID from controller
            Port(
                id="hid_in",
                device_id=node_dev_id,
                direction=PortDirection.sink,
                media_type=MediaType.hid,
                label="HID in (UDP from controller)",
                properties={"udp_port": node_info.port},  # type: ignore[union-attr]
            ),
            # Sends HID to target via USB gadget
            Port(
                id="hid_usb_out",
                device_id=node_dev_id,
                direction=PortDirection.source,
                media_type=MediaType.hid,
                label="HID out (USB gadget to target)",
            ),
        ]

        # Video: does this node have a capture source?
        has_video = bool(
            getattr(node_info, "stream_port", None)
            or getattr(node_info, "capture_device", None)
            or getattr(node_info, "vnc_host", None)
            or getattr(node_info, "display_outputs", [])
        )
        if has_video:
            node_ports.append(Port(
                id="video_out",
                device_id=node_dev_id,
                direction=PortDirection.source,
                media_type=MediaType.video,
                label="Video out (HLS/MJPEG stream)",
                properties=self._video_props(node_info),
            ))

        # Audio
        audio_type = getattr(node_info, "audio_type", None)
        if audio_type == "pipewire":
            node_ports.append(Port(
                id="audio_pw_in",
                device_id=node_dev_id,
                direction=PortDirection.sink,
                media_type=MediaType.audio,
                label="Audio in (PipeWire sink)",
                properties={"sink_name": node_info.audio_sink},  # type: ignore[union-attr]
            ))
        elif audio_type == "vban":
            node_ports.append(Port(
                id="audio_vban_in",
                device_id=node_dev_id,
                direction=PortDirection.sink,
                media_type=MediaType.audio,
                label="Audio in (VBAN UDP)",
                properties={"vban_port": getattr(node_info, "audio_vban_port", None)},
            ))
            mic_port = getattr(node_info, "mic_vban_port", None)
            if mic_port:
                node_ports.append(Port(
                    id="mic_vban_out",
                    device_id=node_dev_id,
                    direction=PortDirection.source,
                    media_type=MediaType.audio,
                    label="Mic out (VBAN UDP)",
                    properties={"vban_port": mic_port},
                ))

        node_device = Device(
            id=node_dev_id,
            name=nid,
            type=DeviceType.node,
            location=Location(
                machine_id=nid,
                overlay_ip=host,
                bus="network",
            ),
            ports=node_ports,
            properties={
                "role": getattr(node_info, "role", ""),
                "hw": getattr(node_info, "hw", ""),
                "fw_version": getattr(node_info, "fw_version", ""),
                "machine_class": getattr(node_info, "machine_class", "workstation"),
                "capabilities": getattr(node_info, "capabilities", []),
            },
        )
        self._graph.add_device(node_device)

        # ── Target device ─────────────────────────────────────────────────
        target_ports: list[Port] = [
            Port(
                id="hid_usb_in",
                device_id=target_dev_id,
                direction=PortDirection.sink,
                media_type=MediaType.hid,
                label="HID in (USB from node)",
            ),
        ]
        if has_video:
            target_ports.append(Port(
                id="video_out",
                device_id=target_dev_id,
                direction=PortDirection.source,
                media_type=MediaType.video,
                label="Video out (HDMI/Display to node capture)",
            ))
        if audio_type:
            target_ports.append(Port(
                id="audio_out",
                device_id=target_dev_id,
                direction=PortDirection.source,
                media_type=MediaType.audio,
                label="Audio out (to node)",
            ))

        target_device = Device(
            id=target_dev_id,
            name=f"{nid} target",
            type=DeviceType.target,
            location=Location(machine_id=nid),
            ports=target_ports,
            properties={
                "machine_class": getattr(node_info, "machine_class", "workstation"),
            },
        )
        self._graph.add_device(target_device)

        # ── Links ─────────────────────────────────────────────────────────
        ctrl_id = self._controller_device_id()
        active_node = getattr(state, "active_node_id", None)
        is_active = (nid == active_node)

        # Controller HID → Node HID
        hid_link = Link(
            id=f"hid:{ctrl_id}→{node_dev_id}",
            source=PortRef(ctrl_id, "hid_out"),
            sink=PortRef(node_dev_id, "hid_in"),
            transport="udp_hid",
            state=LinkState(
                status=LinkStatus.active if is_active else LinkStatus.standby,
                bandwidth=_HID_BANDWIDTH,
                latency=_HID_LATENCY,
                loss=_HID_LOSS,
                activation_time=_HID_ACTIVATION,
            ),
            properties={"target_ip": host, "target_port": node_info.port},  # type: ignore[union-attr]
        )
        self._graph.add_link(hid_link)

        # Node USB gadget → Target USB
        usb_link = Link(
            id=f"hid:{node_dev_id}→{target_dev_id}",
            source=PortRef(node_dev_id, "hid_usb_out"),
            sink=PortRef(target_dev_id, "hid_usb_in"),
            transport="usb_hid_gadget",
            state=LinkState(
                status=LinkStatus.active,  # always wired
                latency=LatencySpec(min_ms=0.1, typical_ms=0.3, max_ms=1.0,
                                    quality=InfoQuality.spec),
                bandwidth=BandwidthSpec(capacity_bps=12_000_000,
                                        available_bps=11_000_000, used_bps=10_000,
                                        quality=InfoQuality.spec),
                activation_time=_HID_ACTIVATION,
            ),
        )
        self._graph.add_link(usb_link)

        # Audio links
        if audio_type == "pipewire":
            pw_link = Link(
                id=f"audio:pw:{ctrl_id}→{node_dev_id}",
                source=PortRef(ctrl_id, "hid_out"),  # controller runs PW routing
                sink=PortRef(node_dev_id, "audio_pw_in"),
                transport="pipewire",
                state=LinkState(
                    status=LinkStatus.active if is_active else LinkStatus.warm,
                    latency=_PW_LATENCY,
                    bandwidth=_PW_BANDWIDTH,
                ),
                properties={"sink_name": node_info.audio_sink},  # type: ignore[union-attr]
            )
            self._graph.add_link(pw_link)
        elif audio_type == "vban":
            vban_link = Link(
                id=f"audio:vban:{ctrl_id}→{node_dev_id}",
                source=PortRef(ctrl_id, "hid_out"),
                sink=PortRef(node_dev_id, "audio_vban_in"),
                transport="vban",
                state=LinkState(
                    status=LinkStatus.active if is_active else LinkStatus.standby,
                    latency=_VBAN_LATENCY,
                    bandwidth=_VBAN_BANDWIDTH,
                    activation_time=_VBAN_ACTIVATION,
                ),
                properties={
                    "target_ip": host,
                    "vban_port": getattr(node_info, "audio_vban_port", None),
                },
            )
            self._graph.add_link(vban_link)

        # Video link (node → controller, stream pull)
        if has_video:
            video_link = Link(
                id=f"video:{node_dev_id}→{ctrl_id}",
                source=PortRef(node_dev_id, "video_out"),
                sink=PortRef(ctrl_id, "api"),  # controller receives/proxies the stream
                transport="hls_mjpeg",
                state=LinkState(
                    status=LinkStatus.warm if is_active else LinkStatus.standby,
                    latency=_VIDEO_LATENCY,
                    bandwidth=_VIDEO_BANDWIDTH,
                    activation_time=_VIDEO_ACTIVATION,
                ),
                properties=self._video_props(node_info),
            )
            self._graph.add_link(video_link)

    @staticmethod
    def _video_props(node_info: object) -> dict:
        props: dict = {}
        stream_port = getattr(node_info, "stream_port", None)
        stream_path = getattr(node_info, "stream_path", None)
        vnc_host = getattr(node_info, "vnc_host", None)
        vnc_port = getattr(node_info, "vnc_port", None)
        capture_device = getattr(node_info, "capture_device", None)
        if stream_port:
            props["stream_port"] = stream_port
        if stream_path:
            props["stream_path"] = stream_path
        if vnc_host:
            props["vnc_host"] = vnc_host
        if vnc_port:
            props["vnc_port"] = vnc_port
        if capture_device:
            props["capture_device"] = capture_device
        return props
