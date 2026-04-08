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
