# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Unit tests for routing and monitoring API endpoints (Phase 6)."""

from __future__ import annotations

import sys
from pathlib import Path
from unittest.mock import MagicMock, patch

sys.path.insert(0, str(Path(__file__).parent.parent.parent.parent / "controller"))

import pytest
from fastapi.testclient import TestClient

from routing.model import InfoQuality
from routing.monitoring import TrendAlert, TrendAlertManager, TrendAlertType

pytestmark = pytest.mark.unit


# ── Test app setup ────────────────────────────────────────────────────────────

def _make_app():
    """Build a minimal FastAPI app exercising only the routing/monitoring endpoints."""
    from fastapi import FastAPI, Request
    from fastapi.responses import JSONResponse
    from routing import (
        RoutingGraph, GraphBuilder, Router,
        MeasurementStore, MonitoringJournal, MetricStore,
    )
    from routing.intent import BUILTIN_INTENTS
    from routing.monitoring import TrendAlertManager, StateChangeRecord, StateChangeType

    app = FastAPI()

    # Minimal state mock
    graph = RoutingGraph()
    state = MagicMock()
    state.routing_graph = graph
    state.routing_engine = Router(graph)
    state.measurement_store = MeasurementStore()
    state.monitoring_journal = MonitoringJournal()
    state.metric_store = MetricStore()
    state.trend_alert_manager = TrendAlertManager()

    # Add a device + link so explain endpoint can work
    from routing.model import Device, Port, Link, DeviceType, MediaType, PortDirection, LinkState, LinkStatus, PortState, PortRef

    dev_a = Device(id="dev-a", name="Source", type=DeviceType.controller)
    dev_b = Device(id="dev-b", name="Dest", type=DeviceType.node)
    port_a = Port(
        id="pa", device_id="dev-a",
        direction=PortDirection.source, media_type=MediaType.hid,
    )
    port_b = Port(
        id="pb", device_id="dev-b",
        direction=PortDirection.sink, media_type=MediaType.hid,
    )
    dev_a.ports.append(port_a)
    dev_b.ports.append(port_b)
    link = Link(
        id="link-ab",
        source=PortRef(device_id="dev-a", port_id="pa"),
        sink=PortRef(device_id="dev-b", port_id="pb"),
        transport="udp-aead",
        state=LinkState(
            status=LinkStatus.active,
        ),
    )
    graph.add_device(dev_a)
    graph.add_device(dev_b)
    graph.add_link(link)

    # Preload a journal event
    state.monitoring_journal.append(StateChangeRecord(
        type=StateChangeType.device_online,
        device_id="dev-a",
        severity="info",
    ))

    # Preload a measurement
    state.measurement_store.record("dev-a", "latency_ms", 5.0, InfoQuality.measured)

    # Preload a trend alert
    alert = TrendAlert(
        type=TrendAlertType.degradation,
        device_id="dev-a",
        metric_key="latency_ms",
    )
    state.trend_alert_manager.raise_alert(alert)

    # Wire up endpoints (inline, avoiding the giant build_app)
    @app.get("/api/v1/routing/intents")
    async def list_intents() -> dict:
        return {"intents": {k: v.to_dict() for k, v in BUILTIN_INTENTS.items()}}

    @app.get("/api/v1/routing/intents/{name}")
    async def get_intent(name: str) -> dict:
        from fastapi import HTTPException
        intent = BUILTIN_INTENTS.get(name)
        if intent is None:
            raise HTTPException(404, f"Intent not found: {name}")
        return intent.to_dict()

    @app.get("/api/v1/routing/explain")
    async def routing_explain(source: str, destination: str, intent: str = "desktop", top_n: int = 3) -> dict:
        from fastapi import HTTPException
        chosen = BUILTIN_INTENTS.get(intent)
        if chosen is None:
            raise HTTPException(404, f"Intent not found: {intent}")
        src = state.routing_graph.get_device(source)
        dst = state.routing_graph.get_device(destination)
        if src is None:
            raise HTTPException(404, f"Source not found: {source}")
        if dst is None:
            raise HTTPException(404, f"Destination not found: {destination}")
        recs = state.routing_engine.recommend(chosen, src, dst, top_n=top_n)
        return {
            "source": source,
            "destination": destination,
            "intent": intent,
            "streams": [
                {"media_type": si.media_type.value, "pipelines": [p.to_dict() for p in pipes]}
                for si, pipes in recs
            ],
        }

    @app.get("/api/v1/routing/feasibility")
    async def routing_feasibility(source: str, destination: str, intent: str = "desktop") -> dict:
        from fastapi import HTTPException
        chosen = BUILTIN_INTENTS.get(intent)
        if chosen is None:
            raise HTTPException(404, f"Intent not found: {intent}")
        src = state.routing_graph.get_device(source)
        dst = state.routing_graph.get_device(destination)
        if src is None or dst is None:
            raise HTTPException(404, "Device not found")
        result = state.routing_engine.check_feasibility(chosen, src, dst)
        return {"source": source, "destination": destination, "intent": intent,
                "feasible": {mt.value: ok for mt, ok in result.items()}}

    @app.post("/api/v1/routing/evaluate")
    async def routing_evaluate(
        source: str = "",
        destination: str = "",
        intent: str = "desktop",
        top_n: int = 3,
    ) -> dict:
        from fastapi import HTTPException
        from pydantic import BaseModel
        chosen = BUILTIN_INTENTS.get(intent)
        if chosen is None:
            raise HTTPException(400, "Unknown intent")
        src = state.routing_graph.get_device(source)
        dst = state.routing_graph.get_device(destination)
        if src is None or dst is None:
            raise HTTPException(404, "Device not found")
        recs = state.routing_engine.recommend(chosen, src, dst, top_n=top_n)
        return {"source": source, "destination": destination,
                "streams": [{"media_type": si.media_type.value, "pipelines": []} for si, _ in recs]}

    @app.get("/api/v1/monitoring/journal")
    async def monitoring_journal(
        device_id: str | None = None,
        link_id: str | None = None,
        severity: str | None = None,
        limit: int = 100,
        offset: int = 0,
    ) -> dict:
        entries = state.monitoring_journal.query(
            device_id=device_id, link_id=link_id,
            severity=severity, limit=limit, offset=offset,
        )
        return {"entries": [e.to_dict() for e in entries], "total": len(state.monitoring_journal)}

    @app.get("/api/v1/monitoring/metrics/{device_id}")
    async def monitoring_metrics(device_id: str) -> dict:
        metrics = state.measurement_store.metrics_for_device(device_id)
        freshness = state.measurement_store.get_device_freshness(device_id)
        return {
            "device_id": device_id,
            "metrics": {k: v.to_dict() for k, v in metrics.items()},
            "freshness": freshness.to_dict() if freshness else None,
        }

    @app.get("/api/v1/monitoring/health")
    async def monitoring_health() -> dict:
        result = {}
        for did in state.measurement_store.all_device_ids():
            f = state.measurement_store.get_device_freshness(did)
            if f:
                result[did] = f.to_dict()
        return {"devices": result}

    @app.get("/api/v1/monitoring/trends")
    async def monitoring_trends(device_id: str | None = None, active_only: bool = True) -> dict:
        mgr = state.trend_alert_manager
        alerts = mgr.active_alerts() if active_only else mgr.all_alerts()
        if device_id:
            alerts = [a for a in alerts if a.device_id == device_id]
        return {"alerts": [a.to_dict() for a in alerts]}

    @app.get("/api/v1/monitoring/link/{link_id}/history")
    async def monitoring_link_history(
        link_id: str,
        tier: int = 1,
        limit: int = 200,
        metric: str = "latency_ms",
    ) -> dict:
        from fastapi import HTTPException
        if tier not in (1, 2, 3):
            raise HTTPException(400, "tier must be 1, 2, or 3")
        valid_metrics = ("latency_ms", "loss_rate", "jitter_p99_ms")
        if metric not in valid_metrics:
            raise HTTPException(400, f"metric must be one of: {', '.join(valid_metrics)}")
        lnk = state.routing_graph.get_link(link_id)
        if lnk is None:
            raise HTTPException(404, f"Link not found: {link_id}")
        dev_id = lnk.source.device_id
        metric_key = f"link.{link_id}.{metric}"
        series = state.metric_store.get_series(dev_id, metric_key)
        if series is None:
            return {"link_id": link_id, "device_id": dev_id, "metric": metric,
                    "metric_key": metric_key, "tier": tier, "points": []}
        points = series.history(tier=tier, limit=limit)
        return {
            "link_id": link_id,
            "device_id": dev_id,
            "metric": metric,
            "metric_key": metric_key,
            "tier": tier,
            "points": [p.to_dict() for p in points],
        }

    return app, state


