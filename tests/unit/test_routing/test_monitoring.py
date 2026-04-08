# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Unit tests for the routing monitoring, journal, and trend alert system (Phase 6)."""

from __future__ import annotations

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent.parent.parent.parent / "controller"))

import pytest
import time

from routing.monitoring import (
    JournalPolicy,
    JournalRetention,
    MetricPoint,
    MetricRetention,
    MetricSeries,
    MetricStore,
    MonitoringJournal,
    StateChangeRecord,
    StateChangeType,
    TrendAlert,
    TrendAlertManager,
    TrendAlertType,
)

pytestmark = pytest.mark.unit


# ── StateChangeType ───────────────────────────────────────────────────────────

class TestStateChangeType:
    def test_at_least_25_types(self):
        assert len(StateChangeType) >= 25

    def test_device_and_link_types_present(self):
        values = [t.value for t in StateChangeType]
        assert "device_online" in values
        assert "device_offline" in values
        assert "link_up" in values
        assert "link_down" in values

    def test_pipeline_types_present(self):
        values = [t.value for t in StateChangeType]
        assert "pipeline_activated" in values
        assert "pipeline_rerouted" in values

    def test_trend_types_present(self):
        values = [t.value for t in StateChangeType]
        assert "trend_degradation" in values
        assert "trend_capacity_exhaustion" in values
        assert "trend_anomaly" in values
        assert "trend_recurring_failure" in values
        assert "trend_lifetime_estimate" in values

    def test_alert_types_present(self):
        values = [t.value for t in StateChangeType]
        assert "alert_raised" in values
        assert "alert_resolved" in values


# ── StateChangeRecord ─────────────────────────────────────────────────────────

class TestStateChangeRecord:
    def test_defaults(self):
        rec = StateChangeRecord(type=StateChangeType.device_online)
        assert rec.severity == "info"
        assert rec.device_id is None
        assert rec.id  # auto-generated

    def test_with_device(self):
        rec = StateChangeRecord(
            type=StateChangeType.link_down,
            device_id="dev-1",
            link_id="link-a",
            message="link went down",
            severity="error",
        )
        assert rec.device_id == "dev-1"
        assert rec.link_id == "link-a"
        assert rec.severity == "error"

    def test_to_dict(self):
        rec = StateChangeRecord(
            type=StateChangeType.pipeline_activated,
            device_id="dev-1",
            message="pipeline started",
        )
        d = rec.to_dict()
        assert d["type"] == "pipeline_activated"
        assert d["device_id"] == "dev-1"
        assert d["message"] == "pipeline started"
        assert "id" in d
        assert "timestamp" in d
        assert "wall_time" in d

    def test_unique_ids(self):
        r1 = StateChangeRecord(type=StateChangeType.device_online)
        r2 = StateChangeRecord(type=StateChangeType.device_online)
        assert r1.id != r2.id


# ── JournalRetention / JournalPolicy ─────────────────────────────────────────

class TestJournalPolicy:
    def test_defaults(self):
        pol = JournalPolicy()
        assert pol.retention.max_entries == 10_000
        assert pol.dedup_window_s == 1.0

    def test_custom_retention(self):
        pol = JournalPolicy(retention=JournalRetention(max_entries=50, max_age_s=3600))
        assert pol.retention.max_entries == 50

    def test_to_dict(self):
        pol = JournalPolicy()
        d = pol.to_dict()
        assert "retention" in d
        assert "dedup_window_s" in d


# ── MonitoringJournal ─────────────────────────────────────────────────────────

