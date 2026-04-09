# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Unit tests for the routing engine (Phase 2)."""

from __future__ import annotations

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent.parent.parent.parent / "controller"))

import pytest
from routing.formats import FormatRange, FormatSet
from routing.model import (
    BandwidthSpec,
    Device,
    DeviceType,
    InfoQuality,
    LatencySpec,
    Link,
    LinkState,
    LinkStatus,
    LossSpec,
    MediaType,
    Port,
    PortDirection,
    PortRef,
    PortState,
)
from routing.graph import RoutingGraph
from routing.intent import (
    BUILTIN_INTENTS,
    Constraints,
    Intent,
    Preferences,
    StreamIntent,
)
from routing.router import (
    ConstraintChecker,
    CandidatePath,
    Router,
    _compute_cost,
    _enumerate_paths,
)

pytestmark = pytest.mark.unit


# ── Graph helpers ─────────────────────────────────────────────────────────────

def _make_port(
    port_id: str,
    direction: PortDirection,
    media_type: MediaType = MediaType.hid,
) -> Port:
    return Port(
        id=port_id,
        device_id="",   # set by caller
        direction=direction,
        media_type=media_type,
        current_state=PortState(),
    )


def _make_device(device_id: str, dtype: DeviceType, ports: list[Port]) -> Device:
    for p in ports:
        p.device_id = device_id
    return Device(id=device_id, name=device_id, type=dtype, ports=ports)


def _make_link(
    link_id: str,
    src_device: str,
    src_port: str,
    sink_device: str,
    sink_port: str,
    latency_ms: float = 1.0,
    status: LinkStatus = LinkStatus.active,
    loss: float = 0.0,
) -> Link:
    return Link(
        id=link_id,
        source=PortRef(device_id=src_device, port_id=src_port),
        sink=PortRef(device_id=sink_device, port_id=sink_port),
        transport="udp_hid",
        state=LinkState(
            status=status,
            latency=LatencySpec(
                min_ms=latency_ms * 0.5,
                typical_ms=latency_ms,
                max_ms=latency_ms * 2,
            ),
            loss=LossSpec(rate=loss, window_seconds=60),
            bandwidth=BandwidthSpec(
                capacity_bps=100_000_000,
                available_bps=100_000_000,
                used_bps=0,
            ),
        ),
    )


def _two_node_graph() -> tuple[RoutingGraph, PortRef, PortRef]:
    """
    Simple two-hop graph:
      controller:hid-out → node-a:hid-in
    """
    ctrl = _make_device("ctrl", DeviceType.controller, [
        _make_port("hid-out", PortDirection.source, MediaType.hid),
    ])
    node_a = _make_device("node-a", DeviceType.node, [
        _make_port("hid-in", PortDirection.sink, MediaType.hid),
    ])

    graph = RoutingGraph()
    graph.add_device(ctrl)
    graph.add_device(node_a)
    link = _make_link("l1", "ctrl", "hid-out", "node-a", "hid-in")
    graph.add_link(link)

    src = PortRef(device_id="ctrl", port_id="hid-out")
    dst = PortRef(device_id="node-a", port_id="hid-in")
    return graph, src, dst


def _multi_hop_graph() -> tuple[RoutingGraph, PortRef, PortRef]:
    """
    Three-device, two-hop graph:
      ctrl:out → relay:in  (relay:out → node-a:in)
    """
    ctrl = _make_device("ctrl", DeviceType.controller, [
        _make_port("out", PortDirection.source, MediaType.hid),
    ])
    relay = _make_device("relay", DeviceType.network_switch, [
        _make_port("in", PortDirection.sink, MediaType.hid),
        _make_port("out", PortDirection.source, MediaType.hid),
    ])
    node_a = _make_device("node-a", DeviceType.node, [
        _make_port("in", PortDirection.sink, MediaType.hid),
    ])

    graph = RoutingGraph()
    for d in (ctrl, relay, node_a):
        graph.add_device(d)
    graph.add_link(_make_link("l1", "ctrl", "out", "relay", "in", latency_ms=2.0))
    graph.add_link(_make_link("l2", "relay", "out", "node-a", "in", latency_ms=3.0))

    src = PortRef(device_id="ctrl", port_id="out")
    dst = PortRef(device_id="node-a", port_id="in")
    return graph, src, dst


