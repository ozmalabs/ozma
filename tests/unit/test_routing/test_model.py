# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Unit tests for the routing graph model (Phase 1)."""

from __future__ import annotations

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent.parent.parent.parent / "controller"))

import pytest
from routing.model import (
    InfoQuality,
    BandwidthSpec,
    LatencySpec,
    JitterSpec,
    LossSpec,
    ActivationTimeSpec,
    Location,
    PhysicalLocation,
    HardwareIdentity,
    PortRef,
    PortState,
    Port,
    PortDirection,
    MediaType,
    Link,
    LinkState,
    LinkStatus,
    Device,
    DeviceType,
)
from routing.graph import RoutingGraph


# ── InfoQuality ───────────────────────────────────────────────────────────────

class TestInfoQuality:
    def test_trust_ordering(self):
        assert InfoQuality.user > InfoQuality.measured
        assert InfoQuality.measured > InfoQuality.inferred
        assert InfoQuality.inferred > InfoQuality.reported
        assert InfoQuality.reported > InfoQuality.commanded
        assert InfoQuality.commanded > InfoQuality.spec
        assert InfoQuality.spec > InfoQuality.assumed

    def test_ge(self):
        assert InfoQuality.user >= InfoQuality.user
        assert InfoQuality.measured >= InfoQuality.spec

    def test_lt(self):
        assert InfoQuality.assumed < InfoQuality.spec
        assert InfoQuality.spec < InfoQuality.user

    def test_le(self):
        assert InfoQuality.assumed <= InfoQuality.assumed
        assert InfoQuality.spec <= InfoQuality.measured

    def test_all_values_serialise(self):
        for q in InfoQuality:
            assert isinstance(q.value, str)


# ── Measurement specs ─────────────────────────────────────────────────────────

class TestMeasurementSpecs:
    def test_bandwidth_to_dict(self):
        b = BandwidthSpec(1_000_000, 900_000, 50_000, InfoQuality.spec)
        d = b.to_dict()
        assert d["capacity_bps"] == 1_000_000
        assert d["quality"] == "spec"

    def test_latency_to_dict(self):
        lat = LatencySpec(0.5, 1.0, 5.0, InfoQuality.assumed)
        d = lat.to_dict()
        assert d["min_ms"] == 0.5
        assert d["typical_ms"] == 1.0
        assert d["max_ms"] == 5.0
        assert d["quality"] == "assumed"

    def test_loss_rate_valid(self):
        loss = LossSpec(0.001, 10, InfoQuality.spec)
        assert 0.0 <= loss.rate <= 1.0

    def test_activation_time_to_dict(self):
        at = ActivationTimeSpec(100, 10, 10, 50, InfoQuality.spec)
        d = at.to_dict()
        assert d["cold_to_warm_ms"] == 100
        assert d["warm_to_active_ms"] == 10


# ── Location ──────────────────────────────────────────────────────────────────

class TestLocation:
    def test_empty_location(self):
        loc = Location()
        d = loc.to_dict()
        assert d == {}

    def test_with_machine_id(self):
        loc = Location(machine_id="vm1", bus="network", overlay_ip="10.200.0.5")
        d = loc.to_dict()
        assert d["machine_id"] == "vm1"
        assert d["bus"] == "network"
        assert d["overlay_ip"] == "10.200.0.5"

    def test_physical_location(self):
        phys = PhysicalLocation(site="home", space="study", quality=InfoQuality.user)
        d = phys.to_dict()
        assert d["site"] == "home"
        assert d["space"] == "study"
        assert d["quality"] == "user"

    def test_physical_omits_none_fields(self):
        phys = PhysicalLocation(quality=InfoQuality.assumed)
        d = phys.to_dict()
        assert "site" not in d
        assert "space" not in d


# ── Port ──────────────────────────────────────────────────────────────────────

class TestPort:
    def test_port_serialises(self):
        port = Port(
            id="hid_in",
            device_id="node:vm1",
            direction=PortDirection.sink,
            media_type=MediaType.hid,
            label="HID in",
        )
        d = port.to_dict()
        assert d["id"] == "hid_in"
        assert d["direction"] == "sink"
        assert d["media_type"] == "hid"
        assert d["label"] == "HID in"

    def test_port_state_connections(self):
        state = PortState()
        ref = PortRef("other_dev", "other_port")
        state.connected_to.append(ref)
        d = state.to_dict()
        assert len(d["connected_to"]) == 1
        assert d["connected_to"][0]["device_id"] == "other_dev"


# ── PortRef ───────────────────────────────────────────────────────────────────

class TestPortRef:
    def test_equality(self):
        a = PortRef("dev1", "port1")
        b = PortRef("dev1", "port1")
        c = PortRef("dev1", "port2")
        assert a == b
        assert a != c

    def test_hashable(self):
        a = PortRef("dev1", "port1")
        b = PortRef("dev1", "port1")
        assert len({a, b}) == 1

    def test_to_dict(self):
        ref = PortRef("d", "p")
        assert ref.to_dict() == {"device_id": "d", "port_id": "p"}


