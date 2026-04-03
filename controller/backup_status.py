# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Controller-side backup status tracker.

Each agent reports its backup status periodically via
POST /api/v1/nodes/{id}/backup-status.  This module aggregates those
reports, computes fleet-level health, and fires alert events when nodes
go overdue or start failing.

Fleet health roll-up
────────────────────
  green       — all configured nodes are green
  yellow      — at least one node is yellow; none are orange/red
  orange      — at least one node is orange; none are red
  red         — at least one node is red (overdue >14d or 3+ failures)
  unconfigured— no nodes have backup configured yet

Alert thresholds (match agent/backup.py)
────────────────────────────────────────
  GREEN       — last_success_at < 3 days ago
  YELLOW      — 3–7 days  or  unconfigured
  ORANGE      — 7–14 days or  2 consecutive failures
  RED         — >14 days  or  3+ consecutive failures
"""

from __future__ import annotations

import json
import logging
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

try:
    import aiohttp
except ImportError:
    aiohttp = None  # type: ignore

log = logging.getLogger("ozma.controller.backup_status")

# Health severity order (ascending)
_SEVERITY: dict[str, int] = {
    "unconfigured": 0,
    "green": 1,
    "yellow": 2,
    "orange": 3,
    "red": 4,
}


@dataclass
class NodeBackupReport:
    """Single node backup status as received from the agent."""

    node_id: str
    enabled: bool
    health: str                     # green/yellow/orange/red/unconfigured
    health_message: str
    last_success_at: float | None
    last_failure_at: float | None
    last_error: str | None
    consecutive_failures: int
    snapshots_count: int
    total_size_bytes: int
    running: bool
    reported_at: float = field(default_factory=time.time)

    def to_dict(self) -> dict[str, Any]:
        return {
            "node_id":              self.node_id,
            "enabled":              self.enabled,
            "health":               self.health,
            "health_message":       self.health_message,
            "last_success_at":      self.last_success_at,
            "last_failure_at":      self.last_failure_at,
            "last_error":           self.last_error,
            "consecutive_failures": self.consecutive_failures,
            "snapshots_count":      self.snapshots_count,
            "total_size_bytes":     self.total_size_bytes,
            "running":              self.running,
            "reported_at":          self.reported_at,
        }

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> "NodeBackupReport":
        return cls(
            node_id              = d["node_id"],
            enabled              = d.get("enabled", False),
            health               = d.get("health", "unconfigured"),
            health_message       = d.get("health_message", ""),
            last_success_at      = d.get("last_success_at"),
            last_failure_at      = d.get("last_failure_at"),
            last_error           = d.get("last_error"),
            consecutive_failures = d.get("consecutive_failures", 0),
            snapshots_count      = d.get("snapshots_count", 0),
            total_size_bytes     = d.get("total_size_bytes", 0),
            running              = d.get("running", False),
            reported_at          = d.get("reported_at", time.time()),
        )


@dataclass
class FleetBackupSummary:
    """Aggregated backup health across all nodes."""

    fleet_health: str                        # worst node health
    total_nodes: int
    configured_nodes: int
    green_count: int
    yellow_count: int
    orange_count: int
    red_count: int
    unconfigured_count: int
    total_backup_size_bytes: int
    nodes: list[dict[str, Any]]

    def to_dict(self) -> dict[str, Any]:
        return {
            "fleet_health":           self.fleet_health,
            "total_nodes":            self.total_nodes,
            "configured_nodes":       self.configured_nodes,
            "green_count":            self.green_count,
            "yellow_count":           self.yellow_count,
            "orange_count":           self.orange_count,
            "red_count":              self.red_count,
            "unconfigured_count":     self.unconfigured_count,
            "total_backup_size_bytes": self.total_backup_size_bytes,
            "nodes":                  self.nodes,
        }


class BackupStatusTracker:
    """
    Maintains per-node backup reports and computes fleet-level health.

    Thread-safe for asyncio (single-threaded event loop model).
    State persists to disk so dashboard survives controller restarts.
    """

    # Forget reports older than this (node probably gone)
    STALE_THRESHOLD = 7 * 24 * 3600  # 7 days

    def __init__(self, state_path: Path | None = None,
                 alert_callback: Any | None = None) -> None:
        self._state_path = state_path or Path("/var/lib/ozma/backup_fleet_status.json")
        self._alert_callback = alert_callback  # async callable(node_id, health, message)
        self._reports: dict[str, NodeBackupReport] = {}
        self._prev_health: dict[str, str] = {}
        self._load()

    # ------------------------------------------------------------------
    # Public API
    # ------------------------------------------------------------------

    def ingest(self, node_id: str, report_dict: dict[str, Any]) -> None:
        """Accept a backup status report from a node agent."""
        report_dict["node_id"] = node_id
        report = NodeBackupReport.from_dict(report_dict)
        prev = self._reports.get(node_id)
        self._reports[node_id] = report
        self._save()

        # Fire alert if health degraded
        if self._alert_callback and prev and prev.health != report.health:
            prev_sev = _SEVERITY.get(prev.health, 0)
            new_sev  = _SEVERITY.get(report.health, 0)
            if new_sev > prev_sev:
                import asyncio
                asyncio.create_task(
                    self._alert_callback(node_id, report.health, report.health_message),
                    name=f"backup-alert-{node_id}",
                )

    def get_node_report(self, node_id: str) -> NodeBackupReport | None:
        return self._reports.get(node_id)

    def get_fleet_summary(self) -> FleetBackupSummary:
        """Compute and return aggregated fleet backup health."""
        self._prune_stale()

        counts: dict[str, int] = {k: 0 for k in _SEVERITY}
        total_size = 0
        worst = "unconfigured"

        node_dicts = []
        for report in self._reports.values():
            h = report.health
            counts[h] = counts.get(h, 0) + 1
            total_size += report.total_size_bytes
            if _SEVERITY.get(h, 0) > _SEVERITY.get(worst, 0):
                worst = h
            node_dicts.append(report.to_dict())

        configured = sum(
            1 for r in self._reports.values() if r.enabled
        )

        return FleetBackupSummary(
            fleet_health          = worst,
            total_nodes           = len(self._reports),
            configured_nodes      = configured,
            green_count           = counts.get("green", 0),
            yellow_count          = counts.get("yellow", 0),
            orange_count          = counts.get("orange", 0),
            red_count             = counts.get("red", 0),
            unconfigured_count    = counts.get("unconfigured", 0),
            total_backup_size_bytes = total_size,
            nodes                 = node_dicts,
        )

    def remove_node(self, node_id: str) -> None:
        """Remove a node's backup status record (e.g. node decommissioned)."""
        self._reports.pop(node_id, None)
        self._save()

    def all_node_ids(self) -> list[str]:
        return list(self._reports.keys())

    # ------------------------------------------------------------------
    # Persistence
    # ------------------------------------------------------------------

    def _save(self) -> None:
        self._state_path.parent.mkdir(parents=True, exist_ok=True)
        tmp = self._state_path.with_suffix(".tmp")
        data = {nid: r.to_dict() for nid, r in self._reports.items()}
        tmp.write_text(json.dumps(data, indent=2))
        tmp.chmod(0o600)
        tmp.rename(self._state_path)

    def _load(self) -> None:
        if not self._state_path.exists():
            return
        try:
            data = json.loads(self._state_path.read_text())
            for node_id, d in data.items():
                self._reports[node_id] = NodeBackupReport.from_dict(d)
        except Exception:
            log.exception("Failed to load backup fleet status")

    def _prune_stale(self) -> None:
        cutoff = time.time() - self.STALE_THRESHOLD
        stale = [nid for nid, r in self._reports.items() if r.reported_at < cutoff]
        for nid in stale:
            log.info("Pruning stale backup report for node %s", nid)
            self._reports.pop(nid)
        if stale:
            self._save()