# ── CandidatePath ─────────────────────────────────────────────────────────────

class TestCandidatePath:
    def _link(self, latency=5.0, loss=0.0):
        return _make_link("l1", "a", "p1", "b", "p2",
                          latency_ms=latency, loss=loss)

    def test_feasible_when_no_violations(self):
        p = CandidatePath(links=[self._link()])
        assert p.feasible

    def test_hop_count(self):
        p = CandidatePath(links=[self._link(), self._link()])
        assert p.hop_count == 2

    def test_total_latency(self):
        l1 = _make_link("l1", "a", "p1", "b", "p2", latency_ms=10.0)
        l2 = _make_link("l2", "b", "p2", "c", "p3", latency_ms=5.0)
        p = CandidatePath(links=[l1, l2])
        assert p.total_latency_ms == pytest.approx(15.0)

    def test_bottleneck_bandwidth(self):
        l1 = _make_link("l1", "a", "p1", "b", "p2")
        l1.state.bandwidth.available_bps = 50_000_000
        l2 = _make_link("l2", "b", "p2", "c", "p3")
        l2.state.bandwidth.available_bps = 10_000_000
        p = CandidatePath(links=[l1, l2])
        assert p.bottleneck_bandwidth_bps == 10_000_000


# ── ConstraintChecker ─────────────────────────────────────────────────────────

class TestConstraintChecker:
    def _stream(self, **kwargs) -> StreamIntent:
        return StreamIntent(
            media_type=MediaType.hid,
            constraints=Constraints(**kwargs),
        )

    def _path(self, latency=5.0, bw=100_000_000, loss=0.0, hops=1):
        links = [
            _make_link(f"l{i}", f"d{i}", "p1", f"d{i+1}", "p2",
                       latency_ms=latency / hops, loss=loss)
            for i in range(hops)
        ]
        return CandidatePath(links=links)

    def test_passes_with_no_constraints(self):
        checker = ConstraintChecker()
        stream = self._stream()
        violations = checker.check(self._path(), stream)
        assert violations == []

    def test_latency_violation(self):
        checker = ConstraintChecker()
        stream = self._stream(max_latency_ms=1.0)
        path = self._path(latency=50.0)
        violations = checker.check(path, stream)
        assert any(v.check == "latency" for v in violations)

    def test_latency_passes_at_limit(self):
        checker = ConstraintChecker()
        stream = self._stream(max_latency_ms=5.0)
        path = self._path(latency=5.0)
        violations = checker.check(path, stream)
        assert not any(v.check == "latency" for v in violations)

    def test_hops_violation(self):
        checker = ConstraintChecker()
        stream = self._stream(max_hops=1)
        path = self._path(hops=3)
        violations = checker.check(path, stream)
        assert any(v.check == "hops" for v in violations)

    def test_hops_passes(self):
        checker = ConstraintChecker()
        stream = self._stream(max_hops=3)
        path = self._path(hops=2)
        violations = checker.check(path, stream)
        assert not any(v.check == "hops" for v in violations)

    def test_loss_violation(self):
        checker = ConstraintChecker()
        stream = self._stream(max_loss=0.0)
        path = self._path(loss=0.01)
        violations = checker.check(path, stream)
        assert any(v.check == "loss" for v in violations)

    def test_loss_passes(self):
        checker = ConstraintChecker()
        stream = self._stream(max_loss=0.05)
        path = self._path(loss=0.01)
        violations = checker.check(path, stream)
        assert not any(v.check == "loss" for v in violations)

    def test_bandwidth_violation(self):
        checker = ConstraintChecker()
        stream = self._stream(min_bandwidth_bps=200_000_000)
        path = self._path(bw=100_000_000)
        # manually set available bw below requirement
        path.links[0].state.bandwidth.available_bps = 5_000_000
        violations = checker.check(path, stream)
        assert any(v.check == "bandwidth" for v in violations)


