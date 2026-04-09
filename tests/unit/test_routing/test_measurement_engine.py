# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Unit tests for the active measurement engine (Phase 5 runtime)."""

from __future__ import annotations

import asyncio
import sys
import time
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

sys.path.insert(0, str(Path(__file__).parent.parent.parent.parent / "controller"))

import pytest
from routing.measurement import InfoQuality, MeasurementStore
from routing.measurement_engine import (
    MeasurementEngine,
    PingResult,
    _parse_ping_output,
    _run_ping,
)
from routing.model import (
    BandwidthSpec,
    Device,
    DeviceType,
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

pytestmark = pytest.mark.unit


# ── Helpers ───────────────────────────────────────────────────────────────────

def _make_graph_with_network_link(
    target_ip: str = "192.168.1.10",
) -> tuple[RoutingGraph, str]:
    """One controller → node link with target_ip for probing."""
    ctrl = Device(
        id="ctrl", name="ctrl", type=DeviceType.controller,
        ports=[Port(id="out", device_id="ctrl",
                    direction=PortDirection.source, media_type=MediaType.hid,
                    current_state=PortState())],
    )
    node = Device(
        id="node-a", name="node-a", type=DeviceType.node,
        ports=[Port(id="in", device_id="node-a",
                    direction=PortDirection.sink, media_type=MediaType.hid,
                    current_state=PortState())],
    )
    graph = RoutingGraph()
    graph.add_device(ctrl)
    graph.add_device(node)
    link = Link(
        id="l1",
        source=PortRef("ctrl", "out"),
        sink=PortRef("node-a", "in"),
        transport="udp_hid",
        state=LinkState(
            status=LinkStatus.active,
            latency=LatencySpec(min_ms=0.5, typical_ms=1.0, max_ms=5.0),
            loss=LossSpec(rate=0.0, window_seconds=10),
        ),
        properties={"target_ip": target_ip, "target_port": 7332},
    )
    graph.add_link(link)
    return graph, "l1"


# ── PingResult ────────────────────────────────────────────────────────────────

class TestPingResult:
    def test_loss_rate_none_lost(self):
        r = PingResult("h", 4, 4, 1.0, 2.0, 5.0, 0.5)
        assert r.loss_rate == 0.0

    def test_loss_rate_all_lost(self):
        r = PingResult("h", 4, 0, 0.0, 0.0, 0.0, 0.0)
        assert r.loss_rate == 1.0

    def test_loss_rate_partial(self):
        r = PingResult("h", 4, 2, 1.0, 2.0, 5.0, 0.5)
        assert r.loss_rate == pytest.approx(0.5)

    def test_p99_jitter_proportional_to_mdev(self):
        r = PingResult("h", 4, 4, 1.0, 2.0, 5.0, mdev_ms=10.0)
        assert r.p99_jitter_ms == pytest.approx(23.3)

    def test_p99_jitter_capped_at_1000ms(self):
        r = PingResult("h", 4, 4, 1.0, 2.0, 5.0, mdev_ms=10_000.0)
        assert r.p99_jitter_ms == 1000.0

    def test_zero_sent_gives_full_loss(self):
        r = PingResult("h", 0, 0, 0.0, 0.0, 0.0, 0.0)
        assert r.loss_rate == 1.0


# ── _parse_ping_output ────────────────────────────────────────────────────────

class TestParsePingOutput:
    # Typical Linux ping output
    LINUX_OK = (
        "PING 192.168.1.1 (192.168.1.1) 56(84) bytes of data.\n"
        "64 bytes from 192.168.1.1: icmp_seq=1 ttl=64 time=1.23 ms\n"
        "64 bytes from 192.168.1.1: icmp_seq=2 ttl=64 time=1.45 ms\n"
        "64 bytes from 192.168.1.1: icmp_seq=3 ttl=64 time=1.12 ms\n"
        "64 bytes from 192.168.1.1: icmp_seq=4 ttl=64 time=1.31 ms\n"
        "\n"
        "--- 192.168.1.1 ping statistics ---\n"
        "4 packets transmitted, 4 received, 0% packet loss, time 3003ms\n"
        "rtt min/avg/max/mdev = 1.120/1.277/1.450/0.122 ms\n"
    )

    LINUX_LOSS = (
        "--- 192.168.1.1 ping statistics ---\n"
        "4 packets transmitted, 2 received, 50% packet loss, time 3000ms\n"
        "rtt min/avg/max/mdev = 2.00/3.00/4.00/1.00 ms\n"
    )

    LINUX_ALL_LOST = (
        "--- 192.168.1.1 ping statistics ---\n"
        "4 packets transmitted, 0 received, 100% packet loss, time 3000ms\n"
    )

    MACOS_OK = (
        "PING 192.168.1.1 (192.168.1.1): 56 data bytes\n"
        "\n"
        "--- 192.168.1.1 ping statistics ---\n"
        "4 packets transmitted, 4 packets received, 0.0% packet loss\n"
        "round-trip min/avg/max/stddev = 1.120/1.277/1.450/0.122 ms\n"
    )

    def test_linux_ok(self):
        r = _parse_ping_output("h", self.LINUX_OK, 4)
        assert r is not None
        assert r.received == 4
        assert r.sent == 4
        assert r.loss_rate == 0.0
        assert r.avg_ms == pytest.approx(1.277)
        assert r.mdev_ms == pytest.approx(0.122)

    def test_linux_with_loss(self):
        r = _parse_ping_output("h", self.LINUX_LOSS, 4)
        assert r is not None
        assert r.loss_rate == pytest.approx(0.5)
        assert r.avg_ms == pytest.approx(3.0)

    def test_linux_all_lost(self):
        r = _parse_ping_output("h", self.LINUX_ALL_LOST, 4)
        assert r is not None
        assert r.received == 0
        assert r.loss_rate == 1.0
        assert r.avg_ms == 0.0

    def test_macos_ok(self):
        r = _parse_ping_output("h", self.MACOS_OK, 4)
        assert r is not None
        assert r.received == 4
        assert r.avg_ms == pytest.approx(1.277)

    def test_empty_output_returns_none(self):
        assert _parse_ping_output("h", "", 4) is None

    def test_garbage_output_returns_none(self):
        assert _parse_ping_output("h", "garbage\nno stats here\n", 4) is None


# ── MeasurementEngine unit tests ──────────────────────────────────────────────

class TestMeasurementEngineInit:
    def test_not_running_initially(self):
        graph = RoutingGraph()
        store = MeasurementStore()
        engine = MeasurementEngine(graph, store)
        assert not engine._running
        assert engine._task is None

    def test_to_dict_reflects_state(self):
        graph = RoutingGraph()
        store = MeasurementStore()
        engine = MeasurementEngine(graph, store)
        d = engine.to_dict()
        assert d["running"] is False
        assert d["task_active"] is False
        assert isinstance(d["failure_counts"], dict)


class TestMeasurementEngineLink:
    def test_link_with_target_ip_is_probeable(self):
        graph, lid = _make_graph_with_network_link()
        link = graph.get_link(lid)
        assert MeasurementEngine._link_has_target(link)

    def test_link_without_target_ip_not_probeable(self):
        link = Link(
            id="usb", source=PortRef("a", "p"), sink=PortRef("b", "p"),
            transport="usb_hid_gadget",
        )
        assert not MeasurementEngine._link_has_target(link)


class TestMeasurementEngineProbeLinkNow:
    """probe_link_now() returns False for missing links, True for valid ones."""

    @pytest.mark.asyncio
    async def test_returns_false_for_unknown_link(self):
        graph, _ = _make_graph_with_network_link()
        store = MeasurementStore()
        engine = MeasurementEngine(graph, store)
        result = await engine.probe_link_now("nonexistent")
        assert result is False

    @pytest.mark.asyncio
    async def test_records_measurements_on_success(self):
        graph, lid = _make_graph_with_network_link()
        store = MeasurementStore()
        engine = MeasurementEngine(graph, store)

        fake_result = PingResult("192.168.1.10", 4, 4, 1.0, 2.5, 5.0, 0.3)

        with patch(
            "routing.measurement_engine._run_ping",
            new=AsyncMock(return_value=fake_result),
        ):
            found = await engine.probe_link_now(lid)

        assert found is True
        # Latency was recorded
        qv = store.get("ctrl", f"link.{lid}.latency_ms", apply_decay=False)
        assert qv is not None
        assert qv.value == pytest.approx(2.5)
        assert qv.quality == InfoQuality.measured
        # Loss was recorded
        qv_loss = store.get("ctrl", f"link.{lid}.loss_rate", apply_decay=False)
        assert qv_loss is not None
        assert qv_loss.value == pytest.approx(0.0)
        # Jitter was recorded
        qv_jitter = store.get("ctrl", f"link.{lid}.jitter_p99_ms", apply_decay=False)
        assert qv_jitter is not None
        assert qv_jitter.value == pytest.approx(fake_result.p99_jitter_ms)

    @pytest.mark.asyncio
    async def test_failure_increments_counter(self):
        graph, lid = _make_graph_with_network_link()
        store = MeasurementStore()
        engine = MeasurementEngine(graph, store)

        with patch(
            "routing.measurement_engine._run_ping",
            new=AsyncMock(return_value=None),
        ):
            await engine.probe_link_now(lid)

        assert engine._failures.get(lid, 0) == 1

    @pytest.mark.asyncio
    async def test_marks_link_failed_after_threshold(self):
        from routing.measurement_engine import _FAILURE_THRESHOLD
        graph, lid = _make_graph_with_network_link()
        store = MeasurementStore()
        engine = MeasurementEngine(graph, store)
        # Pre-seed failure count just below threshold
        engine._failures[lid] = _FAILURE_THRESHOLD - 1

        with patch(
            "routing.measurement_engine._run_ping",
            new=AsyncMock(return_value=None),
        ):
            await engine.probe_link_now(lid)

        link = graph.get_link(lid)
        assert link.state.status == LinkStatus.failed

    @pytest.mark.asyncio
    async def test_success_resets_failure_counter(self):
        graph, lid = _make_graph_with_network_link()
        store = MeasurementStore()
        engine = MeasurementEngine(graph, store)
        engine._failures[lid] = 5  # pre-seed failures

        fake_result = PingResult("192.168.1.10", 4, 4, 1.0, 2.5, 5.0, 0.3)
        with patch(
            "routing.measurement_engine._run_ping",
            new=AsyncMock(return_value=fake_result),
        ):
            await engine.probe_link_now(lid)

        assert engine._failures[lid] == 0

    @pytest.mark.asyncio
    async def test_updates_link_state_on_success(self):
        graph, lid = _make_graph_with_network_link()
        store = MeasurementStore()
        engine = MeasurementEngine(graph, store)

        fake_result = PingResult("192.168.1.10", 4, 4, 0.8, 1.5, 4.0, 0.2)
        with patch(
            "routing.measurement_engine._run_ping",
            new=AsyncMock(return_value=fake_result),
        ):
            await engine.probe_link_now(lid)

        link = graph.get_link(lid)
        assert link.state.latency is not None
        assert link.state.latency.typical_ms == pytest.approx(1.5)
        assert link.state.latency.quality == InfoQuality.measured
        assert link.state.loss is not None
        assert link.state.loss.rate == pytest.approx(0.0)
        assert link.state.jitter is not None

    @pytest.mark.asyncio
    async def test_recovery_from_failed_state(self):
        graph, lid = _make_graph_with_network_link()
        store = MeasurementStore()
        engine = MeasurementEngine(graph, store)

        # Pre-mark link as failed
        link = graph.get_link(lid)
        link.state.status = LinkStatus.failed

        fake_result = PingResult("192.168.1.10", 4, 4, 1.0, 2.0, 5.0, 0.3)
        with patch(
            "routing.measurement_engine._run_ping",
            new=AsyncMock(return_value=fake_result),
        ):
            await engine.probe_link_now(lid)

        assert link.state.status == LinkStatus.active


class TestMeasurementEngineStartStop:
    @pytest.mark.asyncio
    async def test_start_creates_task(self):
        graph = RoutingGraph()
        store = MeasurementStore()
        engine = MeasurementEngine(graph, store)

        await engine.start()
        try:
            assert engine._running is True
            assert engine._task is not None
            assert not engine._task.done()
        finally:
            await engine.stop()

    @pytest.mark.asyncio
    async def test_stop_cancels_task(self):
        graph = RoutingGraph()
        store = MeasurementStore()
        engine = MeasurementEngine(graph, store)

        await engine.start()
        await engine.stop()

        assert engine._running is False
        assert engine._task is None

    @pytest.mark.asyncio
    async def test_double_start_is_idempotent(self):
        graph = RoutingGraph()
        store = MeasurementStore()
        engine = MeasurementEngine(graph, store)

        await engine.start()
        first_task = engine._task
        await engine.start()  # should be a no-op
        assert engine._task is first_task
        await engine.stop()

    @pytest.mark.asyncio
    async def test_to_dict_running_reflects_state(self):
        graph = RoutingGraph()
        store = MeasurementStore()
        engine = MeasurementEngine(graph, store)

        await engine.start()
        try:
            d = engine.to_dict()
            assert d["running"] is True
            assert d["task_active"] is True
        finally:
            await engine.stop()


class TestMeasurementEngineJournalIntegration:
    """Engine emits journal records on link failure/recovery."""

    @pytest.mark.asyncio
    async def test_journal_entry_on_link_failure(self):
        from routing.monitoring import MonitoringJournal, StateChangeType
        graph, lid = _make_graph_with_network_link()
        store = MeasurementStore()
        journal = MonitoringJournal()
        engine = MeasurementEngine(graph, store, journal=journal)
        from routing.measurement_engine import _FAILURE_THRESHOLD
        engine._failures[lid] = _FAILURE_THRESHOLD - 1

        with patch(
            "routing.measurement_engine._run_ping",
            new=AsyncMock(return_value=None),
        ):
            await engine.probe_link_now(lid)

        records = journal.query()
        assert any(
            r.type == StateChangeType.link_down for r in records
        )

    @pytest.mark.asyncio
    async def test_journal_entry_on_link_recovery(self):
        from routing.monitoring import MonitoringJournal, StateChangeType
        graph, lid = _make_graph_with_network_link()
        store = MeasurementStore()
        journal = MonitoringJournal()
        engine = MeasurementEngine(graph, store, journal=journal)

        link = graph.get_link(lid)
        link.state.status = LinkStatus.failed  # pre-mark as failed

        fake_result = PingResult("192.168.1.10", 4, 4, 1.0, 2.0, 5.0, 0.3)
        with patch(
            "routing.measurement_engine._run_ping",
            new=AsyncMock(return_value=fake_result),
        ):
            await engine.probe_link_now(lid)

        records = journal.query()
        assert any(
            r.type == StateChangeType.link_recovered for r in records
        )
