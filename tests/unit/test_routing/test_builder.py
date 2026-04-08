# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Unit tests for the GraphBuilder."""

from __future__ import annotations

import sys
from pathlib import Path
from dataclasses import dataclass, field

sys.path.insert(0, str(Path(__file__).parent.parent.parent.parent / "controller"))

import pytest
from routing.graph import RoutingGraph
from routing.builder import GraphBuilder
from routing.model import DeviceType, MediaType, LinkStatus


# ── Minimal AppState stub ─────────────────────────────────────────────────────

@dataclass
class StubNodeInfo:
    id: str
    host: str = "192.168.1.10"
    port: int = 7331
    role: str = "compute"
    hw: str = "rpi4"
    fw_version: str = "0.1.0"
    proto_version: int = 1
    capabilities: list = field(default_factory=list)
    machine_class: str = "workstation"
    audio_type: str | None = None
    audio_sink: str | None = None
    audio_vban_port: int | None = None
    mic_vban_port: int | None = None
    stream_port: int | None = None
    stream_path: str | None = None
    vnc_host: str | None = None
    vnc_port: int | None = None
    capture_device: str | None = None
    display_outputs: list = field(default_factory=list)


class StubAppState:
    def __init__(self, nodes: dict | None = None, active_node_id: str | None = None):
        self.nodes = nodes or {}
        self.active_node_id = active_node_id


# ── GraphBuilder tests ────────────────────────────────────────────────────────

class TestGraphBuilderBasic:
    def test_rebuild_empty(self):
        g = RoutingGraph()
        b = GraphBuilder(g)
        state = StubAppState()
        b.rebuild(state)
        # Controller device should be present
        assert g.get_device("controller") is not None
        assert g.device_count == 1

    def test_rebuild_single_node(self):
        g = RoutingGraph()
        b = GraphBuilder(g)
        node = StubNodeInfo(id="vm1._ozma._udp.local.")
        state = StubAppState(nodes={"vm1._ozma._udp.local.": node})
        b.rebuild(state)
        # controller + node + target = 3
        assert g.device_count == 3
        node_dev = g.get_device("node:vm1._ozma._udp.local.")
        assert node_dev is not None
        assert node_dev.type == DeviceType.node
        target_dev = g.get_device("target:vm1._ozma._udp.local.")
        assert target_dev is not None
        assert target_dev.type == DeviceType.target

    def test_hid_link_present(self):
        g = RoutingGraph()
        b = GraphBuilder(g)
        node = StubNodeInfo(id="vm1")
        state = StubAppState(nodes={"vm1": node})
        b.rebuild(state)
        # Should have controller→node HID link and node→target USB link
        links = list(g.links())
        transports = {l.transport for l in links}
        assert "udp_hid" in transports
        assert "usb_hid_gadget" in transports

    def test_active_node_link_status(self):
        g = RoutingGraph()
        b = GraphBuilder(g)
        node = StubNodeInfo(id="vm1")
        state = StubAppState(nodes={"vm1": node}, active_node_id="vm1")
        b.rebuild(state)
        hid_links = [l for l in g.links() if l.transport == "udp_hid"]
        assert len(hid_links) == 1
        assert hid_links[0].state.status == LinkStatus.active

    def test_inactive_node_link_status(self):
        g = RoutingGraph()
        b = GraphBuilder(g)
        node = StubNodeInfo(id="vm1")
        state = StubAppState(nodes={"vm1": node}, active_node_id="vm2")
        b.rebuild(state)
        hid_links = [l for l in g.links() if l.transport == "udp_hid"]
        assert hid_links[0].state.status == LinkStatus.standby

    def test_two_nodes(self):
        g = RoutingGraph()
        b = GraphBuilder(g)
        state = StubAppState(nodes={
            "vm1": StubNodeInfo(id="vm1"),
            "vm2": StubNodeInfo(id="vm2"),
        })
        b.rebuild(state)
        # controller + 2×(node+target) = 5
        assert g.device_count == 5

    def test_controller_device_type(self):
        g = RoutingGraph()
        b = GraphBuilder(g)
        b.rebuild(StubAppState())
        ctrl = g.get_device("controller")
        assert ctrl.type == DeviceType.controller

    def test_controller_has_hid_source_port(self):
        g = RoutingGraph()
        b = GraphBuilder(g)
        b.rebuild(StubAppState())
        ctrl = g.get_device("controller")
        hid_ports = ctrl.ports_by_media(MediaType.hid)
        assert len(hid_ports) == 1

    def test_controller_has_audio_source_port(self):
        g = RoutingGraph()
        b = GraphBuilder(g)
        b.rebuild(StubAppState())
        ctrl = g.get_device("controller")
        audio_ports = ctrl.ports_by_media(MediaType.audio)
        assert len(audio_ports) == 1
        assert audio_ports[0].id == "audio_out"

    def test_audio_links_use_audio_out_not_hid_out(self):
        """Audio links must originate from controller's audio_out port, not hid_out."""
        g = RoutingGraph()
        b = GraphBuilder(g)
        node = StubNodeInfo(id="vm1", audio_type="pipewire", audio_sink="ozma-vm1")
        state = StubAppState(nodes={"vm1": node})
        b.rebuild(state)
        audio_links = [l for l in g.links() if l.transport in ("pipewire", "vban")]
        assert len(audio_links) == 1
        assert audio_links[0].source.port_id == "audio_out"
        assert audio_links[0].source.device_id == "controller"