# ── Cost function ─────────────────────────────────────────────────────────────

class TestComputeCost:
    def test_fewer_hops_is_cheaper(self):
        link_short = _make_link("l1", "a", "p1", "b", "p2", latency_ms=5.0)
        link_long1 = _make_link("l2", "a", "p1", "b", "p2", latency_ms=3.0)
        link_long2 = _make_link("l3", "b", "p1", "c", "p2", latency_ms=3.0)

        path_1hop = CandidatePath(links=[link_short])
        path_2hop = CandidatePath(links=[link_long1, link_long2])

        prefs = Preferences(prefer_fewer_hops=True)
        c1 = _compute_cost(path_1hop, prefs)
        c2 = _compute_cost(path_2hop, prefs)
        assert c1 < c2

    def test_lower_latency_is_cheaper_when_preferred(self):
        fast = CandidatePath(links=[
            _make_link("l1", "a", "p1", "b", "p2", latency_ms=1.0)
        ])
        slow = CandidatePath(links=[
            _make_link("l2", "a", "p1", "b", "p2", latency_ms=100.0)
        ])
        prefs = Preferences(prefer_lower_latency=True)
        assert _compute_cost(fast, prefs) < _compute_cost(slow, prefs)

    def test_cost_is_non_negative(self):
        path = CandidatePath(links=[
            _make_link("l1", "a", "p1", "b", "p2")
        ])
        cost = _compute_cost(path, Preferences())
        assert cost >= 0.0


# ── _enumerate_paths ──────────────────────────────────────────────────────────

class TestEnumeratePaths:
    def test_direct_path_found(self):
        graph, src, dst = _two_node_graph()
        paths = _enumerate_paths(graph, src, dst, MediaType.hid)
        assert len(paths) == 1
        assert paths[0].hop_count == 1

    def test_no_path_returns_empty(self):
        graph, src, dst = _two_node_graph()
        # Source points to wrong port
        wrong_src = PortRef(device_id="ctrl", port_id="nonexistent")
        paths = _enumerate_paths(graph, wrong_src, dst, MediaType.hid)
        assert len(paths) == 0

    def test_multi_hop_path(self):
        graph, src, dst = _multi_hop_graph()
        paths = _enumerate_paths(graph, src, dst, MediaType.hid)
        assert len(paths) == 1
        assert paths[0].hop_count == 2

    def test_max_hops_limits_search(self):
        graph, src, dst = _multi_hop_graph()
        # 2-hop path, but limit to 1 hop
        paths = _enumerate_paths(graph, src, dst, MediaType.hid, max_hops=1)
        assert len(paths) == 0

    def test_failed_links_excluded(self):
        graph, src, dst = _two_node_graph()
        link = list(graph.links())[0]
        link.state.status = LinkStatus.failed
        paths = _enumerate_paths(graph, src, dst, MediaType.hid)
        assert len(paths) == 0


# ── Router ────────────────────────────────────────────────────────────────────

