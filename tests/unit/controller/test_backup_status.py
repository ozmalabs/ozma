# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Unit tests for controller/backup_status.py."""

from __future__ import annotations

import json
import sys
import time
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).parents[3] / "controller"))

from backup_status import (
    NodeBackupReport,
    FleetBackupSummary,
    BackupStatusTracker,
)


# ---------------------------------------------------------------------------
# NodeBackupReport
# ---------------------------------------------------------------------------

class TestNodeBackupReport:
    def _make(self, **kw) -> NodeBackupReport:
        defaults = dict(
            node_id="node-1",
            enabled=True,
            health="green",
            health_message="Backed up 1h ago",
            last_success_at=time.time() - 3600,
            last_failure_at=None,
            last_error=None,
            consecutive_failures=0,
            snapshots_count=5,
            total_size_bytes=1_000_000,
            running=False,
        )
        defaults.update(kw)
        return NodeBackupReport(**defaults)

    def test_to_dict_keys(self):
        r = self._make()
        d = r.to_dict()
        for key in ("node_id", "enabled", "health", "health_message",
                    "last_success_at", "last_failure_at", "last_error",
                    "consecutive_failures", "snapshots_count",
                    "total_size_bytes", "running", "reported_at"):
            assert key in d, f"Missing key: {key}"

    def test_roundtrip(self):
        r = self._make(health="orange", consecutive_failures=2)
        r2 = NodeBackupReport.from_dict(r.to_dict())
        assert r2.health == "orange"
        assert r2.consecutive_failures == 2
        assert r2.node_id == "node-1"

    def test_from_dict_defaults(self):
        r = NodeBackupReport.from_dict({"node_id": "x"})
        assert r.health == "unconfigured"
        assert r.enabled is False
        assert r.consecutive_failures == 0


# ---------------------------------------------------------------------------
# BackupStatusTracker — ingest and retrieve
# ---------------------------------------------------------------------------

class TestBackupStatusTrackerIngest:
    def _tracker(self, tmp_path) -> BackupStatusTracker:
        return BackupStatusTracker(state_path=tmp_path / "backup_fleet.json")

    def _report_dict(self, **kw) -> dict:
        defaults = dict(
            enabled=True,
            health="green",
            health_message="OK",
            last_success_at=time.time() - 1800,
            last_failure_at=None,
            last_error=None,
            consecutive_failures=0,
            snapshots_count=3,
            total_size_bytes=500_000,
            running=False,
        )
        defaults.update(kw)
        return defaults

    def test_ingest_and_retrieve(self, tmp_path):
        t = self._tracker(tmp_path)
        t.ingest("node-1", self._report_dict(health="yellow"))
        r = t.get_node_report("node-1")
        assert r is not None
        assert r.health == "yellow"

    def test_ingest_updates_existing(self, tmp_path):
        t = self._tracker(tmp_path)
        t.ingest("node-1", self._report_dict(health="green"))
        t.ingest("node-1", self._report_dict(health="red"))
        assert t.get_node_report("node-1").health == "red"

    def test_unknown_node_returns_none(self, tmp_path):
        t = self._tracker(tmp_path)
        assert t.get_node_report("ghost") is None

    def test_all_node_ids(self, tmp_path):
        t = self._tracker(tmp_path)
        t.ingest("node-1", self._report_dict())
        t.ingest("node-2", self._report_dict())
        ids = t.all_node_ids()
        assert "node-1" in ids
        assert "node-2" in ids

    def test_remove_node(self, tmp_path):
        t = self._tracker(tmp_path)
        t.ingest("node-1", self._report_dict())
        t.remove_node("node-1")
        assert t.get_node_report("node-1") is None

    def test_remove_nonexistent_noop(self, tmp_path):
        t = self._tracker(tmp_path)
        t.remove_node("ghost")  # should not raise


# ---------------------------------------------------------------------------
# Persistence
# ---------------------------------------------------------------------------