# ── Device ────────────────────────────────────────────────────────────────────

class TestDevice:
    def _make_device(self) -> Device:
        return Device(
            id="node:vm1._ozma._udp.local.",
            name="vm1._ozma._udp.local.",
            type=DeviceType.node,
            location=Location(machine_id="vm1"),
            ports=[
                Port("hid_in", "node:vm1._ozma._udp.local.",
                     PortDirection.sink, MediaType.hid),
                Port("video_out", "node:vm1._ozma._udp.local.",
                     PortDirection.source, MediaType.video),
            ],
        )

    def test_get_port_found(self):
        dev = self._make_device()
        port = dev.get_port("hid_in")
        assert port is not None
        assert port.media_type == MediaType.hid

    def test_get_port_not_found(self):
        dev = self._make_device()
        assert dev.get_port("nonexistent") is None

    def test_ports_by_media(self):
        dev = self._make_device()
        hid_ports = dev.ports_by_media(MediaType.hid)
        assert len(hid_ports) == 1
        assert hid_ports[0].id == "hid_in"

    def test_to_dict(self):
        dev = self._make_device()
        d = dev.to_dict()
        assert d["id"] == "node:vm1._ozma._udp.local."
        assert d["type"] == "node"
        assert len(d["ports"]) == 2


# ── RoutingGraph ──────────────────────────────────────────────────────────────

class TestRoutingGraph:
    def _make_graph_with_two_nodes(self) -> RoutingGraph:
        g = RoutingGraph()
        ctrl = Device("controller", "Controller", DeviceType.controller,
                      ports=[Port("hid_out", "controller", PortDirection.source, MediaType.hid)])
        node = Device("node:vm1", "vm1", DeviceType.node,
                      ports=[Port("hid_in", "node:vm1", PortDirection.sink, MediaType.hid)])
        g.add_device(ctrl)
        g.add_device(node)
        link = Link(
            id="hid:ctrl→node:vm1",
            source=PortRef("controller", "hid_out"),
            sink=PortRef("node:vm1", "hid_in"),
            transport="udp_hid",
        )
        g.add_link(link)
        return g

    def test_device_count(self):
        g = self._make_graph_with_two_nodes()
        assert g.device_count == 2

    def test_link_count(self):
        g = self._make_graph_with_two_nodes()
        assert g.link_count == 1

    def test_get_device(self):
        g = self._make_graph_with_two_nodes()
        assert g.get_device("controller") is not None
        assert g.get_device("missing") is None

    def test_add_link_updates_port_state(self):
        g = self._make_graph_with_two_nodes()
        ctrl = g.get_device("controller")
        hid_out = ctrl.get_port("hid_out")
        assert PortRef("node:vm1", "hid_in") in hid_out.current_state.connected_to

    def test_remove_device_removes_links(self):
        g = self._make_graph_with_two_nodes()
        g.remove_device("node:vm1")
        assert g.device_count == 1
        assert g.link_count == 0

    def test_remove_link_clears_port_state(self):
        g = self._make_graph_with_two_nodes()
        g.remove_link("hid:ctrl→node:vm1")
        ctrl = g.get_device("controller")
        hid_out = ctrl.get_port("hid_out")
        assert PortRef("node:vm1", "hid_in") not in hid_out.current_state.connected_to

    def test_clear(self):
        g = self._make_graph_with_two_nodes()
        g.clear()
        assert g.device_count == 0
        assert g.link_count == 0

    def test_devices_by_type(self):
        g = self._make_graph_with_two_nodes()
        nodes = g.devices_by_type(DeviceType.node)
        assert len(nodes) == 1
        assert nodes[0].id == "node:vm1"

    def test_links_from(self):
        g = self._make_graph_with_two_nodes()
        links = g.links_from(PortRef("controller", "hid_out"))
        assert len(links) == 1

    def test_links_to(self):
        g = self._make_graph_with_two_nodes()
        links = g.links_to(PortRef("node:vm1", "hid_in"))
        assert len(links) == 1

    def test_to_dict(self):
        g = self._make_graph_with_two_nodes()
        d = g.to_dict()
        assert "devices" in d
        assert "links" in d
        assert d["stats"]["device_count"] == 2
        assert d["stats"]["link_count"] == 1

    def test_replace_device(self):
        g = self._make_graph_with_two_nodes()
        # Re-adding a device with same ID replaces it
        new_ctrl = Device("controller", "Controller v2", DeviceType.controller,
                          ports=[Port("hid_out", "controller", PortDirection.source, MediaType.hid)])
        g.add_device(new_ctrl)
        assert g.device_count == 2
        assert g.get_device("controller").name == "Controller v2"