class TestRouter:
    def test_recommend_desktop_returns_pipelines(self):
        graph, src, dst = _two_node_graph()
        router = Router(graph)
        intent = BUILTIN_INTENTS["control"]
        results = router.recommend(intent, src, dst)
        # HID stream should have at least one pipeline
        assert len(results) > 0
        stream_intent, pipelines = results[0]
        assert stream_intent.media_type == MediaType.hid
        assert len(pipelines) >= 1

    def test_recommend_top_n_respected(self):
        # Add parallel paths to same destination
        ctrl = _make_device("ctrl", DeviceType.controller, [
            _make_port("out", PortDirection.source, MediaType.hid),
        ])
        r1 = _make_device("r1", DeviceType.network_switch, [
            _make_port("in", PortDirection.sink, MediaType.hid),
            _make_port("out", PortDirection.source, MediaType.hid),
        ])
        r2 = _make_device("r2", DeviceType.network_switch, [
            _make_port("in", PortDirection.sink, MediaType.hid),
            _make_port("out", PortDirection.source, MediaType.hid),
        ])
        node = _make_device("node", DeviceType.node, [
            _make_port("in", PortDirection.sink, MediaType.hid),
        ])
        graph = RoutingGraph()
        for d in (ctrl, r1, r2, node):
            graph.add_device(d)
        graph.add_link(_make_link("l1", "ctrl", "out", "r1", "in", latency_ms=2.0))
        graph.add_link(_make_link("l2", "r1", "out", "node", "in", latency_ms=3.0))
        graph.add_link(_make_link("l3", "ctrl", "out", "r2", "in", latency_ms=4.0))
        graph.add_link(_make_link("l4", "r2", "out", "node", "in", latency_ms=5.0))

        router = Router(graph)
        intent = BUILTIN_INTENTS["control"]
        src = PortRef("ctrl", "out")
        dst = PortRef("node", "in")
        results = router.recommend(intent, src, dst, top_n=2)
        _, pipelines = results[0]
        assert len(pipelines) <= 2

    def test_recommend_rejects_latency_violation(self):
        graph, src, dst = _two_node_graph()
        # Set a very tight latency constraint
        intent = Intent(
            name="tight",
            streams=[StreamIntent(
                media_type=MediaType.hid,
                required=True,
                constraints=Constraints(max_latency_ms=0.0001),
            )],
        )
        router = Router(graph)
        results = router.recommend(intent, src, dst)
        _, pipelines = results[0]
        assert len(pipelines) == 0

    def test_recommend_pipelines_sorted_by_cost(self):
        # Two paths: fast direct, slow indirect
        ctrl = _make_device("ctrl", DeviceType.controller, [
            _make_port("out", PortDirection.source, MediaType.hid),
        ])
        relay = _make_device("relay", DeviceType.network_switch, [
            _make_port("in", PortDirection.sink, MediaType.hid),
            _make_port("out", PortDirection.source, MediaType.hid),
        ])
        node = _make_device("node", DeviceType.node, [
            _make_port("in", PortDirection.sink, MediaType.hid),
        ])
        graph = RoutingGraph()
        for d in (ctrl, relay, node):
            graph.add_device(d)
        graph.add_link(_make_link("direct", "ctrl", "out", "node", "in", latency_ms=2.0))
        graph.add_link(_make_link("via-r1", "ctrl", "out", "relay", "in", latency_ms=5.0))
        graph.add_link(_make_link("via-r2", "relay", "out", "node", "in", latency_ms=5.0))

        router = Router(graph)
        intent = BUILTIN_INTENTS["control"]
        src = PortRef("ctrl", "out")
        dst = PortRef("node", "in")
        results = router.recommend(intent, src, dst, top_n=5)
        _, pipelines = results[0]
        # Verify cost is non-decreasing
        costs = [p.aggregate.total_latency_ms for p in pipelines]
        assert costs == sorted(costs)

    def test_check_feasibility_true(self):
        graph, src, dst = _two_node_graph()
        router = Router(graph)
        intent = BUILTIN_INTENTS["control"]
        result = router.check_feasibility(intent, src, dst)
        assert result[MediaType.hid] is True

    def test_check_feasibility_false_no_path(self):
        graph, src, dst = _two_node_graph()
        wrong_dst = PortRef(device_id="nonexistent", port_id="p1")
        router = Router(graph)
        intent = BUILTIN_INTENTS["control"]
        result = router.check_feasibility(intent, src, wrong_dst)
        assert result[MediaType.hid] is False

    def test_optional_stream_always_feasible(self):
        graph, src, dst = _two_node_graph()
        intent = Intent(
            name="test",
            streams=[StreamIntent(
                media_type=MediaType.audio,
                required=False,
            )],
        )
        router = Router(graph)
        result = router.check_feasibility(intent, src, dst)
        assert result[MediaType.audio] is True

    def test_pipeline_ids_unique(self):
        graph, src, dst = _two_node_graph()
        router = Router(graph)
        intent = BUILTIN_INTENTS["control"]
        results1 = router.recommend(intent, src, dst)
        results2 = router.recommend(intent, src, dst)
        _, p1 = results1[0]
        _, p2 = results2[0]
        # Each call produces new UUIDs
        if p1 and p2:
            assert p1[0].id != p2[0].id


