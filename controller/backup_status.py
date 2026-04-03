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