class TestMonitoringJournal:
    def _journal(self, max_entries=1000, max_age_s=3600, dedup_window_s=0.0):
        return MonitoringJournal(
            JournalPolicy(
                retention=JournalRetention(max_entries=max_entries, max_age_s=max_age_s),
                dedup_window_s=dedup_window_s,
            )
        )

    def test_append_and_length(self):
        j = self._journal()
        j.append(StateChangeRecord(type=StateChangeType.device_online, device_id="a"))
        j.append(StateChangeRecord(type=StateChangeType.device_offline, device_id="a"))
        assert len(j) == 2

    def test_query_all(self):
        j = self._journal()
        j.append(StateChangeRecord(type=StateChangeType.device_online, device_id="dev-1"))
        j.append(StateChangeRecord(type=StateChangeType.link_up, link_id="link-1"))
        results = j.query(limit=10)
        assert len(results) == 2

    def test_query_by_device(self):
        j = self._journal()
        j.append(StateChangeRecord(type=StateChangeType.device_online, device_id="dev-1"))
        j.append(StateChangeRecord(type=StateChangeType.device_online, device_id="dev-2"))
        results = j.query(device_id="dev-1")
        assert len(results) == 1
        assert results[0].device_id == "dev-1"

    def test_query_by_type(self):
        j = self._journal()
        j.append(StateChangeRecord(type=StateChangeType.link_up))
        j.append(StateChangeRecord(type=StateChangeType.link_down))
        j.append(StateChangeRecord(type=StateChangeType.link_up))
        results = j.query(types=[StateChangeType.link_up])
        assert len(results) == 2

    def test_query_by_severity(self):
        j = self._journal()
        j.append(StateChangeRecord(type=StateChangeType.device_online, severity="info"))
        j.append(StateChangeRecord(type=StateChangeType.link_down, severity="error"))
        assert len(j.query(severity="error")) == 1
        assert len(j.query(severity="info")) == 1

    def test_query_newest_first(self):
        j = self._journal()
        t0 = time.monotonic()
        j.append(StateChangeRecord(type=StateChangeType.device_online, timestamp=t0 + 0, device_id="a"))
        j.append(StateChangeRecord(type=StateChangeType.device_offline, timestamp=t0 + 1, device_id="b"))
        results = j.query(limit=10)
        # Newest first
        assert results[0].device_id == "b"
        assert results[1].device_id == "a"

    def test_query_limit(self):
        j = self._journal()
        for i in range(20):
            j.append(StateChangeRecord(type=StateChangeType.device_online, device_id=f"dev-{i}"))
        results = j.query(limit=5)
        assert len(results) == 5

    def test_query_offset(self):
        j = self._journal()
        for i in range(10):
            j.append(StateChangeRecord(type=StateChangeType.device_online, device_id=f"dev-{i}"))
        r1 = j.query(limit=5, offset=0)
        r2 = j.query(limit=5, offset=5)
        ids1 = {r.device_id for r in r1}
        ids2 = {r.device_id for r in r2}
        assert ids1.isdisjoint(ids2)

    def test_trim_max_entries(self):
        j = self._journal(max_entries=3)
        for i in range(5):
            j.append(StateChangeRecord(type=StateChangeType.device_online, device_id=f"dev-{i}"))
        assert len(j) == 3

    def test_trim_max_age(self):
        j = self._journal(max_age_s=60)
        old_time = time.monotonic() - 200.0
        j.append(StateChangeRecord(type=StateChangeType.device_online, timestamp=old_time))
        j.append(StateChangeRecord(type=StateChangeType.device_offline))  # triggers trim
        assert len(j) == 1

    def test_dedup_suppresses_consecutive_same_event(self):
        j = self._journal(dedup_window_s=5.0)
        t = time.monotonic()
        j.append(StateChangeRecord(type=StateChangeType.link_flapping, device_id="x"), now=t)
        j.append(StateChangeRecord(type=StateChangeType.link_flapping, device_id="x"), now=t + 0.5)
        # Second event is within 5s dedup window and same key — suppressed
        assert len(j) == 1

    def test_dedup_allows_after_window_expired(self):
        j = self._journal(dedup_window_s=1.0)
        t = time.monotonic()
        j.append(StateChangeRecord(type=StateChangeType.link_flapping, device_id="x"), now=t)
        j.append(StateChangeRecord(type=StateChangeType.link_flapping, device_id="x"), now=t + 2.0)
        assert len(j) == 2

    def test_dedup_different_devices_not_suppressed(self):
        j = self._journal(dedup_window_s=5.0)
        t = time.monotonic()
        j.append(StateChangeRecord(type=StateChangeType.device_online, device_id="dev-1"), now=t)
        j.append(StateChangeRecord(type=StateChangeType.device_online, device_id="dev-2"), now=t + 0.1)
        assert len(j) == 2

    def test_query_since_filter(self):
        j = self._journal()
        t = time.monotonic()
        j.append(StateChangeRecord(type=StateChangeType.device_online, timestamp=t - 100))
        j.append(StateChangeRecord(type=StateChangeType.device_offline, timestamp=t - 10))
        results = j.query(since=t - 50)
        assert len(results) == 1

    def test_query_until_filter(self):
        j = self._journal()
        t = time.monotonic()
        j.append(StateChangeRecord(type=StateChangeType.device_online, timestamp=t - 100))
        j.append(StateChangeRecord(type=StateChangeType.device_offline, timestamp=t - 10))
        results = j.query(until=t - 50)
        assert len(results) == 1

    def test_to_dict(self):
        j = self._journal()
        d = j.to_dict()
        assert "count" in d
        assert "policy" in d