class TestBackupStatusPersistence:
    def _report_dict(self, health="green") -> dict:
        return dict(
            enabled=True,
            health=health,
            health_message="",
            last_success_at=time.time() - 3600,
            last_failure_at=None,
            last_error=None,
            consecutive_failures=0,
            snapshots_count=2,
            total_size_bytes=200_000,
            running=False,
        )

    def test_persists_to_disk(self, tmp_path):
        p = tmp_path / "fleet.json"
        t = BackupStatusTracker(state_path=p)
        t.ingest("n1", self._report_dict("yellow"))
        assert p.exists()

    def test_loads_on_restart(self, tmp_path):
        p = tmp_path / "fleet.json"
        t = BackupStatusTracker(state_path=p)
        t.ingest("n1", self._report_dict("orange"))

        t2 = BackupStatusTracker(state_path=p)
        assert t2.get_node_report("n1").health == "orange"

    def test_file_permissions(self, tmp_path):
        p = tmp_path / "fleet.json"
        t = BackupStatusTracker(state_path=p)
        t.ingest("n1", self._report_dict())
        assert oct(p.stat().st_mode)[-3:] == "600"

    def test_missing_file_no_error(self, tmp_path):
        p = tmp_path / "missing_fleet.json"
        t = BackupStatusTracker(state_path=p)
        assert t.all_node_ids() == []


# ---------------------------------------------------------------------------
# Fleet summary
# ---------------------------------------------------------------------------

class TestFleetSummary:
    def _tracker(self, tmp_path) -> BackupStatusTracker:
        return BackupStatusTracker(state_path=tmp_path / "fleet.json")

    def _report(self, health: str, enabled=True, size=100_000) -> dict:
        return dict(
            enabled=enabled,
            health=health,
            health_message="",
            last_success_at=time.time() - 1000,
            last_failure_at=None,
            last_error=None,
            consecutive_failures=0,
            snapshots_count=1,
            total_size_bytes=size,
            running=False,
        )

    def test_empty_fleet_is_unconfigured(self, tmp_path):
        t = self._tracker(tmp_path)
        s = t.get_fleet_summary()
        assert s.fleet_health == "unconfigured"
        assert s.total_nodes == 0

    def test_all_green(self, tmp_path):
        t = self._tracker(tmp_path)
        t.ingest("n1", self._report("green"))
        t.ingest("n2", self._report("green"))
        s = t.get_fleet_summary()
        assert s.fleet_health == "green"
        assert s.green_count == 2

    def test_worst_wins(self, tmp_path):
        t = self._tracker(tmp_path)
        t.ingest("n1", self._report("green"))
        t.ingest("n2", self._report("red"))
        assert t.get_fleet_summary().fleet_health == "red"

    def test_yellow_beats_green(self, tmp_path):
        t = self._tracker(tmp_path)
        t.ingest("n1", self._report("green"))
        t.ingest("n2", self._report("yellow"))
        assert t.get_fleet_summary().fleet_health == "yellow"

    def test_orange_beats_yellow(self, tmp_path):
        t = self._tracker(tmp_path)
        t.ingest("n1", self._report("yellow"))
        t.ingest("n2", self._report("orange"))
        assert t.get_fleet_summary().fleet_health == "orange"

    def test_total_size_aggregated(self, tmp_path):
        t = self._tracker(tmp_path)
        t.ingest("n1", self._report("green", size=1_000))
        t.ingest("n2", self._report("green", size=2_000))
        assert t.get_fleet_summary().total_backup_size_bytes == 3_000

    def test_configured_count(self, tmp_path):
        t = self._tracker(tmp_path)
        t.ingest("n1", self._report("green", enabled=True))
        t.ingest("n2", self._report("unconfigured", enabled=False))
        s = t.get_fleet_summary()
        assert s.configured_nodes == 1
        assert s.total_nodes == 2

    def test_summary_contains_nodes(self, tmp_path):
        t = self._tracker(tmp_path)
        t.ingest("n1", self._report("green"))
        s = t.get_fleet_summary()
        assert len(s.nodes) == 1
        assert s.nodes[0]["node_id"] == "n1"

    def test_summary_to_dict_keys(self, tmp_path):
        t = self._tracker(tmp_path)
        d = t.get_fleet_summary().to_dict()
        for key in ("fleet_health", "total_nodes", "configured_nodes",
                    "green_count", "yellow_count", "orange_count",
                    "red_count", "unconfigured_count",
                    "total_backup_size_bytes", "nodes"):
            assert key in d, f"Missing key: {key}"


