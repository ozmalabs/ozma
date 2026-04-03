# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
MSP Multi-tenant Dashboard — aggregates health, runs bulk operations,
and generates billing exports across all managed client controllers.

Clients are remote Ozma controllers that the MSP manages. Each client
has its own controller URL and API token. This manager polls each client
controller periodically and stores a health cache.

Persistence: msp_data/
  clients.json      — client registry
  health_cache.json — latest health snapshot per client
  operations.json   — bulk operation history

Events:
  msp.health_updated   — health status changed for one or more clients
  msp.client_added     — new client registered
  msp.client_removed   — client removed
  msp.bulk_completed   — bulk operation finished
"""

from __future__ import annotations

import asyncio
import csv
import io
import json
import logging
import time
import uuid
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

try:
    import httpx as _httpx
except ImportError:  # pragma: no cover
    _httpx = None  # type: ignore

log = logging.getLogger("ozma.msp_dashboard")

# Health poll interval — 5 minutes
_POLL_INTERVAL = 300


# ── Data models ───────────────────────────────────────────────────────────────

@dataclass
class MSPClient:
    id: str                          # UUID
    name: str                        # "Acme Corp"
    slug: str                        # "acme" → acme.portal.msp.com
    controller_url: str              # https://controller.acme.internal:7380
    api_token: str                   # JWT or long-lived token for that controller
    tier: str = "business"           # starter / business / enterprise
    seat_count: int = 0
    tags: list[str] = field(default_factory=list)
    notes: str = ""
    onboarded_at: float = 0.0
    last_seen: float = 0.0
    monthly_rate: float = 0.0        # what MSP charges this client (for billing export)
    wholesale_cost: float = 0.0      # what MSP pays Ozma Connect for this client

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id,
            "name": self.name,
            "slug": self.slug,
            "controller_url": self.controller_url,
            "api_token": self.api_token,
            "tier": self.tier,
            "seat_count": self.seat_count,
            "tags": self.tags,
            "notes": self.notes,
            "onboarded_at": self.onboarded_at,
            "last_seen": self.last_seen,
            "monthly_rate": self.monthly_rate,
            "wholesale_cost": self.wholesale_cost,
        }

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> MSPClient:
        return cls(
            id=d["id"],
            name=d["name"],
            slug=d["slug"],
            controller_url=d["controller_url"],
            api_token=d["api_token"],
            tier=d.get("tier", "business"),
            seat_count=d.get("seat_count", 0),
            tags=d.get("tags", []),
            notes=d.get("notes", ""),
            onboarded_at=d.get("onboarded_at", 0.0),
            last_seen=d.get("last_seen", 0.0),
            monthly_rate=d.get("monthly_rate", 0.0),
            wholesale_cost=d.get("wholesale_cost", 0.0),
        )


@dataclass
class MSPClientHealth:
    client_id: str
    fetched_at: float
    machines_online: int
    machines_total: int
    critical_alerts: int
    compliance_score: float          # 0.0–1.0
    e8_score: float
    iso27001_score: float
    last_backup_ok: bool
    pending_approvals: int
    upcoming_renewals: int           # SaaS renewals in next 30 days
    health: str                      # "green" | "amber" | "red"
    error: str = ""                  # if we couldn't reach the controller

    def to_dict(self) -> dict[str, Any]:
        return {
            "client_id": self.client_id,
            "fetched_at": self.fetched_at,
            "machines_online": self.machines_online,
            "machines_total": self.machines_total,
            "critical_alerts": self.critical_alerts,
            "compliance_score": self.compliance_score,
            "e8_score": self.e8_score,
            "iso27001_score": self.iso27001_score,
            "last_backup_ok": self.last_backup_ok,
            "pending_approvals": self.pending_approvals,
            "upcoming_renewals": self.upcoming_renewals,
            "health": self.health,
            "error": self.error,
        }

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> MSPClientHealth:
        return cls(
            client_id=d["client_id"],
            fetched_at=d.get("fetched_at", 0.0),
            machines_online=d.get("machines_online", 0),
            machines_total=d.get("machines_total", 0),
            critical_alerts=d.get("critical_alerts", 0),
            compliance_score=d.get("compliance_score", 0.0),
            e8_score=d.get("e8_score", 0.0),
            iso27001_score=d.get("iso27001_score", 0.0),
            last_backup_ok=d.get("last_backup_ok", False),
            pending_approvals=d.get("pending_approvals", 0),
            upcoming_renewals=d.get("upcoming_renewals", 0),
            health=d.get("health", "red"),
            error=d.get("error", ""),
        )

    @staticmethod
    def calculate_health(critical_alerts: int, compliance_score: float,
                         pending_approvals: int) -> str:
        """
        red   — critical alerts present OR compliance_score < 0.5
        amber — pending_approvals > 5 OR compliance_score < 0.7
        green — otherwise
        """
        if critical_alerts > 0 or compliance_score < 0.5:
            return "red"
        if pending_approvals > 5 or compliance_score < 0.7:
            return "amber"
        return "green"


@dataclass
class BulkOperation:
    id: str
    type: str                        # "patch_deploy" | "compliance_report" | "policy_push"
    client_ids: list[str]
    params: dict
    created_at: float
    status: str = "pending"          # pending | running | completed | failed
    results: dict[str, Any] = field(default_factory=dict)  # client_id → result

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id,
            "type": self.type,
            "client_ids": self.client_ids,
            "params": self.params,
            "created_at": self.created_at,
            "status": self.status,
            "results": self.results,
        }

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> BulkOperation:
        return cls(
            id=d["id"],
            type=d["type"],
            client_ids=d.get("client_ids", []),
            params=d.get("params", {}),
            created_at=d.get("created_at", 0.0),
            status=d.get("status", "pending"),
            results=d.get("results", {}),
        )


@dataclass
class BillingLine:
    client_id: str
    client_name: str
    tier: str
    seat_count: int
    monthly_rate: float
    wholesale_cost: float
    margin: float
    notes: str = ""

    def to_dict(self) -> dict[str, Any]:
        return {
            "client_id": self.client_id,
            "client_name": self.client_name,
            "tier": self.tier,
            "seat_count": self.seat_count,
            "monthly_rate": self.monthly_rate,
            "wholesale_cost": self.wholesale_cost,
            "margin": self.margin,
            "notes": self.notes,
        }


# ── Manager ───────────────────────────────────────────────────────────────────

class MSPDashboardManager:
    """Multi-tenant MSP dashboard — aggregates health across all managed clients."""

    def __init__(self, data_dir: Path, event_queue: asyncio.Queue | None = None):
        self._data_dir = data_dir
        self._events = event_queue
        self._clients: dict[str, MSPClient] = {}
        self._health: dict[str, MSPClientHealth] = {}
        self._operations: dict[str, BulkOperation] = {}
        self._poll_task: asyncio.Task | None = None
        self._running = False

    # ── Lifecycle ─────────────────────────────────────────────────────────────

    async def start(self) -> None:
        self._data_dir.mkdir(parents=True, exist_ok=True)
        (self._data_dir / "reports").mkdir(exist_ok=True)
        self._load()
        self._running = True
        self._poll_task = asyncio.create_task(self._poll_loop(), name="msp-health-poll")
        log.info("MSP dashboard started (%d clients)", len(self._clients))

    async def stop(self) -> None:
        self._running = False
        if self._poll_task:
            self._poll_task.cancel()
            try:
                await self._poll_task
            except asyncio.CancelledError:
                pass
        self._save()
        log.info("MSP dashboard stopped")

    # ── Persistence ───────────────────────────────────────────────────────────

    def _load(self) -> None:
        clients_path = self._data_dir / "clients.json"
        if clients_path.exists():
            try:
                raw = json.loads(clients_path.read_text())
                for d in raw:
                    c = MSPClient.from_dict(d)
                    self._clients[c.id] = c
            except Exception as e:
                log.warning("Failed to load MSP clients: %s", e)

        health_path = self._data_dir / "health_cache.json"
        if health_path.exists():
            try:
                raw = json.loads(health_path.read_text())
                for d in raw:
                    h = MSPClientHealth.from_dict(d)
                    self._health[h.client_id] = h
            except Exception as e:
                log.warning("Failed to load MSP health cache: %s", e)

        ops_path = self._data_dir / "operations.json"
        if ops_path.exists():
            try:
                raw = json.loads(ops_path.read_text())
                for d in raw:
                    op = BulkOperation.from_dict(d)
                    self._operations[op.id] = op
            except Exception as e:
                log.warning("Failed to load MSP operations: %s", e)

    def _save(self) -> None:
        try:
            (self._data_dir / "clients.json").write_text(
                json.dumps([c.to_dict() for c in self._clients.values()], indent=2)
            )
            (self._data_dir / "health_cache.json").write_text(
                json.dumps([h.to_dict() for h in self._health.values()], indent=2)
            )
            (self._data_dir / "operations.json").write_text(
                json.dumps([op.to_dict() for op in self._operations.values()], indent=2)
            )
        except Exception as e:
            log.warning("Failed to save MSP data: %s", e)

    # ── Client CRUD ───────────────────────────────────────────────────────────

    async def add_client(self, name: str, slug: str, controller_url: str,
                         api_token: str, **kwargs) -> MSPClient:
        client = MSPClient(
            id=str(uuid.uuid4()),
            name=name,
            slug=slug,
            controller_url=controller_url.rstrip("/"),
            api_token=api_token,
            onboarded_at=time.time(),
            **{k: v for k, v in kwargs.items()
               if k in ("tier", "seat_count", "tags", "notes",
                        "monthly_rate", "wholesale_cost")},
        )
        self._clients[client.id] = client
        self._save()
        if self._events:
            await self._events.put({"type": "msp.client_added", "client": client.to_dict()})
        log.info("MSP client added: %s (%s)", name, client.id)
        return client

    async def remove_client(self, client_id: str) -> None:
        client = self._clients.pop(client_id, None)
        self._health.pop(client_id, None)
        self._save()
        if client and self._events:
            await self._events.put({"type": "msp.client_removed", "client_id": client_id})

    async def update_client(self, client_id: str, **kwargs) -> MSPClient:
        client = self._clients.get(client_id)
        if client is None:
            raise KeyError(f"Unknown MSP client: {client_id}")
        allowed = ("name", "slug", "controller_url", "api_token", "tier",
                   "seat_count", "tags", "notes", "monthly_rate", "wholesale_cost")
        for k, v in kwargs.items():
            if k in allowed:
                setattr(client, k, v)
        self._save()
        return client

    async def list_clients(self) -> list[MSPClient]:
        return list(self._clients.values())

    async def get_client(self, client_id: str) -> MSPClient | None:
        return self._clients.get(client_id)

    # ── Health aggregation ────────────────────────────────────────────────────

    async def refresh_client_health(self, client_id: str) -> MSPClientHealth:
        """
        Fetch health from the client's controller via HTTP.
        Contacts five endpoints:
          GET /api/v1/status            → machines_online, machines_total
          GET /api/v1/compliance/status → compliance_score, e8_score, iso27001_score
          GET /api/v1/backup/status     → last_backup_ok
          GET /api/v1/itsm/pending      → pending_approvals
          GET /api/v1/saas/renewals     → upcoming_renewals (next 30 days)
        On any failure the health record gets error set and health="red".
        """
        client = self._clients.get(client_id)
        if client is None:
            raise KeyError(f"Unknown MSP client: {client_id}")

        fetched_at = time.time()
        machines_online = 0
        machines_total = 0
        critical_alerts = 0
        compliance_score = 0.0
        e8_score = 0.0
        iso27001_score = 0.0
        last_backup_ok = False
        pending_approvals = 0
        upcoming_renewals = 0
        error = ""

        try:
            headers = {"Authorization": f"Bearer {client.api_token}"}
            async with _httpx.AsyncClient(timeout=10.0) as http:
                # /api/v1/status
                try:
                    r = await http.get(f"{client.controller_url}/api/v1/status",
                                       headers=headers)
                    if r.status_code == 200:
                        data = r.json()
                        nodes = data.get("nodes", {})
                        machines_total = len(nodes)
                        machines_online = sum(
                            1 for n in nodes.values()
                            if time.time() - n.get("last_seen", 0) < 120
                        )
                        critical_alerts = data.get("critical_alerts", 0)
                except Exception as e:
                    log.debug("MSP health /status for %s failed: %s", client_id, e)

                # /api/v1/compliance/status
                try:
                    r = await http.get(f"{client.controller_url}/api/v1/compliance/status",
                                       headers=headers)
                    if r.status_code == 200:
                        data = r.json()
                        compliance_score = data.get("overall_score", 0.0)
                        e8_score = data.get("e8_score", 0.0)
                        iso27001_score = data.get("iso27001_score", 0.0)
                except Exception as e:
                    log.debug("MSP health /compliance/status for %s failed: %s", client_id, e)

                # /api/v1/backup/status
                try:
                    r = await http.get(f"{client.controller_url}/api/v1/backup/status",
                                       headers=headers)
                    if r.status_code == 200:
                        data = r.json()
                        last_backup_ok = data.get("last_backup_ok", False)
                except Exception as e:
                    log.debug("MSP health /backup/status for %s failed: %s", client_id, e)

                # /api/v1/itsm/pending
                try:
                    r = await http.get(f"{client.controller_url}/api/v1/itsm/pending",
                                       headers=headers)
                    if r.status_code == 200:
                        data = r.json()
                        pending_approvals = data.get("count", 0)
                except Exception as e:
                    log.debug("MSP health /itsm/pending for %s failed: %s", client_id, e)

                # /api/v1/saas/renewals
                try:
                    r = await http.get(f"{client.controller_url}/api/v1/saas/renewals",
                                       headers=headers)
                    if r.status_code == 200:
                        data = r.json()
                        upcoming_renewals = data.get("count", 0)
                except Exception as e:
                    log.debug("MSP health /saas/renewals for %s failed: %s", client_id, e)

        except Exception as e:
            error = str(e)
            log.warning("MSP health fetch failed for %s: %s", client.name, e)

        health_status = MSPClientHealth.calculate_health(
            critical_alerts, compliance_score, pending_approvals
        )
        if error:
            health_status = "red"

        h = MSPClientHealth(
            client_id=client_id,
            fetched_at=fetched_at,
            machines_online=machines_online,
            machines_total=machines_total,
            critical_alerts=critical_alerts,
            compliance_score=compliance_score,
            e8_score=e8_score,
            iso27001_score=iso27001_score,
            last_backup_ok=last_backup_ok,
            pending_approvals=pending_approvals,
            upcoming_renewals=upcoming_renewals,
            health=health_status,
            error=error,
        )

        prev = self._health.get(client_id)
        self._health[client_id] = h

        # Update last_seen on the client
        if not error and client_id in self._clients:
            self._clients[client_id].last_seen = fetched_at

        # Fire event if health status changed
        if self._events and (prev is None or prev.health != h.health):
            asyncio.create_task(
                self._events.put({"type": "msp.health_updated",
                                  "client_id": client_id,
                                  "health": h.to_dict()}),
                name=f"msp-health-event-{client_id[:8]}",
            )

        return h

    async def refresh_all_health(self) -> list[MSPClientHealth]:
        """Refresh all clients concurrently."""
        if not self._clients:
            return []
        results = await asyncio.gather(
            *[self.refresh_client_health(cid) for cid in self._clients],
            return_exceptions=False,
        )
        self._save()
        return list(results)

    async def get_health(self, client_id: str) -> MSPClientHealth | None:
        return self._health.get(client_id)

    async def get_all_health(self) -> list[MSPClientHealth]:
        return list(self._health.values())

    # ── Bulk operations ───────────────────────────────────────────────────────

    async def bulk_patch_deploy(self, client_ids: list[str],
                                ring: str = "emergency") -> BulkOperation:
        """
        POST /api/v1/jobs to each client controller with type=package_install,
        target=all. Tracks per-client job IDs in BulkOperation.results.
        Fire-and-forget: records the submitted job ID; does not poll for completion.
        Phase 2: live job status polling from client controllers.
        """
        op = BulkOperation(
            id=str(uuid.uuid4()),
            type="patch_deploy",
            client_ids=client_ids,
            params={"ring": ring},
            created_at=time.time(),
            status="running",
        )
        self._operations[op.id] = op

        async with _httpx.AsyncClient(timeout=10.0) as http:
            for cid in client_ids:
                client = self._clients.get(cid)
                if not client:
                    op.results[cid] = {"error": "unknown client"}
                    continue
                try:
                    headers = {"Authorization": f"Bearer {client.api_token}"}
                    payload = {
                        "type": "package_install",
                        "target_scope": "all",
                        "params": {"ring": ring},
                    }
                    r = await http.post(f"{client.controller_url}/api/v1/jobs",
                                        json=payload, headers=headers)
                    if r.status_code in (200, 201):
                        data = r.json()
                        op.results[cid] = {"job_id": data.get("id"), "status": "submitted"}
                    else:
                        op.results[cid] = {"error": f"HTTP {r.status_code}"}
                except Exception as e:
                    op.results[cid] = {"error": str(e)}
                    log.warning("bulk_patch_deploy to %s failed: %s", client.name, e)

        op.status = "completed"
        self._save()
        if self._events:
            await self._events.put({"type": "msp.bulk_completed", "operation": op.to_dict()})
        return op

    async def bulk_compliance_reports(self, client_ids: list[str],
                                      framework: str = "e8") -> BulkOperation:
        """
        POST /api/v1/compliance/report to each client controller.
        Stores reports in msp_data/reports/{operation_id}/.
        Fire-and-forget per client: saves the returned report JSON.
        """
        op = BulkOperation(
            id=str(uuid.uuid4()),
            type="compliance_report",
            client_ids=client_ids,
            params={"framework": framework},
            created_at=time.time(),
            status="running",
        )
        self._operations[op.id] = op
        report_dir = self._data_dir / "reports" / op.id
        report_dir.mkdir(parents=True, exist_ok=True)

        async with _httpx.AsyncClient(timeout=10.0) as http:
            for cid in client_ids:
                client = self._clients.get(cid)
                if not client:
                    op.results[cid] = {"error": "unknown client"}
                    continue
                try:
                    headers = {"Authorization": f"Bearer {client.api_token}"}
                    r = await http.post(
                        f"{client.controller_url}/api/v1/compliance/report",
                        json={"framework": framework},
                        headers=headers,
                    )
                    if r.status_code in (200, 201):
                        report_path = report_dir / f"{cid}.json"
                        report_path.write_text(r.text)
                        op.results[cid] = {
                            "status": "ok",
                            "report_path": str(report_path),
                        }
                    else:
                        op.results[cid] = {"error": f"HTTP {r.status_code}"}
                except Exception as e:
                    op.results[cid] = {"error": str(e)}
                    log.warning("bulk_compliance_reports to %s failed: %s", client.name, e)

        op.status = "completed"
        self._save()
        if self._events:
            await self._events.put({"type": "msp.bulk_completed", "operation": op.to_dict()})
        return op

    async def bulk_policy_push(self, client_ids: list[str],
                               policy: dict) -> BulkOperation:
        """
        POST /api/v1/osquery/policy to each client controller.
        Tracks success/failure per client.
        """
        op = BulkOperation(
            id=str(uuid.uuid4()),
            type="policy_push",
            client_ids=client_ids,
            params={"policy": policy},
            created_at=time.time(),
            status="running",
        )
        self._operations[op.id] = op

        async with _httpx.AsyncClient(timeout=10.0) as http:
            for cid in client_ids:
                client = self._clients.get(cid)
                if not client:
                    op.results[cid] = {"error": "unknown client"}
                    continue
                try:
                    headers = {"Authorization": f"Bearer {client.api_token}"}
                    r = await http.post(
                        f"{client.controller_url}/api/v1/osquery/policy",
                        json=policy,
                        headers=headers,
                    )
                    if r.status_code in (200, 201):
                        op.results[cid] = {"status": "ok"}
                    else:
                        op.results[cid] = {"error": f"HTTP {r.status_code}"}
                except Exception as e:
                    op.results[cid] = {"error": str(e)}
                    log.warning("bulk_policy_push to %s failed: %s", client.name, e)

        op.status = "completed"
        self._save()
        if self._events:
            await self._events.put({"type": "msp.bulk_completed", "operation": op.to_dict()})
        return op

    async def get_operation(self, op_id: str) -> BulkOperation | None:
        # NOTE: Phase 2 — live per-client job status polling requires a second
        # round of GET /api/v1/jobs/{job_id} calls to each client controller.
        # Currently returns the initial BulkOperation result as recorded.
        return self._operations.get(op_id)

    async def list_operations(self) -> list[BulkOperation]:
        return list(self._operations.values())

    # ── Billing ───────────────────────────────────────────────────────────────

    async def monthly_billing_export(self, year: int, month: int) -> list[BillingLine]:
        """
        Collects current seat counts from each client controller (GET /api/v1/status).
        Uses stored monthly_rate and wholesale_cost from the client record.
        Returns BillingLine per client, sorted by client name.
        """
        lines: list[BillingLine] = []

        async with _httpx.AsyncClient(timeout=10.0) as http:
            for client in self._clients.values():
                seat_count = client.seat_count  # fallback to stored value
                try:
                    headers = {"Authorization": f"Bearer {client.api_token}"}
                    r = await http.get(f"{client.controller_url}/api/v1/status",
                                       headers=headers)
                    if r.status_code == 200:
                        data = r.json()
                        nodes = data.get("nodes", {})
                        if nodes:
                            seat_count = len(nodes)
                except Exception as e:
                    log.debug("billing seat count for %s failed: %s", client.name, e)

                margin = client.monthly_rate - client.wholesale_cost
                lines.append(BillingLine(
                    client_id=client.id,
                    client_name=client.name,
                    tier=client.tier,
                    seat_count=seat_count,
                    monthly_rate=client.monthly_rate,
                    wholesale_cost=client.wholesale_cost,
                    margin=round(margin, 2),
                    notes=client.notes,
                ))

        lines.sort(key=lambda l: l.client_name.lower())
        return lines

    async def billing_csv(self, year: int, month: int) -> str:
        """Returns CSV string of billing export."""
        lines = await self.monthly_billing_export(year, month)
        buf = io.StringIO()
        writer = csv.writer(buf)
        writer.writerow(["Client", "Slug", "Tier", "Seats",
                         "Monthly Rate", "Wholesale Cost", "Margin", "Notes"])
        for line in lines:
            client = next((c for c in self._clients.values()
                           if c.id == line.client_id), None)
            slug = client.slug if client else ""
            writer.writerow([
                line.client_name,
                slug,
                line.tier,
                line.seat_count,
                f"{line.monthly_rate:.2f}",
                f"{line.wholesale_cost:.2f}",
                f"{line.margin:.2f}",
                line.notes,
            ])
        return buf.getvalue()

    # ── Alert aggregation ─────────────────────────────────────────────────────

    async def aggregate_alerts(self, severity: str = "",
                               client_id: str = "") -> list[dict]:
        """
        Fetch recent alerts from each client (GET /api/v1/notifications?format=msp).
        Returns merged list sorted by severity + age, with client_id injected.
        Clients that fail are skipped (their error is logged).
        """
        _SEVERITY_ORDER = {"critical": 0, "high": 1, "medium": 2, "low": 3, "info": 4}
        all_alerts: list[dict] = []

        target_clients = (
            [self._clients[client_id]]
            if client_id and client_id in self._clients
            else list(self._clients.values())
        )

        async with _httpx.AsyncClient(timeout=10.0) as http:
            for client in target_clients:
                try:
                    headers = {"Authorization": f"Bearer {client.api_token}"}
                    params: dict[str, str] = {"format": "msp"}
                    if severity:
                        params["severity"] = severity
                    r = await http.get(f"{client.controller_url}/api/v1/notifications",
                                       headers=headers, params=params)
                    if r.status_code == 200:
                        alerts = r.json()
                        if isinstance(alerts, list):
                            for alert in alerts:
                                alert["client_id"] = client.id
                                alert["client_name"] = client.name
                                all_alerts.append(alert)
                except Exception as e:
                    log.debug("aggregate_alerts from %s failed: %s", client.name, e)

        # Sort by severity (critical first) then by age (newest first)
        all_alerts.sort(key=lambda a: (
            _SEVERITY_ORDER.get(a.get("severity", "info"), 99),
            -(a.get("created_at", 0)),
        ))
        return all_alerts

    # ── Internal poll loop ────────────────────────────────────────────────────

    async def _poll_loop(self) -> None:
        """Every 5 minutes, refresh health for all clients concurrently."""
        while self._running:
            try:
                await asyncio.sleep(_POLL_INTERVAL)
                if self._clients:
                    log.debug("MSP health poll: %d clients", len(self._clients))
                    await self.refresh_all_health()
            except asyncio.CancelledError:
                break
            except Exception as e:
                log.warning("MSP poll loop error: %s", e)