class TestGraphBuilderAudio:
    def test_pipewire_audio_adds_port(self):
        g = RoutingGraph()
        b = GraphBuilder(g)
        node = StubNodeInfo(id="vm1", audio_type="pipewire", audio_sink="ozma-vm1")
        state = StubAppState(nodes={"vm1": node})
        b.rebuild(state)
        node_dev = g.get_device("node:vm1")
        audio_ports = node_dev.ports_by_media(MediaType.audio)
        assert len(audio_ports) == 1
        assert audio_ports[0].id == "audio_pw_in"

    def test_vban_audio_adds_port(self):
        g = RoutingGraph()
        b = GraphBuilder(g)
        node = StubNodeInfo(id="vm1", audio_type="vban", audio_vban_port=6980)
        state = StubAppState(nodes={"vm1": node})
        b.rebuild(state)
        node_dev = g.get_device("node:vm1")
        audio_ports = node_dev.ports_by_media(MediaType.audio)
        assert any(p.id == "audio_vban_in" for p in audio_ports)

    def test_vban_with_mic_adds_mic_port(self):
        g = RoutingGraph()
        b = GraphBuilder(g)
        node = StubNodeInfo(id="vm1", audio_type="vban",
                            audio_vban_port=6980, mic_vban_port=6981)
        state = StubAppState(nodes={"vm1": node})
        b.rebuild(state)
        node_dev = g.get_device("node:vm1")
        audio_ports = node_dev.ports_by_media(MediaType.audio)
        port_ids = {p.id for p in audio_ports}
        assert "audio_vban_in" in port_ids
        assert "mic_vban_out" in port_ids

    def test_no_audio_no_audio_links(self):
        g = RoutingGraph()
        b = GraphBuilder(g)
        node = StubNodeInfo(id="vm1")  # no audio_type
        state = StubAppState(nodes={"vm1": node})
        b.rebuild(state)
        audio_links = [l for l in g.links()
                       if l.transport in ("pipewire", "vban")]
        assert len(audio_links) == 0


class TestGraphBuilderVideo:
    def test_stream_port_adds_video_port(self):
        g = RoutingGraph()
        b = GraphBuilder(g)
        node = StubNodeInfo(id="vm1", stream_port=7382, stream_path="/stream/stream.m3u8")
        state = StubAppState(nodes={"vm1": node})
        b.rebuild(state)
        node_dev = g.get_device("node:vm1")
        video_ports = node_dev.ports_by_media(MediaType.video)
        assert len(video_ports) == 1

    def test_vnc_host_adds_video_port(self):
        g = RoutingGraph()
        b = GraphBuilder(g)
        node = StubNodeInfo(id="vm1", vnc_host="127.0.0.1", vnc_port=5901)
        state = StubAppState(nodes={"vm1": node})
        b.rebuild(state)
        node_dev = g.get_device("node:vm1")
        video_ports = node_dev.ports_by_media(MediaType.video)
        assert len(video_ports) == 1

    def test_no_video_no_video_port(self):
        g = RoutingGraph()
        b = GraphBuilder(g)
        node = StubNodeInfo(id="vm1")  # no video
        state = StubAppState(nodes={"vm1": node})
        b.rebuild(state)
        node_dev = g.get_device("node:vm1")
        video_ports = node_dev.ports_by_media(MediaType.video)
        assert len(video_ports) == 0


class TestGraphBuilderIncremental:
    def test_apply_node_added(self):
        g = RoutingGraph()
        b = GraphBuilder(g)
        b.rebuild(StubAppState())  # just controller
        assert g.device_count == 1

        node = StubNodeInfo(id="vm1")
        state = StubAppState(nodes={"vm1": node})
        b.apply_node_added(node, state)
        assert g.device_count == 3  # controller + node + target

    def test_apply_node_removed(self):
        g = RoutingGraph()
        b = GraphBuilder(g)
        node = StubNodeInfo(id="vm1")
        state = StubAppState(nodes={"vm1": node})
        b.rebuild(state)
        assert g.device_count == 3

        b.apply_node_removed("vm1")
        assert g.device_count == 1  # only controller

    def test_apply_node_added_replaces_existing(self):
        g = RoutingGraph()
        b = GraphBuilder(g)
        node = StubNodeInfo(id="vm1")
        state = StubAppState(nodes={"vm1": node})
        b.rebuild(state)

        # Re-add with updated properties
        node2 = StubNodeInfo(id="vm1", audio_type="pipewire", audio_sink="ozma-vm1")
        b.apply_node_added(node2, state)
        node_dev = g.get_device("node:vm1")
        assert len(node_dev.ports_by_media(MediaType.audio)) == 1