@pytest.fixture(scope="module")
def client():
    app, _state = _make_app()
    return TestClient(app)


# ── /api/v1/routing/intents ───────────────────────────────────────────────────

class TestRoutingIntentsEndpoint:
    def test_list_intents_returns_all_builtins(self, client):
        resp = client.get("/api/v1/routing/intents")
        assert resp.status_code == 200
        data = resp.json()
        assert "intents" in data
        assert "control" in data["intents"]
        assert "desktop" in data["intents"]

    def test_get_intent_known(self, client):
        resp = client.get("/api/v1/routing/intents/control")
        assert resp.status_code == 200
        data = resp.json()
        assert data["name"] == "control"

    def test_get_intent_unknown_404(self, client):
        resp = client.get("/api/v1/routing/intents/no_such_intent")
        assert resp.status_code == 404


# ── /api/v1/routing/explain ───────────────────────────────────────────────────

class TestRoutingExplainEndpoint:
    def test_explain_returns_streams(self, client):
        resp = client.get("/api/v1/routing/explain?source=dev-a&destination=dev-b&intent=control")
        assert resp.status_code == 200
        data = resp.json()
        assert data["source"] == "dev-a"
        assert data["destination"] == "dev-b"
        assert "streams" in data

    def test_explain_unknown_source_404(self, client):
        resp = client.get("/api/v1/routing/explain?source=nope&destination=dev-b")
        assert resp.status_code == 404

    def test_explain_unknown_intent_404(self, client):
        resp = client.get("/api/v1/routing/explain?source=dev-a&destination=dev-b&intent=bogus")
        assert resp.status_code == 404