# ── Format negotiation (check #9) ────────────────────────────────────────────

def _make_port_with_fmt(
    port_id: str,
    direction: PortDirection,
    media_type: MediaType = MediaType.hid,
    format_set: FormatSet | None = None,
) -> Port:
    return Port(
        id=port_id,
        device_id="",
        direction=direction,
        media_type=media_type,
        current_state=PortState(),
        format_set=format_set,
    )


def _hid_format_set(protocols: list[str] | None = None) -> FormatSet:
    """FormatSet restricted to specific HID protocols (or unrestricted if None)."""
    if protocols is None:
        return FormatSet(formats=[FormatRange(media_type=MediaType.hid)])
    return FormatSet(formats=[
        FormatRange(media_type=MediaType.hid, hid_protocols=protocols)
    ])


class TestConstraintCheckerFormatNegotiation:
    """Check #9: format negotiation rejects incompatible port combinations."""

    def _two_port_graph(
        self,
        src_fmt: FormatSet | None,
        dst_fmt: FormatSet | None,
    ) -> tuple[RoutingGraph, PortRef, PortRef]:
        ctrl = _make_device("ctrl", DeviceType.controller, [
            _make_port_with_fmt("out", PortDirection.source,
                                MediaType.hid, src_fmt),
        ])
        node = _make_device("node", DeviceType.node, [
            _make_port_with_fmt("in", PortDirection.sink,
                                MediaType.hid, dst_fmt),
        ])
        graph = RoutingGraph()
        graph.add_device(ctrl)
        graph.add_device(node)
        graph.add_link(_make_link("l1", "ctrl", "out", "node", "in"))
        src = PortRef("ctrl", "out")
        dst = PortRef("node", "in")
        return graph, src, dst

    def test_compatible_format_sets_pass(self):
        graph, src, dst = self._two_port_graph(
            _hid_format_set(),
            _hid_format_set(),
        )
        checker = ConstraintChecker(graph)
        link = list(graph.links())[0]
        path = CandidatePath(links=[link])
        stream = StreamIntent(media_type=MediaType.hid, constraints=Constraints())
        violations = checker.check(path, stream)
        assert not any(v.check == "format" for v in violations)

    def test_no_format_sets_skips_check(self):
        """Ports without format_set → check is skipped, no violation."""
        graph, src, dst = self._two_port_graph(None, None)
        checker = ConstraintChecker(graph)
        link = list(graph.links())[0]
        path = CandidatePath(links=[link])
        stream = StreamIntent(media_type=MediaType.hid, constraints=Constraints())
        violations = checker.check(path, stream)
        assert not any(v.check == "format" for v in violations)

    def test_no_graph_skips_check(self):
        """ConstraintChecker without graph → check #9 never runs."""
        graph, src, dst = self._two_port_graph(
            _hid_format_set(),
            _hid_format_set(),
        )
        # No graph = no check
        checker = ConstraintChecker(graph=None)
        link = list(graph.links())[0]
        path = CandidatePath(links=[link])
        stream = StreamIntent(media_type=MediaType.hid, constraints=Constraints())
        violations = checker.check(path, stream)
        assert not any(v.check == "format" for v in violations)

    def test_router_passes_graph_to_checker(self):
        """Router() passes the graph so format check is active."""
        graph, src, dst = self._two_port_graph(
            _hid_format_set(),
            _hid_format_set(),
        )
        router = Router(graph)
        assert router._checker._graph is graph