# ── TrendAlertType ────────────────────────────────────────────────────────────

class TestTrendAlertType:
    def test_five_types(self):
        assert len(TrendAlertType) == 5

    def test_values(self):
        vals = {t.value for t in TrendAlertType}
        assert "degradation" in vals
        assert "capacity_exhaustion" in vals
        assert "anomaly" in vals


# ── TrendAlert ────────────────────────────────────────────────────────────────

class TestTrendAlert:
    def test_defaults(self):
        a = TrendAlert(
            type=TrendAlertType.degradation,
            device_id="dev-1",
            metric_key="latency_ms",
        )
        assert a.active
        assert not a.acknowledged

    def test_resolve(self):
        a = TrendAlert(type=TrendAlertType.anomaly, device_id="dev-1", metric_key="x")
        assert a.active
        a.resolve()
        assert not a.active

    def test_acknowledge(self):
        a = TrendAlert(type=TrendAlertType.degradation, device_id="dev-1", metric_key="x")
        a.acknowledge()
        assert a.acknowledged

    def test_to_dict(self):
        a = TrendAlert(
            type=TrendAlertType.capacity_exhaustion,
            device_id="dev-1",
            metric_key="bandwidth_bps",
            message="approaching limit",
        )
        d = a.to_dict()
        assert d["type"] == "capacity_exhaustion"
        assert d["device_id"] == "dev-1"
        assert d["active"] is True
        assert "raised_at" in d


# ── TrendAlertManager ─────────────────────────────────────────────────────────

class TestTrendAlertManager:
    def test_raise_and_active(self):
        mgr = TrendAlertManager()
        a = TrendAlert(type=TrendAlertType.degradation, device_id="dev-1", metric_key="x")
        mgr.raise_alert(a)
        assert len(mgr.active_alerts()) == 1

    def test_resolve_removes_from_active(self):
        mgr = TrendAlertManager()
        a = TrendAlert(type=TrendAlertType.degradation, device_id="dev-1", metric_key="x")
        mgr.raise_alert(a)
        assert mgr.resolve(a.id)
        assert len(mgr.active_alerts()) == 0

    def test_resolve_unknown_returns_false(self):
        mgr = TrendAlertManager()
        assert not mgr.resolve("no-such-id")

    def test_acknowledge(self):
        mgr = TrendAlertManager()
        a = TrendAlert(type=TrendAlertType.anomaly, device_id="dev-1", metric_key="x")
        mgr.raise_alert(a)
        assert mgr.acknowledge(a.id)
        assert mgr.get(a.id).acknowledged

    def test_all_alerts_includes_resolved(self):
        mgr = TrendAlertManager()
        a = TrendAlert(type=TrendAlertType.degradation, device_id="dev-1", metric_key="x")
        mgr.raise_alert(a)
        mgr.resolve(a.id)
        assert len(mgr.all_alerts()) == 1
        assert len(mgr.active_alerts()) == 0

    def test_multiple_alerts(self):
        mgr = TrendAlertManager()
        for i in range(3):
            mgr.raise_alert(TrendAlert(
                type=TrendAlertType.degradation,
                device_id=f"dev-{i}",
                metric_key="x",
            ))
        assert len(mgr.active_alerts()) == 3


# ── MetricRetention ───────────────────────────────────────────────────────────