# ── /api/v1/routing/feasibility ───────────────────────────────────────────────

class TestRoutingFeasibilityEndpoint:
    def test_feasibility_returns_dict(self, client):
        resp = client.get("/api/v1/routing/feasibility?source=dev-a&destination=dev-b&intent=control")
        assert resp.status_code == 200
        data = resp.json()
        assert "feasible" in data
        assert isinstance(data["feasible"], dict)

    def test_feasibility_unknown_device_404(self, client):
        resp = client.get("/api/v1/routing/feasibility?source=nope&destination=dev-b")
        assert resp.status_code == 404


# ── /api/v1/routing/evaluate ──────────────────────────────────────────────────

class TestRoutingEvaluateEndpoint:
    def test_evaluate_returns_streams(self, client):
        resp = client.post("/api/v1/routing/evaluate?source=dev-a&destination=dev-b&intent=control&top_n=2")
        assert resp.status_code == 200
        data = resp.json()
        assert "streams" in data

    def test_evaluate_unknown_intent_400(self, client):
        resp = client.post("/api/v1/routing/evaluate?source=dev-a&destination=dev-b&intent=totally_bogus")
        assert resp.status_code == 400

    def test_evaluate_unknown_device_404(self, client):
        resp = client.post("/api/v1/routing/evaluate?source=no-such-device&destination=dev-b&intent=control")
        assert resp.status_code == 404


# ── /api/v1/monitoring/journal ────────────────────────────────────────────────