# ---------------------------------------------------------------------------
# Default-on nudge service
# ---------------------------------------------------------------------------

class BackupNudgeService:
    """
    Fires 'backup.not_configured' events for nodes that have never sent a
    backup report, prompting the dashboard to show the one-click setup offer.

    Also proxies snapshot browse and restore commands from the controller to
    node agents (controller → agent HTTP API on port api_port).
    """

    _NUDGE_INTERVAL  = 3600.0   # check once per hour
    _NUDGE_COOLDOWN  = 86400.0  # don't re-nudge the same node within 24 h
    _PROXY_TIMEOUT   = 15.0     # seconds for proxied HTTP calls

    def __init__(
        self,
        state: Any,                      # AppState — provides node list
        tracker: BackupStatusTracker,
        event_queue: Any | None = None,  # asyncio.Queue for WebSocket events
    ) -> None:
        self._state       = state
        self._tracker     = tracker
        self._event_queue = event_queue
        self._last_nudge:  dict[str, float] = {}   # node_id → last nudge ts
        self._task:        Any | None = None

    async def start(self) -> None:
        import asyncio
        self._task = asyncio.create_task(self._nudge_loop(), name="backup.nudge")

    async def stop(self) -> None:
        if self._task:
            self._task.cancel()
            import asyncio
            await asyncio.gather(self._task, return_exceptions=True)

    async def _nudge_loop(self) -> None:
        import asyncio
        while True:
            await asyncio.sleep(self._NUDGE_INTERVAL)
            try:
                await self._check_unconfigured()
            except Exception:
                log.exception("nudge_loop error")

    async def _check_unconfigured(self) -> None:
        """Fire events for nodes with no or unconfigured backup reports."""
        now = time.time()
        nodes = list(getattr(self._state, "nodes", {}).values())
        for node in nodes:
            node_id = getattr(node, "id", None) or getattr(node, "node_id", None)
            if not node_id:
                continue
            report = self._tracker.get_node_report(node_id)
            if report and report.health not in ("unconfigured", ""):
                continue  # already configured
            if report and getattr(report, "time_machine_enabled", False):
                continue  # macOS TM already covers this node — don't nag
            # Check cooldown
            if now - self._last_nudge.get(node_id, 0) < self._NUDGE_COOLDOWN:
                continue
            self._last_nudge[node_id] = now
            log.info("Nudging node %s: backup not configured", node_id)
            await self._fire_event({
                "type":      "backup.not_configured",
                "node_id":   node_id,
                "node_name": getattr(node, "name", node_id),
                "ts":        now,
            })

    async def check_onboarding(self, node_id: str) -> None:
        """
        Called when a new node registers.  Queries the node's agent for backup
        eligibility and fires a 'backup.onboarding_offer' event if the node
        is eligible for default-on setup.

        This drives the dashboard "~8 GB found — Back up now" one-click prompt.
        """
        # Short delay to let the agent stabilise after registration
        import asyncio
        await asyncio.sleep(5.0)

        # Fetch eligibility from agent
        eligibility = await self.proxy_get(node_id, "/api/v1/backup/onboarding")
        if not eligibility:
            return

        if not eligibility.get("eligible"):
            # Already configured, on battery, not enough disk, etc.
            if eligibility.get("time_machine_enabled"):
                log.info("Node %s: Time Machine detected — skipping backup onboarding", node_id)
            return

        size_bytes  = eligibility.get("estimated_size_bytes", 0)
        size_gb     = round(size_bytes / (1024 ** 3), 1) if size_bytes else 0
        platform    = eligibility.get("platform", "")
        size_label  = f"~{size_gb} GB found" if size_gb else "Files found"

        nodes = getattr(self._state, "nodes", {})
        node  = nodes.get(node_id)
        name  = getattr(node, "name", node_id) if node else node_id

        log.info("Firing backup onboarding offer for node %s (%s, %s GB)",
                 node_id, name, size_gb)
        await self._fire_event({
            "type":                  "backup.onboarding_offer",
            "node_id":               node_id,
            "node_name":             name,
            "platform":              platform,
            "size_label":            size_label,
            "estimated_size_bytes":  size_bytes,
            "suggested_config":      eligibility.get("suggested_config", {}),
            "ts":                    time.time(),
        })

    async def _fire_event(self, event: dict[str, Any]) -> None:
        if self._event_queue is not None:
            await self._event_queue.put(event)

    # ------------------------------------------------------------------
    # Proxy helpers
    # ------------------------------------------------------------------

    async def proxy_get(self, node_id: str, path: str) -> dict[str, Any] | list | None:
        """
        GET a path from a node agent's HTTP API.

        Returns the parsed JSON response, or None if unreachable.
        """
        url = self._agent_url(node_id, path)
        if not url:
            return None
        if aiohttp is None:
            return None
        try:
            async with aiohttp.ClientSession() as session:
                async with session.get(
                    url, timeout=aiohttp.ClientTimeout(total=self._PROXY_TIMEOUT)
                ) as resp:
                    if resp.status == 200:
                        return await resp.json()
                    log.debug("agent proxy GET %s → %d", url, resp.status)
                    return None
        except Exception as exc:
            log.debug("proxy_get %s: %s", url, exc)
            return None

    async def proxy_post(
        self, node_id: str, path: str, body: dict[str, Any]
    ) -> dict[str, Any] | None:
        """POST to a node agent's HTTP API and return the JSON response."""
        url = self._agent_url(node_id, path)
        if not url:
            return None
        if aiohttp is None:
            return None
        try:
            async with aiohttp.ClientSession() as session:
                async with session.post(
                    url, json=body,
                    timeout=aiohttp.ClientTimeout(total=self._PROXY_TIMEOUT),
                ) as resp:
                    if resp.status in (200, 201, 202):
                        return await resp.json()
                    log.debug("agent proxy POST %s → %d", url, resp.status)
                    return None
        except Exception as exc:
            log.debug("proxy_post %s: %s", url, exc)
            return None

    def _agent_url(self, node_id: str, path: str) -> str | None:
        """Build the base URL for a node's agent API."""
        nodes = getattr(self._state, "nodes", {})
        node  = nodes.get(node_id)
        if not node:
            return None
        host     = getattr(node, "host", None)
        api_port = getattr(node, "api_port", None)
        if not host or not api_port:
            return None
        return f"http://{host}:{api_port}{path}"