# ---------------------------------------------------------------------------
# Stale pruning
# ---------------------------------------------------------------------------

class TestStalePruning:
    def test_stale_reports_pruned(self, tmp_path):
        p = tmp_path / "fleet.json"
        t = BackupStatusTracker(state_path=p)
        # Manually inject a stale report
        old_report = NodeBackupReport(
            node_id="old-node",
            enabled=True,
            health="green",
            health_message="",
            last_success_at=0.0,
            last_failure_at=None,
            last_error=None,
            consecutive_failures=0,
            snapshots_count=0,
            total_size_bytes=0,
            running=False,
            reported_at=1.0,  # epoch — definitely stale
        )
        t._reports["old-node"] = old_report

        # get_fleet_summary triggers pruning
        s = t.get_fleet_summary()
        assert t.get_node_report("old-node") is None

    def test_fresh_reports_not_pruned(self, tmp_path):
        t = BackupStatusTracker(state_path=tmp_path / "fleet.json")
        t.ingest("fresh", dict(
            enabled=True, health="green", health_message="",
            last_success_at=time.time(), last_failure_at=None, last_error=None,
            consecutive_failures=0, snapshots_count=1,
            total_size_bytes=1000, running=False,
        ))
        t.get_fleet_summary()
        assert t.get_node_report("fresh") is not None


# ---------------------------------------------------------------------------
# Alert callback
# ---------------------------------------------------------------------------

class TestAlertCallback:
    def test_alert_fired_on_health_degradation(self, tmp_path):
        """Alert callback is scheduled when health worsens."""
        import asyncio

        fired: list[tuple] = []

        async def _alert(node_id, health, message):
            fired.append((node_id, health, message))

        t = BackupStatusTracker(state_path=tmp_path / "fleet.json",
                                alert_callback=_alert)

        def _report(h):
            return dict(
                enabled=True, health=h, health_message=f"health={h}",
                last_success_at=time.time(), last_failure_at=None, last_error=None,
                consecutive_failures=0, snapshots_count=1,
                total_size_bytes=0, running=False,
            )

        async def _run():
            t.ingest("n1", _report("green"))
            t.ingest("n1", _report("red"))   # degradation → alert task created
            await asyncio.sleep(0)           # allow task to run

        asyncio.get_event_loop().run_until_complete(_run())
        assert len(fired) == 1
        assert fired[0][0] == "n1"
        assert fired[0][1] == "red"

    def test_no_alert_on_improvement(self, tmp_path):
        """Alert callback NOT fired when health improves."""
        import asyncio

        fired: list[tuple] = []

        async def _alert(node_id, health, message):
            fired.append((node_id, health, message))

        t = BackupStatusTracker(state_path=tmp_path / "fleet.json",
                                alert_callback=_alert)

        def _report(h):
            return dict(
                enabled=True, health=h, health_message="",
                last_success_at=time.time(), last_failure_at=None, last_error=None,
                consecutive_failures=0, snapshots_count=1,
                total_size_bytes=0, running=False,
            )

        async def _run():
            t.ingest("n1", _report("red"))
            t.ingest("n1", _report("green"))  # improvement → no alert
            await asyncio.sleep(0)

        asyncio.get_event_loop().run_until_complete(_run())
        assert len(fired) == 0