class TestMetricRetention:
    def test_defaults(self):
        r = MetricRetention()
        assert r.tier1_resolution_s == 1.0
        assert r.tier2_resolution_s == 60.0
        assert r.tier3_resolution_s == 900.0

    def test_max_points(self):
        r = MetricRetention()
        assert r.tier1_max_points == 3600   # 3600s / 1s
        assert r.tier2_max_points == 1440   # 86400s / 60s
        assert r.tier3_max_points == 2880   # 2592000s / 900s

    def test_to_dict(self):
        d = MetricRetention().to_dict()
        assert "tier1" in d
        assert "tier2" in d
        assert "tier3" in d
        assert d["tier1"]["max_points"] == 3600


# ── MetricPoint ───────────────────────────────────────────────────────────────

class TestMetricPoint:
    def test_to_dict(self):
        mp = MetricPoint(timestamp=100.0, value=42.0, count=5,
                         min_val=38.0, max_val=47.0)
        d = mp.to_dict()
        assert d["v"] == 42.0
        assert d["n"] == 5
        assert d["min"] == 38.0


# ── MetricSeries ──────────────────────────────────────────────────────────────

class TestMetricSeries:
    def test_record_single(self):
        s = MetricSeries()
        s.record(42.0, now=100.0)
        assert len(s._tier1) == 1
        assert s._tier1[0].value == 42.0

    def test_recent_returns_within_window(self):
        s = MetricSeries()
        t = time.monotonic()
        s.record(1.0, now=t - 30)
        s.record(2.0, now=t - 10)
        s.record(3.0, now=t - 5)
        recent = s.recent(seconds=20, now=t)
        assert len(recent) == 2
        vals = {p.value for p in recent}
        assert vals == {2.0, 3.0}

    def test_tier1_trims_old_points(self):
        ret = MetricRetention(tier1_retain_s=10.0)
        s = MetricSeries(retention=ret)
        t = 1000.0
        for i in range(5):
            s.record(float(i), now=t - 50.0 + i)  # all older than 10s
        s.record(999.0, now=t)  # triggers trim
        # all old points should be trimmed
        for p in s._tier1:
            assert p.timestamp >= (t - 10.0)

    def test_tier2_flush_aggregates(self):
        ret = MetricRetention(
            tier1_retain_s=3600.0,
            tier2_resolution_s=5.0,  # flush every 5s
        )
        s = MetricSeries(retention=ret)
        t = 1000.0  # non-zero base (0.0 is falsy)
        s.record(10.0, now=t)       # sets _last_t2_flush = t
        s.record(20.0, now=t + 1.0)
        s.record(30.0, now=t + 2.0)
        # Now advance past the tier2 resolution
        s.record(40.0, now=t + 6.0)  # triggers flush; includes 10,20,30,40
        assert len(s._tier2) >= 1
        agg = s._tier2[0]
        assert agg.min_val == 10.0
        assert agg.max_val == 40.0
        assert agg.count == 4

    def test_to_dict(self):
        s = MetricSeries()
        d = s.to_dict()
        assert "tier1_count" in d
        assert "tier2_count" in d
        assert "retention" in d


# ── MetricStore ───────────────────────────────────────────────────────────────

class TestMetricStore:
    def test_record_and_series(self):
        store = MetricStore()
        store.record("dev-1", "latency_ms", 12.5)
        store.record("dev-1", "latency_ms", 13.0)
        s = store.get_series("dev-1", "latency_ms")
        assert s is not None
        assert len(s._tier1) == 2

    def test_get_nonexistent_returns_none(self):
        store = MetricStore()
        assert store.get_series("dev-x", "no_such_key") is None

    def test_device_series_keys(self):
        store = MetricStore()
        store.record("dev-1", "latency_ms", 1.0)
        store.record("dev-1", "jitter_ms", 0.5)
        store.record("dev-2", "latency_ms", 2.0)
        keys = store.device_series_keys("dev-1")
        assert "latency_ms" in keys
        assert "jitter_ms" in keys
        assert len(keys) == 2

    def test_to_dict(self):
        store = MetricStore()
        store.record("dev-1", "x", 1.0)
        d = store.to_dict()
        assert d["series_count"] == 1
        assert "retention" in d