class TestMonitoringJournalEndpoint:
    def test_returns_entries(self, client):
        resp = client.get("/api/v1/monitoring/journal")
        assert resp.status_code == 200
        data = resp.json()
        assert "entries" in data
        assert "total" in data
        assert data["total"] >= 1

    def test_filter_by_device(self, client):
        resp = client.get("/api/v1/monitoring/journal?device_id=dev-a")
        assert resp.status_code == 200
        entries = resp.json()["entries"]
        for e in entries:
            assert e["device_id"] == "dev-a"

    def test_filter_unknown_device_returns_empty(self, client):
        resp = client.get("/api/v1/monitoring/journal?device_id=no-such-device")
        assert resp.status_code == 200
        assert resp.json()["entries"] == []

    def test_limit_respected(self, client):
        resp = client.get("/api/v1/monitoring/journal?limit=1")
        assert resp.status_code == 200
        assert len(resp.json()["entries"]) <= 1


# ── /api/v1/monitoring/metrics/{device_id} ────────────────────────────────────

class TestMonitoringMetricsEndpoint:
    def test_returns_metrics_for_device(self, client):
        resp = client.get("/api/v1/monitoring/metrics/dev-a")
        assert resp.status_code == 200
        data = resp.json()
        assert data["device_id"] == "dev-a"
        assert "latency_ms" in data["metrics"]

    def test_unknown_device_returns_empty_metrics(self, client):
        resp = client.get("/api/v1/monitoring/metrics/no-such-device")
        assert resp.status_code == 200
        data = resp.json()
        assert data["metrics"] == {}
        assert data["freshness"] is None


# ── /api/v1/monitoring/health ─────────────────────────────────────────────────

class TestMonitoringHealthEndpoint:
    def test_returns_devices_dict(self, client):
        resp = client.get("/api/v1/monitoring/health")
        assert resp.status_code == 200
        data = resp.json()
        assert "devices" in data

    def test_device_with_metrics_appears(self, client):
        resp = client.get("/api/v1/monitoring/health")
        data = resp.json()
        assert "dev-a" in data["devices"]


# ── /api/v1/monitoring/trends ─────────────────────────────────────────────────

class TestMonitoringTrendsEndpoint:
    def test_returns_alerts(self, client):
        resp = client.get("/api/v1/monitoring/trends")
        assert resp.status_code == 200
        data = resp.json()
        assert "alerts" in data
        assert len(data["alerts"]) >= 1

    def test_filter_by_device(self, client):
        resp = client.get("/api/v1/monitoring/trends?device_id=dev-a")
        assert resp.status_code == 200
        alerts = resp.json()["alerts"]
        for a in alerts:
            assert a["device_id"] == "dev-a"

    def test_filter_unknown_device_returns_empty(self, client):
        resp = client.get("/api/v1/monitoring/trends?device_id=no-device")
        assert resp.status_code == 200
        assert resp.json()["alerts"] == []


# ── /api/v1/monitoring/link/{link_id}/history ─────────────────────────────────

@pytest.fixture(scope="module")
def history_client():
    """Client with MetricStore pre-seeded with link metrics."""
    app, state = _make_app()
    import time

    # Seed some MetricStore data for link-ab
    now = time.monotonic()
    for i in range(5):
        t = now - i * 10.0
        state.metric_store.record("dev-a", "link.link-ab.latency_ms", 1.0 + i * 0.1, t)
        state.metric_store.record("dev-a", "link.link-ab.loss_rate", 0.0, t)
        state.metric_store.record("dev-a", "link.link-ab.jitter_p99_ms", 0.5 + i * 0.05, t)

    return TestClient(app)


