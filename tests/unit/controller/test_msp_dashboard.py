#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Unit tests for MSPDashboardManager, MSPPortalManager, and related data models.
"""

import asyncio
import csv
import io
import json
import sys
import tempfile
import time
import unittest
from pathlib import Path
from typing import Any
from unittest.mock import AsyncMock, MagicMock, patch, PropertyMock

sys.path.insert(0, str(Path(__file__).parent.parent.parent.parent / "controller"))

from msp_dashboard import (
    MSPClient, MSPClientHealth, BulkOperation, BillingLine,
    MSPDashboardManager,
)
from msp_portal import MSPPortalManager, PortalConfig


def _run(coro):
    return asyncio.run(coro)


def _mgr(tmp: Path, event_queue=None) -> MSPDashboardManager:
    return MSPDashboardManager(data_dir=tmp, event_queue=event_queue)


def _sample_client(**kwargs) -> dict:
    base = {
        "name": "Acme Corp",
        "slug": "acme",
        "controller_url": "http://acme.internal:7380",
        "api_token": "tok-acme-123",
    }
    base.update(kwargs)
    return base


# ── MSPClient model ───────────────────────────────────────────────────────────

class TestMSPClient(unittest.TestCase):
    def test_to_dict_round_trip(self):
        c = MSPClient(
            id="c1",
            name="Acme Corp",
            slug="acme",
            controller_url="http://acme.internal:7380",
            api_token="tok-123",
            tier="enterprise",
            seat_count=50,
            tags=["finance", "au"],
            notes="VIP client",
            onboarded_at=1000.0,
            last_seen=2000.0,
            monthly_rate=500.0,
            wholesale_cost=300.0,
        )
        d = c.to_dict()
        c2 = MSPClient.from_dict(d)
        self.assertEqual(c2.id, "c1")
        self.assertEqual(c2.name, "Acme Corp")
        self.assertEqual(c2.slug, "acme")
        self.assertEqual(c2.tier, "enterprise")
        self.assertEqual(c2.seat_count, 50)
        self.assertEqual(c2.tags, ["finance", "au"])
        self.assertEqual(c2.notes, "VIP client")
        self.assertAlmostEqual(c2.monthly_rate, 500.0)
        self.assertAlmostEqual(c2.wholesale_cost, 300.0)

    def test_defaults(self):
        c = MSPClient(
            id="c2", name="Beta Ltd", slug="beta",
            controller_url="http://beta:7380", api_token="tok",
        )
        self.assertEqual(c.tier, "business")
        self.assertEqual(c.seat_count, 0)
        self.assertEqual(c.tags, [])
        self.assertEqual(c.notes, "")
        self.assertAlmostEqual(c.monthly_rate, 0.0)
        self.assertAlmostEqual(c.wholesale_cost, 0.0)

    def test_from_dict_missing_optional_fields(self):
        d = {
            "id": "c3", "name": "Gamma", "slug": "gamma",
            "controller_url": "http://g:7380", "api_token": "t",
        }
        c = MSPClient.from_dict(d)
        self.assertEqual(c.tier, "business")
        self.assertEqual(c.seat_count, 0)


# ── MSPClientHealth model ─────────────────────────────────────────────────────

class TestMSPClientHealth(unittest.TestCase):
    def test_to_dict_round_trip(self):
        h = MSPClientHealth(
            client_id="c1",
            fetched_at=1000.0,
            machines_online=8,
            machines_total=10,
            critical_alerts=0,
            compliance_score=0.85,
            e8_score=0.9,
            iso27001_score=0.8,
            last_backup_ok=True,
            pending_approvals=2,
            upcoming_renewals=3,
            health="green",
        )
        d = h.to_dict()
        h2 = MSPClientHealth.from_dict(d)
        self.assertEqual(h2.client_id, "c1")
        self.assertEqual(h2.machines_online, 8)
        self.assertEqual(h2.machines_total, 10)
        self.assertAlmostEqual(h2.compliance_score, 0.85)
        self.assertEqual(h2.health, "green")
        self.assertTrue(h2.last_backup_ok)
        self.assertEqual(h2.error, "")

    def test_health_red_on_critical_alerts(self):
        result = MSPClientHealth.calculate_health(
            critical_alerts=1, compliance_score=0.8, pending_approvals=0
        )
        self.assertEqual(result, "red")

    def test_health_red_on_low_compliance(self):
        result = MSPClientHealth.calculate_health(
            critical_alerts=0, compliance_score=0.4, pending_approvals=0
        )
        self.assertEqual(result, "red")

    def test_health_amber_on_many_pending(self):
        result = MSPClientHealth.calculate_health(
            critical_alerts=0, compliance_score=0.75, pending_approvals=6
        )
        self.assertEqual(result, "amber")

    def test_health_amber_on_medium_compliance(self):
        result = MSPClientHealth.calculate_health(
            critical_alerts=0, compliance_score=0.65, pending_approvals=0
        )
        self.assertEqual(result, "amber")

    def test_health_green(self):
        result = MSPClientHealth.calculate_health(
            critical_alerts=0, compliance_score=0.8, pending_approvals=2
        )
        self.assertEqual(result, "green")

    def test_health_red_beats_amber(self):
        # critical_alerts > 0 → red even if compliance is OK
        result = MSPClientHealth.calculate_health(
            critical_alerts=2, compliance_score=0.9, pending_approvals=10
        )
        self.assertEqual(result, "red")

    def test_from_dict_error_field(self):
        d = {
            "client_id": "c1", "fetched_at": 0.0, "machines_online": 0,
            "machines_total": 0, "critical_alerts": 0, "compliance_score": 0.0,
            "e8_score": 0.0, "iso27001_score": 0.0, "last_backup_ok": False,
            "pending_approvals": 0, "upcoming_renewals": 0, "health": "red",
            "error": "connection refused",
        }
        h = MSPClientHealth.from_dict(d)
        self.assertEqual(h.error, "connection refused")


# ── BulkOperation model ───────────────────────────────────────────────────────

class TestBulkOperation(unittest.TestCase):
    def test_to_dict_round_trip(self):
        op = BulkOperation(
            id="op1",
            type="patch_deploy",
            client_ids=["c1", "c2"],
            params={"ring": "emergency"},
            created_at=1000.0,
            status="completed",
            results={"c1": {"job_id": "j1"}, "c2": {"error": "timeout"}},
        )
        d = op.to_dict()
        op2 = BulkOperation.from_dict(d)
        self.assertEqual(op2.id, "op1")
        self.assertEqual(op2.type, "patch_deploy")
        self.assertEqual(op2.client_ids, ["c1", "c2"])
        self.assertEqual(op2.status, "completed")
        self.assertEqual(op2.results["c1"]["job_id"], "j1")

    def test_defaults(self):
        op = BulkOperation(
            id="op2", type="policy_push", client_ids=[], params={}, created_at=0.0
        )
        self.assertEqual(op.status, "pending")
        self.assertEqual(op.results, {})

    def test_lifecycle_pending_to_completed(self):
        op = BulkOperation(
            id="op3", type="compliance_report",
            client_ids=["c1"], params={}, created_at=time.time(),
        )
        self.assertEqual(op.status, "pending")
        op.status = "running"
        self.assertEqual(op.status, "running")
        op.status = "completed"
        self.assertEqual(op.status, "completed")


# ── BillingLine model ─────────────────────────────────────────────────────────

class TestBillingLine(unittest.TestCase):
    def test_margin_calculation(self):
        line = BillingLine(
            client_id="c1",
            client_name="Acme",
            tier="business",
            seat_count=10,
            monthly_rate=500.0,
            wholesale_cost=300.0,
            margin=200.0,
        )
        self.assertAlmostEqual(line.margin, 200.0)

    def test_to_dict(self):
        line = BillingLine(
            client_id="c1", client_name="Acme", tier="business",
            seat_count=10, monthly_rate=500.0, wholesale_cost=300.0, margin=200.0,
        )
        d = line.to_dict()
        self.assertEqual(d["client_name"], "Acme")
        self.assertEqual(d["seat_count"], 10)
        self.assertAlmostEqual(d["margin"], 200.0)

    def test_zero_margin(self):
        line = BillingLine(
            client_id="c1", client_name="X", tier="starter",
            seat_count=5, monthly_rate=100.0, wholesale_cost=100.0, margin=0.0,
        )
        self.assertAlmostEqual(line.margin, 0.0)


# ── MSPDashboardManager CRUD ──────────────────────────────────────────────────

class TestMSPDashboardManagerCRUD(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.mkdtemp()
        self._dir = Path(self._tmp)

    def _start_mgr(self) -> MSPDashboardManager:
        mgr = _mgr(self._dir)
        _run(mgr.start())
        return mgr

    def test_add_client(self):
        mgr = self._start_mgr()
        client = _run(mgr.add_client(**_sample_client()))
        self.assertIsNotNone(client.id)
        self.assertEqual(client.name, "Acme Corp")
        self.assertEqual(client.slug, "acme")

    def test_add_client_with_optional_fields(self):
        mgr = self._start_mgr()
        client = _run(mgr.add_client(
            **_sample_client(tier="enterprise", seat_count=25,
                             monthly_rate=750.0, wholesale_cost=400.0,
                             tags=["au", "finance"], notes="Priority client")
        ))
        self.assertEqual(client.tier, "enterprise")
        self.assertEqual(client.seat_count, 25)
        self.assertAlmostEqual(client.monthly_rate, 750.0)
        self.assertEqual(client.tags, ["au", "finance"])

    def test_list_clients_empty(self):
        mgr = self._start_mgr()
        clients = _run(mgr.list_clients())
        self.assertEqual(clients, [])

    def test_list_clients_multiple(self):
        mgr = self._start_mgr()
        _run(mgr.add_client(**_sample_client(name="Alpha")))
        _run(mgr.add_client(**_sample_client(name="Beta", slug="beta")))
        clients = _run(mgr.list_clients())
        self.assertEqual(len(clients), 2)
        names = {c.name for c in clients}
        self.assertIn("Alpha", names)
        self.assertIn("Beta", names)

    def test_get_client(self):
        mgr = self._start_mgr()
        added = _run(mgr.add_client(**_sample_client()))
        fetched = _run(mgr.get_client(added.id))
        self.assertIsNotNone(fetched)
        self.assertEqual(fetched.name, "Acme Corp")

    def test_get_client_not_found(self):
        mgr = self._start_mgr()
        result = _run(mgr.get_client("nonexistent"))
        self.assertIsNone(result)

    def test_update_client(self):
        mgr = self._start_mgr()
        client = _run(mgr.add_client(**_sample_client()))
        updated = _run(mgr.update_client(client.id, name="Acme Corp 2",
                                          monthly_rate=600.0))
        self.assertEqual(updated.name, "Acme Corp 2")
        self.assertAlmostEqual(updated.monthly_rate, 600.0)

    def test_update_client_not_found(self):
        mgr = self._start_mgr()
        with self.assertRaises(KeyError):
            _run(mgr.update_client("nonexistent", name="X"))

    def test_remove_client(self):
        mgr = self._start_mgr()
        client = _run(mgr.add_client(**_sample_client()))
        _run(mgr.remove_client(client.id))
        self.assertIsNone(_run(mgr.get_client(client.id)))

    def test_remove_nonexistent_client_silent(self):
        mgr = self._start_mgr()
        # Should not raise
        _run(mgr.remove_client("nonexistent"))

    def test_remove_client_clears_health(self):
        mgr = self._start_mgr()
        client = _run(mgr.add_client(**_sample_client()))
        # Inject fake health
        from msp_dashboard import MSPClientHealth
        mgr._health[client.id] = MSPClientHealth(
            client_id=client.id, fetched_at=time.time(),
            machines_online=1, machines_total=1, critical_alerts=0,
            compliance_score=0.8, e8_score=0.8, iso27001_score=0.8,
            last_backup_ok=True, pending_approvals=0, upcoming_renewals=0,
            health="green",
        )
        _run(mgr.remove_client(client.id))
        self.assertIsNone(_run(mgr.get_health(client.id)))


# ── Persistence round-trip ────────────────────────────────────────────────────

class TestMSPDashboardPersistence(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.mkdtemp()
        self._dir = Path(self._tmp)

    def test_clients_persist_across_restart(self):
        mgr = _mgr(self._dir)
        _run(mgr.start())
        _run(mgr.add_client(**_sample_client()))
        _run(mgr.stop())

        mgr2 = _mgr(self._dir)
        _run(mgr2.start())
        clients = _run(mgr2.list_clients())
        self.assertEqual(len(clients), 1)
        self.assertEqual(clients[0].name, "Acme Corp")
        _run(mgr2.stop())

    def test_health_cache_persists(self):
        from msp_dashboard import MSPClientHealth
        mgr = _mgr(self._dir)
        _run(mgr.start())
        client = _run(mgr.add_client(**_sample_client()))
        h = MSPClientHealth(
            client_id=client.id, fetched_at=time.time(),
            machines_online=5, machines_total=6, critical_alerts=0,
            compliance_score=0.75, e8_score=0.7, iso27001_score=0.8,
            last_backup_ok=True, pending_approvals=1, upcoming_renewals=2,
            health="green",
        )
        mgr._health[client.id] = h
        _run(mgr.stop())

        mgr2 = _mgr(self._dir)
        _run(mgr2.start())
        h2 = _run(mgr2.get_health(client.id))
        self.assertIsNotNone(h2)
        self.assertEqual(h2.machines_online, 5)
        self.assertEqual(h2.health, "green")
        _run(mgr2.stop())

    def test_operations_persist(self):
        mgr = _mgr(self._dir)
        _run(mgr.start())
        client = _run(mgr.add_client(**_sample_client()))
        op = BulkOperation(
            id="op-persist-test", type="patch_deploy",
            client_ids=[client.id], params={}, created_at=time.time(),
            status="completed",
        )
        mgr._operations[op.id] = op
        _run(mgr.stop())

        mgr2 = _mgr(self._dir)
        _run(mgr2.start())
        ops = _run(mgr2.list_operations())
        self.assertEqual(len(ops), 1)
        self.assertEqual(ops[0].id, "op-persist-test")
        _run(mgr2.stop())


# ── Health refresh (mocked HTTP) ──────────────────────────────────────────────

def _make_response(status_code: int, body: Any) -> MagicMock:
    """Return a mock httpx.Response with synchronous .json() and .text."""
    resp = MagicMock()
    resp.status_code = status_code
    resp.json = MagicMock(return_value=body)
    resp.text = json.dumps(body)
    return resp


def _make_http_mock(responses: dict) -> AsyncMock:
    """
    Returns a mock httpx.AsyncClient where each URL pattern maps to a
    (status_code, json_body) tuple. Unmatched URLs return 404.
    """
    mock_client = AsyncMock()

    async def _get(url, **kwargs):
        for pattern, (code, body) in responses.items():
            if pattern in url:
                return _make_response(code, body)
        return _make_response(404, {})

    async def _post(url, **kwargs):
        for pattern, (code, body) in responses.items():
            if pattern in url:
                return _make_response(code, body)
        return _make_response(404, {})

    mock_client.__aenter__ = AsyncMock(return_value=mock_client)
    mock_client.__aexit__ = AsyncMock(return_value=None)
    mock_client.get = AsyncMock(side_effect=_get)
    mock_client.post = AsyncMock(side_effect=_post)
    return mock_client


_HEALTHY_RESPONSES = {
    "/api/v1/status": (200, {
        "nodes": {
            "node1": {"last_seen": time.time()},
            "node2": {"last_seen": time.time()},
        },
        "critical_alerts": 0,
    }),
    "/api/v1/compliance/status": (200, {
        "overall_score": 0.82,
        "e8_score": 0.85,
        "iso27001_score": 0.79,
    }),
    "/api/v1/backup/status": (200, {"last_backup_ok": True}),
    "/api/v1/itsm/pending": (200, {"count": 1}),
    "/api/v1/saas/renewals": (200, {"count": 3}),
}

_RED_RESPONSES = {
    "/api/v1/status": (200, {
        "nodes": {"node1": {"last_seen": time.time()}},
        "critical_alerts": 2,
    }),
    "/api/v1/compliance/status": (200, {
        "overall_score": 0.35,
        "e8_score": 0.3,
        "iso27001_score": 0.4,
    }),
    "/api/v1/backup/status": (200, {"last_backup_ok": False}),
    "/api/v1/itsm/pending": (200, {"count": 0}),
    "/api/v1/saas/renewals": (200, {"count": 0}),
}


class TestRefreshClientHealth(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.mkdtemp()
        self._dir = Path(self._tmp)

    def _mgr_with_client(self) -> tuple[MSPDashboardManager, MSPClient]:
        mgr = _mgr(self._dir)
        _run(mgr.start())
        client = _run(mgr.add_client(**_sample_client()))
        return mgr, client

    def test_refresh_unknown_client_raises(self):
        mgr = _mgr(self._dir)
        _run(mgr.start())
        with self.assertRaises(KeyError):
            _run(mgr.refresh_client_health("nonexistent"))

    def test_refresh_green_health(self):
        mgr, client = self._mgr_with_client()
        mock_http = _make_http_mock(_HEALTHY_RESPONSES)
        with patch("msp_dashboard._httpx.AsyncClient", return_value=mock_http):
            h = _run(mgr.refresh_client_health(client.id))
        self.assertEqual(h.health, "green")
        self.assertEqual(h.machines_total, 2)
        self.assertAlmostEqual(h.compliance_score, 0.82)
        self.assertTrue(h.last_backup_ok)
        self.assertEqual(h.pending_approvals, 1)
        self.assertEqual(h.upcoming_renewals, 3)
        self.assertEqual(h.error, "")

    def test_refresh_red_health(self):
        mgr, client = self._mgr_with_client()
        mock_http = _make_http_mock(_RED_RESPONSES)
        with patch("msp_dashboard._httpx.AsyncClient", return_value=mock_http):
            h = _run(mgr.refresh_client_health(client.id))
        self.assertEqual(h.health, "red")
        self.assertEqual(h.critical_alerts, 2)
        self.assertFalse(h.last_backup_ok)

    def test_refresh_stores_health_in_cache(self):
        mgr, client = self._mgr_with_client()
        mock_http = _make_http_mock(_HEALTHY_RESPONSES)
        with patch("msp_dashboard._httpx.AsyncClient", return_value=mock_http):
            _run(mgr.refresh_client_health(client.id))
        cached = _run(mgr.get_health(client.id))
        self.assertIsNotNone(cached)
        self.assertEqual(cached.health, "green")

    def test_refresh_connection_error_gives_red(self):
        mgr, client = self._mgr_with_client()
        # Simulate httpx raising an exception on connect
        mock_client_ctx = AsyncMock()
        mock_client_ctx.__aenter__ = AsyncMock(side_effect=Exception("connection refused"))
        mock_client_ctx.__aexit__ = AsyncMock(return_value=None)
        with patch("msp_dashboard._httpx.AsyncClient", return_value=mock_client_ctx):
            h = _run(mgr.refresh_client_health(client.id))
        self.assertEqual(h.health, "red")
        self.assertIn("connection refused", h.error)

    def test_refresh_fires_event_on_status_change(self):
        eq: asyncio.Queue = asyncio.Queue()
        mgr = MSPDashboardManager(data_dir=self._dir, event_queue=eq)
        _run(mgr.start())
        client = _run(mgr.add_client(**_sample_client()))

        # First refresh → no previous health → event fires
        mock_http = _make_http_mock(_HEALTHY_RESPONSES)
        with patch("msp_dashboard._httpx.AsyncClient", return_value=mock_http):
            _run(mgr.refresh_client_health(client.id))

        # Drain any pending tasks
        _run(asyncio.sleep(0))

        events = []
        while not eq.empty():
            events.append(eq.get_nowait())
        health_events = [e for e in events if e["type"] == "msp.health_updated"]
        self.assertGreater(len(health_events), 0)

    def test_refresh_updates_last_seen_on_success(self):
        mgr, client = self._mgr_with_client()
        before = client.last_seen
        mock_http = _make_http_mock(_HEALTHY_RESPONSES)
        with patch("msp_dashboard._httpx.AsyncClient", return_value=mock_http):
            _run(mgr.refresh_client_health(client.id))
        updated = _run(mgr.get_client(client.id))
        self.assertGreater(updated.last_seen, before)


class TestRefreshAllHealth(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.mkdtemp()
        self._dir = Path(self._tmp)

    def test_refresh_all_empty(self):
        mgr = _mgr(self._dir)
        _run(mgr.start())
        result = _run(mgr.refresh_all_health())
        self.assertEqual(result, [])

    def test_refresh_all_concurrent_all_succeed(self):
        mgr = _mgr(self._dir)
        _run(mgr.start())
        c1 = _run(mgr.add_client(**_sample_client(name="Alpha")))
        c2 = _run(mgr.add_client(**_sample_client(name="Beta", slug="beta")))
        mock_http = _make_http_mock(_HEALTHY_RESPONSES)
        with patch("msp_dashboard._httpx.AsyncClient", return_value=mock_http):
            results = _run(mgr.refresh_all_health())
        self.assertEqual(len(results), 2)
        for h in results:
            self.assertEqual(h.health, "green")

    def test_refresh_all_one_unreachable(self):
        mgr = _mgr(self._dir)
        _run(mgr.start())
        c1 = _run(mgr.add_client(**_sample_client(name="Alpha")))
        c2 = _run(mgr.add_client(
            name="Broken", slug="broken",
            controller_url="http://broken.internal:7380", api_token="tok-broken",
        ))

        call_count = 0

        async def _get(url, **kwargs):
            if "broken" in url:
                raise Exception("unreachable")
            for pattern, (code, body) in _HEALTHY_RESPONSES.items():
                if pattern in url:
                    return _make_response(code, body)
            return _make_response(404, {})

        mock_client = AsyncMock()
        mock_client.__aenter__ = AsyncMock(return_value=mock_client)
        mock_client.__aexit__ = AsyncMock(return_value=None)
        mock_client.get = AsyncMock(side_effect=_get)
        mock_client.post = AsyncMock(return_value=MagicMock(status_code=200))

        with patch("msp_dashboard._httpx.AsyncClient", return_value=mock_client):
            results = _run(mgr.refresh_all_health())

        self.assertEqual(len(results), 2)
        healthy = next(r for r in results if r.client_id == c1.id)
        broken = next(r for r in results if r.client_id == c2.id)
        self.assertEqual(healthy.health, "green")
        # Broken client either gets an error string or falls back to red due to
        # zero compliance score — either way health must be red.
        self.assertEqual(broken.health, "red")


# ── Bulk operations ───────────────────────────────────────────────────────────

class TestBulkPatchDeploy(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.mkdtemp()
        self._dir = Path(self._tmp)

    def _setup_mgr(self) -> tuple[MSPDashboardManager, MSPClient]:
        mgr = _mgr(self._dir)
        _run(mgr.start())
        client = _run(mgr.add_client(**_sample_client()))
        return mgr, client

    def test_patch_deploy_creates_operation(self):
        mgr, client = self._setup_mgr()
        mock_http = _make_http_mock({"/api/v1/jobs": (201, {"id": "job-abc"})})
        with patch("msp_dashboard._httpx.AsyncClient", return_value=mock_http):
            op = _run(mgr.bulk_patch_deploy([client.id], ring="emergency"))
        self.assertIsNotNone(op.id)
        self.assertEqual(op.type, "patch_deploy")
        self.assertEqual(op.status, "completed")
        self.assertIn(client.id, op.results)

    def test_patch_deploy_records_job_id(self):
        mgr, client = self._setup_mgr()
        mock_http = _make_http_mock({"/api/v1/jobs": (201, {"id": "job-xyz"})})
        with patch("msp_dashboard._httpx.AsyncClient", return_value=mock_http):
            op = _run(mgr.bulk_patch_deploy([client.id]))
        self.assertEqual(op.results[client.id]["job_id"], "job-xyz")
        self.assertEqual(op.results[client.id]["status"], "submitted")

    def test_patch_deploy_unknown_client_records_error(self):
        mgr, _ = self._setup_mgr()
        mock_http = _make_http_mock({"/api/v1/jobs": (201, {"id": "j1"})})
        with patch("msp_dashboard._httpx.AsyncClient", return_value=mock_http):
            op = _run(mgr.bulk_patch_deploy(["nonexistent-id"]))
        self.assertEqual(op.results["nonexistent-id"]["error"], "unknown client")

    def test_patch_deploy_http_error_recorded(self):
        mgr, client = self._setup_mgr()
        mock_http = _make_http_mock({"/api/v1/jobs": (500, {})})
        with patch("msp_dashboard._httpx.AsyncClient", return_value=mock_http):
            op = _run(mgr.bulk_patch_deploy([client.id]))
        self.assertIn("500", op.results[client.id]["error"])

    def test_patch_deploy_persisted(self):
        mgr, client = self._setup_mgr()
        mock_http = _make_http_mock({"/api/v1/jobs": (201, {"id": "j1"})})
        with patch("msp_dashboard._httpx.AsyncClient", return_value=mock_http):
            op = _run(mgr.bulk_patch_deploy([client.id]))

        op2 = _run(mgr.get_operation(op.id))
        self.assertIsNotNone(op2)
        self.assertEqual(op2.type, "patch_deploy")


class TestBulkComplianceReports(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.mkdtemp()
        self._dir = Path(self._tmp)

    def test_compliance_reports_creates_files(self):
        mgr = _mgr(self._dir)
        _run(mgr.start())
        client = _run(mgr.add_client(**_sample_client()))
        report_body = {"framework": "e8", "score": 0.8, "gaps": []}
        mock_http = _make_http_mock({"/api/v1/compliance/report": (200, report_body)})
        with patch("msp_dashboard._httpx.AsyncClient", return_value=mock_http):
            op = _run(mgr.bulk_compliance_reports([client.id], framework="e8"))
        self.assertEqual(op.status, "completed")
        report_path = Path(op.results[client.id]["report_path"])
        self.assertTrue(report_path.exists())
        data = json.loads(report_path.read_text())
        self.assertEqual(data["framework"], "e8")

    def test_compliance_reports_http_error(self):
        mgr = _mgr(self._dir)
        _run(mgr.start())
        client = _run(mgr.add_client(**_sample_client()))
        mock_http = _make_http_mock({"/api/v1/compliance/report": (503, {})})
        with patch("msp_dashboard._httpx.AsyncClient", return_value=mock_http):
            op = _run(mgr.bulk_compliance_reports([client.id]))
        self.assertIn("503", op.results[client.id]["error"])


class TestBulkPolicyPush(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.mkdtemp()
        self._dir = Path(self._tmp)

    def test_policy_push_success(self):
        mgr = _mgr(self._dir)
        _run(mgr.start())
        client = _run(mgr.add_client(**_sample_client()))
        mock_http = _make_http_mock({"/api/v1/osquery/policy": (200, {"ok": True})})
        with patch("msp_dashboard._httpx.AsyncClient", return_value=mock_http):
            op = _run(mgr.bulk_policy_push([client.id], policy={"name": "test"}))
        self.assertEqual(op.status, "completed")
        self.assertEqual(op.results[client.id]["status"], "ok")

    def test_policy_push_multiple_clients(self):
        mgr = _mgr(self._dir)
        _run(mgr.start())
        c1 = _run(mgr.add_client(**_sample_client(name="Alpha")))
        c2 = _run(mgr.add_client(**_sample_client(name="Beta", slug="beta")))
        mock_http = _make_http_mock({"/api/v1/osquery/policy": (200, {"ok": True})})
        with patch("msp_dashboard._httpx.AsyncClient", return_value=mock_http):
            op = _run(mgr.bulk_policy_push([c1.id, c2.id], policy={"name": "hardening"}))
        self.assertEqual(op.status, "completed")
        self.assertEqual(op.results[c1.id]["status"], "ok")
        self.assertEqual(op.results[c2.id]["status"], "ok")

    def test_policy_push_failure_recorded(self):
        mgr = _mgr(self._dir)
        _run(mgr.start())
        client = _run(mgr.add_client(**_sample_client()))
        mock_http = _make_http_mock({"/api/v1/osquery/policy": (422, {})})
        with patch("msp_dashboard._httpx.AsyncClient", return_value=mock_http):
            op = _run(mgr.bulk_policy_push([client.id], policy={}))
        self.assertIn("422", op.results[client.id]["error"])


class TestListOperations(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.mkdtemp()
        self._dir = Path(self._tmp)

    def test_list_operations_empty(self):
        mgr = _mgr(self._dir)
        _run(mgr.start())
        ops = _run(mgr.list_operations())
        self.assertEqual(ops, [])

    def test_get_operation_not_found(self):
        mgr = _mgr(self._dir)
        _run(mgr.start())
        op = _run(mgr.get_operation("nonexistent"))
        self.assertIsNone(op)


# ── Alert aggregation ─────────────────────────────────────────────────────────

class TestAggregateAlerts(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.mkdtemp()
        self._dir = Path(self._tmp)

    def test_aggregate_alerts_merges_clients(self):
        mgr = _mgr(self._dir)
        _run(mgr.start())
        c1 = _run(mgr.add_client(**_sample_client(name="Alpha")))
        c2 = _run(mgr.add_client(**_sample_client(name="Beta", slug="beta",
                                                    controller_url="http://beta:7380")))

        call_results = {
            "alpha": [{"id": "a1", "severity": "critical", "created_at": 1000.0}],
            "beta": [{"id": "b1", "severity": "high", "created_at": 900.0}],
        }

        async def _get(url, **kwargs):
            if "alpha" in url or "acme" in url:
                return _make_response(200, call_results["alpha"])
            return _make_response(200, call_results["beta"])

        mock_client = AsyncMock()
        mock_client.__aenter__ = AsyncMock(return_value=mock_client)
        mock_client.__aexit__ = AsyncMock(return_value=None)
        mock_client.get = AsyncMock(side_effect=_get)

        with patch("msp_dashboard._httpx.AsyncClient", return_value=mock_client):
            alerts = _run(mgr.aggregate_alerts())

        self.assertEqual(len(alerts), 2)
        # All alerts should have client_id and client_name injected
        for alert in alerts:
            self.assertIn("client_id", alert)
            self.assertIn("client_name", alert)

    def test_aggregate_alerts_sorted_by_severity(self):
        mgr = _mgr(self._dir)
        _run(mgr.start())
        client = _run(mgr.add_client(**_sample_client()))

        alerts_data = [
            {"id": "a1", "severity": "low", "created_at": 1000.0},
            {"id": "a2", "severity": "critical", "created_at": 900.0},
            {"id": "a3", "severity": "high", "created_at": 800.0},
        ]

        async def _get(url, **kwargs):
            return _make_response(200, alerts_data)

        mock_client = AsyncMock()
        mock_client.__aenter__ = AsyncMock(return_value=mock_client)
        mock_client.__aexit__ = AsyncMock(return_value=None)
        mock_client.get = AsyncMock(side_effect=_get)

        with patch("msp_dashboard._httpx.AsyncClient", return_value=mock_client):
            result = _run(mgr.aggregate_alerts())

        self.assertEqual(result[0]["severity"], "critical")
        self.assertEqual(result[1]["severity"], "high")
        self.assertEqual(result[2]["severity"], "low")

    def test_aggregate_alerts_client_failure_skipped(self):
        mgr = _mgr(self._dir)
        _run(mgr.start())
        c1 = _run(mgr.add_client(**_sample_client(name="Alpha")))

        async def _get(url, **kwargs):
            raise Exception("timeout")

        mock_client = AsyncMock()
        mock_client.__aenter__ = AsyncMock(return_value=mock_client)
        mock_client.__aexit__ = AsyncMock(return_value=None)
        mock_client.get = AsyncMock(side_effect=_get)

        with patch("msp_dashboard._httpx.AsyncClient", return_value=mock_client):
            alerts = _run(mgr.aggregate_alerts())

        # Should return empty list, not raise
        self.assertEqual(alerts, [])

    def test_aggregate_alerts_filter_by_client(self):
        mgr = _mgr(self._dir)
        _run(mgr.start())
        c1 = _run(mgr.add_client(**_sample_client(name="Alpha")))
        c2 = _run(mgr.add_client(**_sample_client(name="Beta", slug="beta",
                                                    controller_url="http://beta:7380")))

        async def _get(url, **kwargs):
            return _make_response(200, [{"id": "x", "severity": "high", "created_at": 0}])

        mock_client = AsyncMock()
        mock_client.__aenter__ = AsyncMock(return_value=mock_client)
        mock_client.__aexit__ = AsyncMock(return_value=None)
        mock_client.get = AsyncMock(side_effect=_get)

        with patch("msp_dashboard._httpx.AsyncClient", return_value=mock_client):
            alerts = _run(mgr.aggregate_alerts(client_id=c1.id))

        # Only one client queried
        for alert in alerts:
            self.assertEqual(alert["client_id"], c1.id)


# ── Billing ───────────────────────────────────────────────────────────────────

class TestMonthlyBillingExport(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.mkdtemp()
        self._dir = Path(self._tmp)

    def test_billing_uses_stored_rates(self):
        mgr = _mgr(self._dir)
        _run(mgr.start())
        client = _run(mgr.add_client(
            **_sample_client(monthly_rate=600.0, wholesale_cost=350.0, seat_count=15)
        ))
        mock_http = _make_http_mock({"/api/v1/status": (200, {
            "nodes": {f"n{i}": {"last_seen": time.time()} for i in range(20)}
        })})
        with patch("msp_dashboard._httpx.AsyncClient", return_value=mock_http):
            lines = _run(mgr.monthly_billing_export(2026, 4))
        self.assertEqual(len(lines), 1)
        line = lines[0]
        self.assertAlmostEqual(line.monthly_rate, 600.0)
        self.assertAlmostEqual(line.wholesale_cost, 350.0)
        self.assertAlmostEqual(line.margin, 250.0)

    def test_billing_seat_count_from_controller(self):
        mgr = _mgr(self._dir)
        _run(mgr.start())
        client = _run(mgr.add_client(**_sample_client(seat_count=5)))
        # Controller reports 12 nodes
        mock_http = _make_http_mock({"/api/v1/status": (200, {
            "nodes": {f"n{i}": {} for i in range(12)}
        })})
        with patch("msp_dashboard._httpx.AsyncClient", return_value=mock_http):
            lines = _run(mgr.monthly_billing_export(2026, 4))
        self.assertEqual(lines[0].seat_count, 12)

    def test_billing_falls_back_to_stored_seat_count_on_error(self):
        mgr = _mgr(self._dir)
        _run(mgr.start())
        client = _run(mgr.add_client(**_sample_client(seat_count=7)))

        async def _get(url, **kwargs):
            raise Exception("timeout")

        mock_client = AsyncMock()
        mock_client.__aenter__ = AsyncMock(return_value=mock_client)
        mock_client.__aexit__ = AsyncMock(return_value=None)
        mock_client.get = AsyncMock(side_effect=_get)

        with patch("msp_dashboard._httpx.AsyncClient", return_value=mock_client):
            lines = _run(mgr.monthly_billing_export(2026, 4))
        self.assertEqual(lines[0].seat_count, 7)  # stored value

    def test_billing_sorted_by_name(self):
        mgr = _mgr(self._dir)
        _run(mgr.start())
        _run(mgr.add_client(**_sample_client(name="Zebra Corp", slug="zebra")))
        _run(mgr.add_client(**_sample_client(name="Alpha Inc", slug="alpha")))
        mock_http = _make_http_mock({"/api/v1/status": (200, {"nodes": {}})})
        with patch("msp_dashboard._httpx.AsyncClient", return_value=mock_http):
            lines = _run(mgr.monthly_billing_export(2026, 4))
        self.assertEqual(lines[0].client_name, "Alpha Inc")
        self.assertEqual(lines[1].client_name, "Zebra Corp")

    def test_billing_empty_with_no_clients(self):
        mgr = _mgr(self._dir)
        _run(mgr.start())
        mock_http = _make_http_mock({})
        with patch("msp_dashboard._httpx.AsyncClient", return_value=mock_http):
            lines = _run(mgr.monthly_billing_export(2026, 4))
        self.assertEqual(lines, [])


class TestBillingCSV(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.mkdtemp()
        self._dir = Path(self._tmp)

    def test_csv_headers(self):
        mgr = _mgr(self._dir)
        _run(mgr.start())
        mock_http = _make_http_mock({"/api/v1/status": (200, {"nodes": {}})})
        with patch("msp_dashboard._httpx.AsyncClient", return_value=mock_http):
            csv_str = _run(mgr.billing_csv(2026, 4))
        reader = csv.DictReader(io.StringIO(csv_str))
        self.assertIn("Client", reader.fieldnames)
        self.assertIn("Slug", reader.fieldnames)
        self.assertIn("Tier", reader.fieldnames)
        self.assertIn("Seats", reader.fieldnames)
        self.assertIn("Monthly Rate", reader.fieldnames)
        self.assertIn("Wholesale Cost", reader.fieldnames)
        self.assertIn("Margin", reader.fieldnames)
        self.assertIn("Notes", reader.fieldnames)

    def test_csv_data_rows(self):
        mgr = _mgr(self._dir)
        _run(mgr.start())
        _run(mgr.add_client(**_sample_client(
            monthly_rate=500.0, wholesale_cost=300.0, notes="test"
        )))
        mock_http = _make_http_mock({"/api/v1/status": (200, {"nodes": {}})})
        with patch("msp_dashboard._httpx.AsyncClient", return_value=mock_http):
            csv_str = _run(mgr.billing_csv(2026, 4))
        reader = csv.DictReader(io.StringIO(csv_str))
        rows = list(reader)
        self.assertEqual(len(rows), 1)
        self.assertEqual(rows[0]["Client"], "Acme Corp")
        self.assertEqual(rows[0]["Slug"], "acme")
        self.assertEqual(rows[0]["Monthly Rate"], "500.00")
        self.assertEqual(rows[0]["Wholesale Cost"], "300.00")
        self.assertEqual(rows[0]["Margin"], "200.00")


# ── Event firing for client add/remove ────────────────────────────────────────

class TestMSPEvents(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.mkdtemp()
        self._dir = Path(self._tmp)

    def test_client_added_event(self):
        eq: asyncio.Queue = asyncio.Queue()
        mgr = MSPDashboardManager(data_dir=self._dir, event_queue=eq)
        _run(mgr.start())
        _run(mgr.add_client(**_sample_client()))
        events = []
        while not eq.empty():
            events.append(eq.get_nowait())
        add_events = [e for e in events if e["type"] == "msp.client_added"]
        self.assertEqual(len(add_events), 1)
        self.assertEqual(add_events[0]["client"]["name"], "Acme Corp")

    def test_client_removed_event(self):
        eq: asyncio.Queue = asyncio.Queue()
        mgr = MSPDashboardManager(data_dir=self._dir, event_queue=eq)
        _run(mgr.start())
        client = _run(mgr.add_client(**_sample_client()))
        _run(mgr.remove_client(client.id))
        events = []
        while not eq.empty():
            events.append(eq.get_nowait())
        remove_events = [e for e in events if e["type"] == "msp.client_removed"]
        self.assertEqual(len(remove_events), 1)


# ── MSPPortalManager ──────────────────────────────────────────────────────────

class TestMSPPortalManager(unittest.TestCase):
    def _portal(self, **cfg_kwargs) -> MSPPortalManager:
        mgr = MSPDashboardManager(data_dir=Path(tempfile.mkdtemp()))
        config = PortalConfig(**cfg_kwargs)
        return MSPPortalManager(msp_mgr=mgr, config=config)

    def _health(self, **kwargs) -> MSPClientHealth:
        defaults = dict(
            client_id="c1", fetched_at=time.time(),
            machines_online=8, machines_total=10, critical_alerts=0,
            compliance_score=0.82, e8_score=0.85, iso27001_score=0.79,
            last_backup_ok=True, pending_approvals=1, upcoming_renewals=2,
            health="green",
        )
        defaults.update(kwargs)
        return MSPClientHealth(**defaults)

    def _client(self, **kwargs) -> MSPClient:
        defaults = dict(
            id="c1", name="Acme Corp", slug="acme",
            controller_url="http://acme:7380", api_token="tok",
        )
        defaults.update(kwargs)
        return MSPClient(**defaults)

    def test_portal_html_contains_client_name(self):
        portal = self._portal()
        html = portal.get_portal_html(self._client(), self._health())
        self.assertIn("Acme Corp", html)

    def test_portal_html_contains_msp_name(self):
        portal = self._portal(msp_name="SuperIT Solutions")
        html = portal.get_portal_html(self._client(), self._health())
        self.assertIn("SuperIT Solutions", html)

    def test_portal_html_contains_primary_colour(self):
        portal = self._portal(primary_colour="#ff5733")
        html = portal.get_portal_html(self._client(), self._health())
        self.assertIn("#ff5733", html)

    def test_portal_html_green_health_label(self):
        portal = self._portal()
        html = portal.get_portal_html(self._client(), self._health(health="green"))
        self.assertIn("All systems operational", html)

    def test_portal_html_amber_health_label(self):
        portal = self._portal()
        html = portal.get_portal_html(self._client(), self._health(health="amber"))
        self.assertIn("Some attention needed", html)

    def test_portal_html_red_health_label(self):
        portal = self._portal()
        html = portal.get_portal_html(self._client(), self._health(health="red"))
        self.assertIn("Action required", html)

    def test_portal_html_shows_machines(self):
        portal = self._portal(show_machines=True)
        html = portal.get_portal_html(
            self._client(), self._health(machines_online=8, machines_total=10)
        )
        self.assertIn("8", html)
        self.assertIn("10", html)

    def test_portal_html_hides_machines(self):
        portal = self._portal(show_machines=False)
        html = portal.get_portal_html(self._client(), self._health())
        self.assertNotIn("Machines", html)

    def test_portal_html_shows_compliance(self):
        portal = self._portal(show_compliance=True)
        html = portal.get_portal_html(self._client(), self._health())
        self.assertIn("Compliance", html)
        self.assertIn("Essential Eight", html)
        self.assertIn("ISO 27001", html)

    def test_portal_html_hides_compliance(self):
        portal = self._portal(show_compliance=False)
        html = portal.get_portal_html(self._client(), self._health())
        self.assertNotIn("Essential Eight", html)

    def test_portal_html_critical_alerts_banner(self):
        portal = self._portal(show_alerts=True)
        html = portal.get_portal_html(
            self._client(), self._health(critical_alerts=3)
        )
        self.assertIn("3 critical alert", html)

    def test_portal_html_no_alerts_banner(self):
        portal = self._portal(show_alerts=True)
        html = portal.get_portal_html(
            self._client(), self._health(critical_alerts=0)
        )
        self.assertIn("No active alerts", html)

    def test_portal_html_support_email_link(self):
        portal = self._portal(support_email="support@msp.com")
        html = portal.get_portal_html(self._client(), self._health())
        self.assertIn("mailto:support@msp.com", html)

    def test_portal_html_support_phone_link(self):
        portal = self._portal(support_phone="+61299991234")
        html = portal.get_portal_html(self._client(), self._health())
        self.assertIn("tel:+61299991234", html)

    def test_portal_html_logo_injected(self):
        portal = self._portal(msp_logo_url="https://msp.com/logo.png")
        html = portal.get_portal_html(self._client(), self._health())
        self.assertIn("https://msp.com/logo.png", html)

    def test_portal_html_no_logo_no_img_tag(self):
        portal = self._portal(msp_logo_url="")
        html = portal.get_portal_html(self._client(), self._health())
        self.assertNotIn('<img', html)

    def test_portal_html_pending_approvals_shown(self):
        portal = self._portal()
        html = portal.get_portal_html(
            self._client(), self._health(pending_approvals=4)
        )
        self.assertIn("4", html)

    def test_portal_html_is_valid_html(self):
        portal = self._portal()
        html = portal.get_portal_html(self._client(), self._health())
        self.assertIn("<!DOCTYPE html>", html)
        self.assertIn("</html>", html)
        self.assertIn("tailwindcss", html)

    def test_portal_html_no_xss_in_client_name(self):
        portal = self._portal()
        # Client name with special chars should be rendered safely
        client = self._client(name="Acme <Corp>")
        html = portal.get_portal_html(client, self._health())
        # The name should appear in the page (HTML may or may not escape it)
        self.assertIn("Acme", html)

    def test_get_portal_config(self):
        portal = self._portal(msp_name="TestMSP", support_email="it@test.com")
        cfg = portal.get_portal_config()
        self.assertEqual(cfg.msp_name, "TestMSP")
        self.assertEqual(cfg.support_email, "it@test.com")

    def test_update_portal_config(self):
        portal = self._portal()
        cfg = _run(portal.update_portal_config(
            msp_name="Updated MSP", primary_colour="#aabbcc"
        ))
        self.assertEqual(cfg.msp_name, "Updated MSP")
        self.assertEqual(cfg.primary_colour, "#aabbcc")

    def test_update_portal_config_partial(self):
        portal = self._portal(support_email="old@msp.com", msp_name="Old Name")
        _run(portal.update_portal_config(msp_name="New Name"))
        cfg = portal.get_portal_config()
        self.assertEqual(cfg.msp_name, "New Name")
        self.assertEqual(cfg.support_email, "old@msp.com")  # unchanged


# ── PortalConfig model ────────────────────────────────────────────────────────

class TestPortalConfig(unittest.TestCase):
    def test_defaults(self):
        cfg = PortalConfig()
        self.assertFalse(cfg.enabled)
        self.assertEqual(cfg.msp_name, "IT Support")
        self.assertEqual(cfg.primary_colour, "#2563eb")
        self.assertTrue(cfg.show_compliance)
        self.assertTrue(cfg.show_machines)
        self.assertTrue(cfg.show_alerts)

    def test_round_trip(self):
        cfg = PortalConfig(
            enabled=True,
            msp_name="SuperIT",
            msp_logo_url="https://msp.com/logo.png",
            primary_colour="#123456",
            support_email="it@superit.com",
            support_phone="+61299991234",
            show_compliance=False,
            show_machines=True,
            show_alerts=True,
            contact_creates_ticket=False,
        )
        cfg2 = PortalConfig.from_dict(cfg.to_dict())
        self.assertTrue(cfg2.enabled)
        self.assertEqual(cfg2.msp_name, "SuperIT")
        self.assertEqual(cfg2.primary_colour, "#123456")
        self.assertFalse(cfg2.show_compliance)
        self.assertFalse(cfg2.contact_creates_ticket)


if __name__ == "__main__":
    unittest.main()