class TestBuilderFormatSetWiring:
    """Format sets are added to all ports by GraphBuilder._add_node()."""

    def _make_node_info(self) -> object:
        class FakeNode:
            id = "vm1"
            host = "127.0.0.1"
            port = 7332
            role = "compute"
            hw = "vm"
            fw_version = "0.0.0"
            proto_version = 3
            capabilities: list = []
            machine_class = "workstation"
            audio_type = "vban"
            audio_sink = None
            audio_vban_port = 6980
            mic_vban_port = 6981
            stream_port = 7382
            stream_path = "/stream/stream.m3u8"
            vnc_host = None
            vnc_port = None
            capture_device = None
            display_outputs: list = []
            camera_streams: list = []
            frigate_host = None
            frigate_port = None

        return FakeNode()

    def test_node_hid_ports_have_format_set(self):
        from routing.builder import GraphBuilder
        graph = RoutingGraph()
        builder = GraphBuilder(graph)

        class FakeState:
            nodes: dict = {}
            active_node_id = None

        node_info = self._make_node_info()
        builder._add_controller(FakeState())
        builder._add_node(node_info, FakeState())

        node_dev = graph.get_device("node:vm1")
        assert node_dev is not None
        hid_in = node_dev.get_port("hid_in")
        assert hid_in is not None
        assert hid_in.format_set is not None
        hid_usb_out = node_dev.get_port("hid_usb_out")
        assert hid_usb_out is not None
        assert hid_usb_out.format_set is not None

    def test_node_audio_ports_have_format_set(self):
        from routing.builder import GraphBuilder
        graph = RoutingGraph()
        builder = GraphBuilder(graph)

        class FakeState:
            nodes: dict = {}
            active_node_id = None

        node_info = self._make_node_info()
        builder._add_controller(FakeState())
        builder._add_node(node_info, FakeState())

        node_dev = graph.get_device("node:vm1")
        assert node_dev is not None
        # VBAN audio in
        audio_vban = node_dev.get_port("audio_vban_in")
        assert audio_vban is not None
        assert audio_vban.format_set is not None
        # Mic VBAN out
        mic_out = node_dev.get_port("mic_vban_out")
        assert mic_out is not None
        assert mic_out.format_set is not None

    def test_node_video_port_has_format_set(self):
        from routing.builder import GraphBuilder
        graph = RoutingGraph()
        builder = GraphBuilder(graph)

        class FakeState:
            nodes: dict = {}
            active_node_id = None

        node_info = self._make_node_info()
        builder._add_controller(FakeState())
        builder._add_node(node_info, FakeState())

        node_dev = graph.get_device("node:vm1")
        assert node_dev is not None
        video_out = node_dev.get_port("video_out")
        assert video_out is not None
        assert video_out.format_set is not None

    def test_target_ports_have_format_set(self):
        from routing.builder import GraphBuilder
        graph = RoutingGraph()
        builder = GraphBuilder(graph)

        class FakeState:
            nodes: dict = {}
            active_node_id = None

        node_info = self._make_node_info()
        builder._add_controller(FakeState())
        builder._add_node(node_info, FakeState())

        target_dev = graph.get_device("target:vm1")
        assert target_dev is not None
        for port in target_dev.ports:
            assert port.format_set is not None, (
                f"target port {port.id} missing format_set"
            )

    def test_controller_ports_have_format_set(self):
        from routing.builder import GraphBuilder
        graph = RoutingGraph()
        builder = GraphBuilder(graph)

        class FakeState:
            nodes: dict = {}
            active_node_id = None

        builder._add_controller(FakeState())
        ctrl_dev = graph.get_device("controller")
        assert ctrl_dev is not None
        for port in ctrl_dev.ports:
            assert port.format_set is not None, (
                f"controller port {port.id} missing format_set"
            )