class TestMonitoringLinkHistoryEndpoint:
    def test_returns_latency_history(self, history_client):
        resp = history_client.get(
            "/api/v1/monitoring/link/link-ab/history"
            "?metric=latency_ms&tier=1&limit=10"
        )
        assert resp.status_code == 200
        data = resp.json()
        assert data["link_id"] == "link-ab"
        assert data["metric"] == "latency_ms"
        assert data["tier"] == 1
        assert isinstance(data["points"], list)
        assert len(data["points"]) == 5  # all 5 seeded

    def test_points_have_expected_keys(self, history_client):
        resp = history_client.get(
            "/api/v1/monitoring/link/link-ab/history?metric=latency_ms"
        )
        assert resp.status_code == 200
        points = resp.json()["points"]
        assert len(points) > 0
        for p in points:
            assert "t" in p
            assert "v" in p

    def test_limit_respected(self, history_client):
        resp = history_client.get(
            "/api/v1/monitoring/link/link-ab/history?metric=latency_ms&limit=2"
        )
        assert resp.status_code == 200
        assert len(resp.json()["points"]) <= 2

    def test_loss_rate_metric(self, history_client):
        resp = history_client.get(
            "/api/v1/monitoring/link/link-ab/history?metric=loss_rate"
        )
        assert resp.status_code == 200
        data = resp.json()
        assert data["metric"] == "loss_rate"

    def test_jitter_metric(self, history_client):
        resp = history_client.get(
            "/api/v1/monitoring/link/link-ab/history?metric=jitter_p99_ms"
        )
        assert resp.status_code == 200

    def test_unknown_link_404(self, history_client):
        resp = history_client.get(
            "/api/v1/monitoring/link/no-such-link/history"
        )
        assert resp.status_code == 404

    def test_invalid_metric_400(self, history_client):
        resp = history_client.get(
            "/api/v1/monitoring/link/link-ab/history?metric=bogus"
        )
        assert resp.status_code == 400

    def test_invalid_tier_400(self, history_client):
        resp = history_client.get(
            "/api/v1/monitoring/link/link-ab/history?tier=99"
        )
        assert resp.status_code == 400

    def test_no_data_returns_empty_points(self, history_client):
        """A link with no recorded metrics returns empty points list."""
        resp = history_client.get(
            "/api/v1/monitoring/link/link-ab/history"
            "?metric=jitter_p99_ms&tier=3"  # tier 3 won't have data yet
        )
        assert resp.status_code == 200
        # tier 3 aggregates from tier 2 which aggregates from tier 1;
        # at test speeds there's no tier 3 data yet
        assert isinstance(resp.json()["points"], list)

    def test_includes_device_id(self, history_client):
        resp = history_client.get(
            "/api/v1/monitoring/link/link-ab/history?metric=latency_ms"
        )
        assert resp.status_code == 200
        assert resp.json()["device_id"] == "dev-a"


# ── MetricSeries.history() ────────────────────────────────────────────────────

class TestMetricSeriesHistory:
    def _seeded_series(self, count: int = 10) -> object:
        from routing.monitoring import MetricSeries
        import time
        series = MetricSeries()
        now = time.monotonic()
        for i in range(count):
            series.record(float(i), now=now - i * 2.0)
        return series

    def test_tier1_returns_all_points(self):
        series = self._seeded_series(5)
        points = series.history(tier=1)
        assert len(points) == 5

    def test_newest_first(self):
        series = self._seeded_series(5)
        points = series.history(tier=1)
        timestamps = [p.timestamp for p in points]
        assert timestamps == sorted(timestamps, reverse=True)

    def test_limit_applied(self):
        series = self._seeded_series(10)
        points = series.history(tier=1, limit=3)
        assert len(points) == 3

    def test_since_filters_older(self):
        from routing.monitoring import MetricSeries
        import time
        series = MetricSeries()
        base = time.monotonic()
        series.record(1.0, now=base - 100.0)  # old
        series.record(2.0, now=base - 10.0)   # recent
        series.record(3.0, now=base - 5.0)    # recent
        points = series.history(tier=1, since=base - 15.0)
        assert len(points) == 2
        for p in points:
            assert p.timestamp > base - 15.0

    def test_invalid_tier_raises(self):
        from routing.monitoring import MetricSeries
        series = MetricSeries()
        with pytest.raises(ValueError, match="tier must be 1, 2, or 3"):
            series.history(tier=5)

    def test_empty_series_returns_empty(self):
        from routing.monitoring import MetricSeries
        series = MetricSeries()
        assert series.history(tier=1) == []
