# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Unit tests for the Pipeline primitive (Phase 2)."""

from __future__ import annotations

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent.parent.parent.parent / "controller"))

import pytest
from routing.model import (
    ActivationTimeSpec,
    InfoQuality,
    LinkStatus,
    PortRef,
)
from routing.intent import BUILTIN_INTENTS
from routing.pipeline import (
    ConversionRef,
    FormatRef,
    LinkRef,
    Pipeline,
    PipelineHop,
    PipelineMetrics,
    PipelineState,
    WarmCost,
    WarmthPolicy,
)

pytestmark = pytest.mark.unit


# ── FormatRef ─────────────────────────────────────────────────────────────────

class TestFormatRef:
    def test_basic(self):
        f = FormatRef(media_type="video", codec="h264", width=1920, height=1080)
        assert f.width == 1920
        assert not f.lossy

    def test_to_dict_excludes_none(self):
        f = FormatRef(media_type="audio", sample_rate=48000)
        d = f.to_dict()
        assert d["media_type"] == "audio"
        assert d["sample_rate"] == 48000
        assert "width" not in d

    def test_lossy_in_dict(self):
        f = FormatRef(media_type="video", lossy=True)
        assert f.to_dict()["lossy"] is True


# ── PipelineHop ───────────────────────────────────────────────────────────────

class TestPipelineHop:
    def test_no_conversion(self):
        hop = PipelineHop(link=LinkRef("link-1"))
        assert not hop.has_conversion

    def test_with_conversion(self):
        hop = PipelineHop(
            link=LinkRef("link-1"),
            conversion=ConversionRef(device_id="codec-1", description="H264→H265"),
        )
        assert hop.has_conversion

    def test_to_dict(self):
        hop = PipelineHop(
            link=LinkRef("link-1"),
            latency_contribution_ms=5.0,
            current_state=LinkStatus.active,
        )
        d = hop.to_dict()
        assert d["link"]["link_id"] == "link-1"
        assert d["latency_contribution_ms"] == 5.0
        assert d["current_state"] == "active"
        assert d["conversion"] is None


# ── PipelineMetrics ───────────────────────────────────────────────────────────

class TestPipelineMetrics:
    def test_empty_hops(self):
        m = PipelineMetrics.from_hops([])
        assert m.total_latency_ms == 0.0
        assert m.total_hops == 0

    def test_latency_sum(self):
        hops = [
            PipelineHop(link=LinkRef("l1"), latency_contribution_ms=10.0),
            PipelineHop(link=LinkRef("l2"), latency_contribution_ms=5.0),
        ]
        m = PipelineMetrics.from_hops(hops)
        assert m.total_latency_ms == 15.0
        assert m.total_hops == 2

    def test_conversion_count(self):
        hops = [
            PipelineHop(
                link=LinkRef("l1"),
                conversion=ConversionRef(device_id="c1"),
            ),
            PipelineHop(link=LinkRef("l2")),
        ]
        m = PipelineMetrics.from_hops(hops)
        assert m.total_conversions == 1

    def test_activation_time_sum(self):
        at = ActivationTimeSpec(
            cold_to_warm_ms=200.0,
            warm_to_active_ms=50.0,
            active_to_warm_ms=10.0,
            warm_to_standby_ms=5000.0,
        )
        hops = [
            PipelineHop(link=LinkRef("l1"), activation_time=at),
            PipelineHop(link=LinkRef("l2"), activation_time=at),
        ]
        m = PipelineMetrics.from_hops(hops)
        assert m.cold_activation_time_ms == (200.0 + 50.0) * 2
        assert m.warm_activation_time_ms == 50.0 * 2

    def test_weakest_quality_degrades(self):
        at_good = ActivationTimeSpec(100, 10, 5, 500, quality=InfoQuality.measured)
        at_poor = ActivationTimeSpec(500, 50, 20, 5000, quality=InfoQuality.assumed)
        hops = [
            PipelineHop(link=LinkRef("l1"), activation_time=at_good),
            PipelineHop(link=LinkRef("l2"), activation_time=at_poor),
        ]
        m = PipelineMetrics.from_hops(hops)
        assert m.weakest_quality == InfoQuality.assumed

    def test_to_dict(self):
        m = PipelineMetrics(total_latency_ms=20.0, total_hops=2)
        d = m.to_dict()
        assert d["total_latency_ms"] == 20.0
        assert d["total_hops"] == 2


# ── WarmthPolicy ─────────────────────────────────────────────────────────────

class TestWarmthPolicy:
    def test_defaults(self):
        w = WarmthPolicy()
        assert not w.keep_warm
        assert w.max_warm_duration_s is None

    def test_to_dict(self):
        w = WarmthPolicy(keep_warm=True, warm_priority=80, max_warm_duration_s=3600)
        d = w.to_dict()
        assert d["keep_warm"] is True
        assert d["warm_priority"] == 80
        assert d["max_warm_duration_s"] == 3600


# ── WarmCost ─────────────────────────────────────────────────────────────────

class TestWarmCost:
    def test_defaults(self):
        w = WarmCost()
        assert w.cpu_percent == 0.0
        assert w.memory_mb == 0.0

    def test_to_dict(self):
        w = WarmCost(cpu_percent=5.0, memory_mb=128.0, bandwidth_bps=1_000_000)
        d = w.to_dict()
        assert d["cpu_percent"] == 5.0
        assert d["bandwidth_bps"] == 1_000_000


# ── Pipeline ──────────────────────────────────────────────────────────────────

class TestPipeline:
    def _make(self, hops=None):
        return Pipeline(
            id="pipe-1",
            intent=BUILTIN_INTENTS["desktop"],
            source=PortRef(device_id="ctrl", port_id="hid-out"),
            destination=PortRef(device_id="node-a", port_id="hid-in"),
            hops=hops or [],
        )

    def test_basic(self):
        p = self._make()
        assert p.id == "pipe-1"
        assert p.state == PipelineState.standby
        assert p.intent.name == "desktop"

    def test_to_dict(self):
        p = self._make()
        d = p.to_dict()
        assert d["id"] == "pipe-1"
        assert d["intent"] == "desktop"
        assert d["state"] == "standby"
        assert d["source"]["device_id"] == "ctrl"
        assert d["destination"]["device_id"] == "node-a"

    def test_recompute_metrics(self):
        hops = [
            PipelineHop(link=LinkRef("l1"), latency_contribution_ms=8.0),
            PipelineHop(link=LinkRef("l2"), latency_contribution_ms=12.0),
        ]
        p = self._make(hops=hops)
        p.recompute_metrics()
        assert p.aggregate.total_latency_ms == 20.0
        assert p.aggregate.total_hops == 2

    def test_to_dict_includes_hops(self):
        hops = [PipelineHop(link=LinkRef("l1"))]
        p = self._make(hops=hops)
        d = p.to_dict()
        assert len(d["hops"]) == 1
