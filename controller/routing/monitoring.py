# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Routing monitoring, state change journal, and trend alerts — Phase 6.

Provides:
  - StateChangeRecord + StateChangeType: append-only event journal
  - MonitoringJournal: in-memory journal with retention and deduplication
  - TrendAlert + TrendAlertType: raised/resolved threshold-crossing alerts
  - MetricSeries + MetricRetention: tiered in-memory time-series storage
"""

from __future__ import annotations

import time
import uuid
from collections import deque
from dataclasses import dataclass, field
from enum import Enum
from typing import Any


# ── State change types ────────────────────────────────────────────────────────

class StateChangeType(str, Enum):
    # Device lifecycle
    device_online             = "device_online"
    device_offline            = "device_offline"
    device_degraded           = "device_degraded"
    device_recovered          = "device_recovered"

    # Link health
    link_up                   = "link_up"
    link_down                 = "link_down"
    link_degraded             = "link_degraded"
    link_recovered            = "link_recovered"
    link_flapping             = "link_flapping"

    # Pipeline lifecycle
    pipeline_activated        = "pipeline_activated"
    pipeline_deactivated      = "pipeline_deactivated"
    pipeline_rerouted         = "pipeline_rerouted"
    pipeline_format_changed   = "pipeline_format_changed"
    pipeline_quality_changed  = "pipeline_quality_changed"

    # Metric data quality
    metric_quality_degraded   = "metric_quality_degraded"
    metric_stale              = "metric_stale"
    metric_expired            = "metric_expired"
    metric_refreshed          = "metric_refreshed"

    # Intent system
    intent_bound              = "intent_bound"
    intent_unbound            = "intent_unbound"
    intent_conflict           = "intent_conflict"

    # Alerts
    alert_raised              = "alert_raised"
    alert_resolved            = "alert_resolved"
    alert_acknowledged        = "alert_acknowledged"

    # Trends
    trend_degradation         = "trend_degradation"
    trend_capacity_exhaustion = "trend_capacity_exhaustion"
    trend_anomaly             = "trend_anomaly"
    trend_recurring_failure   = "trend_recurring_failure"
    trend_lifetime_estimate   = "trend_lifetime_estimate"


# ── State change record ───────────────────────────────────────────────────────

@dataclass
class StateChangeRecord:
    """An immutable event entry in the monitoring journal."""
    type: StateChangeType
    message: str = ""
    id: str = field(default_factory=lambda: str(uuid.uuid4()))
    device_id: str | None = None
    link_id: str | None = None
    pipeline_id: str | None = None
    timestamp: float = field(default_factory=time.monotonic)
    wall_time: float = field(default_factory=time.time)
    previous: Any | None = None
    current: Any | None = None
    source: str = ""
    severity: str = "info"          # "info", "warning", "error", "critical"
    tags: list[str] = field(default_factory=list)

    def to_dict(self) -> dict:
        return {
            "id": self.id,
            "type": self.type.value,
            "message": self.message,
            "device_id": self.device_id,
            "link_id": self.link_id,
            "pipeline_id": self.pipeline_id,
            "timestamp": self.timestamp,
            "wall_time": self.wall_time,
            "previous": self.previous,
            "current": self.current,
            "source": self.source,
            "severity": self.severity,
            "tags": self.tags,
        }


# ── Journal retention policy ──────────────────────────────────────────────────

@dataclass
class JournalRetention:
    max_entries: int = 10_000
    max_age_s: float = 86400.0 * 7     # 1 week

    def to_dict(self) -> dict:
        return {
            "max_entries": self.max_entries,
            "max_age_s": self.max_age_s,
        }


@dataclass
class JournalPolicy:
    retention: JournalRetention = field(default_factory=JournalRetention)
    dedup_window_s: float = 1.0     # suppress identical consecutive events within this window

    def to_dict(self) -> dict:
        return {
            "retention": self.retention.to_dict(),
            "dedup_window_s": self.dedup_window_s,
        }


# ── Monitoring journal ────────────────────────────────────────────────────────

class MonitoringJournal:
    """
    In-memory append-only journal of StateChangeRecords.

    Supports:
    - Retention limits (max entries, max age)
    - Consecutive duplicate suppression within a dedup window
    - Filtering queries (by device, link, pipeline, type, severity, time window)
    """

    def __init__(self, policy: JournalPolicy | None = None) -> None:
        self._policy = policy or JournalPolicy()
        self._entries: deque[StateChangeRecord] = deque()
        self._last_by_key: dict[tuple, StateChangeRecord] = {}

    @property
    def policy(self) -> JournalPolicy:
        return self._policy

    def _dedup_key(self, record: StateChangeRecord) -> tuple:
        return (record.type, record.device_id, record.link_id, record.pipeline_id)

    def append(self, record: StateChangeRecord, now: float | None = None) -> StateChangeRecord:
        """
        Add an event to the journal.

        Returns the record that was stored (same object if accepted, or None if
        deduplication suppressed it — but currently always returns the record
        so callers can always log the outcome). Suppressed records are NOT
        stored in the journal but the record object is returned unchanged.
        """
        t = now or time.monotonic()
        key = self._dedup_key(record)
        last = self._last_by_key.get(key)
        if last is not None and (t - last.timestamp) < self._policy.dedup_window_s:
            return record  # duplicate — suppressed

        self._entries.append(record)
        self._last_by_key[key] = record
        self.trim(now=t)
        return record

    def trim(self, now: float | None = None) -> int:
        """Remove entries exceeding retention limits. Returns count removed."""
        t = now or time.monotonic()
        removed = 0
        max_age = self._policy.retention.max_age_s
        while self._entries and (t - self._entries[0].timestamp) > max_age:
            self._entries.popleft()
            removed += 1
        max_entries = self._policy.retention.max_entries
        while len(self._entries) > max_entries:
            self._entries.popleft()
            removed += 1
        return removed

    def query(
        self,
        *,
        device_id: str | None = None,
        link_id: str | None = None,
        pipeline_id: str | None = None,
        types: list[StateChangeType] | None = None,
        severity: str | None = None,
        since: float | None = None,
        until: float | None = None,
        limit: int = 100,
        offset: int = 0,
    ) -> list[StateChangeRecord]:
        """Filter and return journal entries, newest-first."""
        results = []
        for rec in reversed(self._entries):
            if device_id is not None and rec.device_id != device_id:
                continue
            if link_id is not None and rec.link_id != link_id:
                continue
            if pipeline_id is not None and rec.pipeline_id != pipeline_id:
                continue
            if types is not None and rec.type not in types:
                continue
            if severity is not None and rec.severity != severity:
                continue
            if since is not None and rec.timestamp < since:
                continue
            if until is not None and rec.timestamp > until:
                continue
            results.append(rec)
            if len(results) >= offset + limit:
                break
        return results[offset:offset + limit]

    def __len__(self) -> int:
        return len(self._entries)

    def to_dict(self) -> dict:
        return {
            "count": len(self._entries),
            "policy": self._policy.to_dict(),
        }


# ── Trend alerts ──────────────────────────────────────────────────────────────

class TrendAlertType(str, Enum):
    degradation          = "degradation"
    capacity_exhaustion  = "capacity_exhaustion"
    lifetime_estimate    = "lifetime_estimate"
    anomaly              = "anomaly"
    recurring_failure    = "recurring_failure"


@dataclass
class TrendAlert:
    """A raised threshold-crossing trend alert for a device metric."""
    type: TrendAlertType
    device_id: str
    metric_key: str
    message: str = ""
    id: str = field(default_factory=lambda: str(uuid.uuid4()))
    raised_at: float = field(default_factory=time.monotonic)
    resolved_at: float | None = None
    acknowledged: bool = False
    severity: str = "warning"       # "warning", "error", "critical"
    details: dict = field(default_factory=dict)

    @property
    def active(self) -> bool:
        return self.resolved_at is None

    def resolve(self, now: float | None = None) -> None:
        self.resolved_at = now or time.monotonic()

    def acknowledge(self) -> None:
        self.acknowledged = True

    def to_dict(self) -> dict:
        return {
            "id": self.id,
            "type": self.type.value,
            "device_id": self.device_id,
            "metric_key": self.metric_key,
            "message": self.message,
            "raised_at": self.raised_at,
            "resolved_at": self.resolved_at,
            "acknowledged": self.acknowledged,
            "active": self.active,
            "severity": self.severity,
            "details": self.details,
        }


class TrendAlertManager:
    """Tracks active and resolved TrendAlerts."""

    def __init__(self) -> None:
        self._alerts: dict[str, TrendAlert] = {}     # id → alert

    def raise_alert(self, alert: TrendAlert) -> TrendAlert:
        self._alerts[alert.id] = alert
        return alert

    def resolve(self, alert_id: str, now: float | None = None) -> bool:
        alert = self._alerts.get(alert_id)
        if alert is None:
            return False
        alert.resolve(now)
        return True

    def acknowledge(self, alert_id: str) -> bool:
        alert = self._alerts.get(alert_id)
        if alert is None:
            return False
        alert.acknowledge()
        return True

    def active_alerts(self) -> list[TrendAlert]:
        return [a for a in self._alerts.values() if a.active]

    def all_alerts(self) -> list[TrendAlert]:
        return list(self._alerts.values())

    def get(self, alert_id: str) -> TrendAlert | None:
        return self._alerts.get(alert_id)


# ── Metric time series ────────────────────────────────────────────────────────

@dataclass
class MetricRetention:
    """
    Tiered time-series retention policy (RRD-style).

    Tier 1: raw samples, 1-second resolution, retained for 1 hour
    Tier 2: 1-minute aggregates, retained for 1 day
    Tier 3: 15-minute aggregates, retained for 30 days
    """
    tier1_resolution_s: float = 1.0
    tier1_retain_s: float = 3600.0          # 1 hour  → ≤3600 points
    tier2_resolution_s: float = 60.0
    tier2_retain_s: float = 86400.0         # 1 day   → ≤1440 points
    tier3_resolution_s: float = 900.0
    tier3_retain_s: float = 86400.0 * 30    # 30 days → ≤2880 points

    @property
    def tier1_max_points(self) -> int:
        return int(self.tier1_retain_s / self.tier1_resolution_s)

    @property
    def tier2_max_points(self) -> int:
        return int(self.tier2_retain_s / self.tier2_resolution_s)

    @property
    def tier3_max_points(self) -> int:
        return int(self.tier3_retain_s / self.tier3_resolution_s)

    def to_dict(self) -> dict:
        return {
            "tier1": {
                "resolution_s": self.tier1_resolution_s,
                "retain_s": self.tier1_retain_s,
                "max_points": self.tier1_max_points,
            },
            "tier2": {
                "resolution_s": self.tier2_resolution_s,
                "retain_s": self.tier2_retain_s,
                "max_points": self.tier2_max_points,
            },
            "tier3": {
                "resolution_s": self.tier3_resolution_s,
                "retain_s": self.tier3_retain_s,
                "max_points": self.tier3_max_points,
            },
        }


@dataclass
class MetricPoint:
    """A single time-series data point."""
    timestamp: float
    value: float
    count: int = 1          # samples aggregated into this point
    min_val: float | None = None
    max_val: float | None = None

    def to_dict(self) -> dict:
        return {
            "t": self.timestamp,
            "v": self.value,
            "n": self.count,
            "min": self.min_val,
            "max": self.max_val,
        }


class MetricSeries:
    """
    Three-tier time series for a single (device_id, metric_key).

    Tier 1 is written directly. Tier 2 and 3 are built by aggregating
    tier 1 and tier 2 respectively when enough time has elapsed.
    """

    def __init__(self, retention: MetricRetention | None = None) -> None:
        self._retention = retention or MetricRetention()
        self._tier1: deque[MetricPoint] = deque()
        self._tier2: deque[MetricPoint] = deque()
        self._tier3: deque[MetricPoint] = deque()
        self._last_t2_flush: float | None = None
        self._last_t3_flush: float | None = None

    def record(self, value: float, now: float | None = None) -> None:
        t = now or time.monotonic()
        self._tier1.append(MetricPoint(timestamp=t, value=value))
        self._trim_tier(self._tier1, self._retention.tier1_retain_s, t)
        self._maybe_flush_tier2(t)
        self._maybe_flush_tier3(t)

    def _trim_tier(
        self, tier: deque[MetricPoint], retain_s: float, now: float
    ) -> None:
        cutoff = now - retain_s
        while tier and tier[0].timestamp < cutoff:
            tier.popleft()

    def _maybe_flush_tier2(self, now: float) -> None:
        r = self._retention.tier2_resolution_s
        if self._last_t2_flush is None:
            self._last_t2_flush = now
            return
        if now - self._last_t2_flush < r:
            return
        points = [p for p in self._tier1 if p.timestamp >= self._last_t2_flush]
        if points:
            agg = self._aggregate(points, self._last_t2_flush)
            self._tier2.append(agg)
            self._trim_tier(self._tier2, self._retention.tier2_retain_s, now)
        self._last_t2_flush = now

    def _maybe_flush_tier3(self, now: float) -> None:
        r = self._retention.tier3_resolution_s
        if self._last_t3_flush is None:
            self._last_t3_flush = now
            return
        if now - self._last_t3_flush < r:
            return
        points = [p for p in self._tier2 if p.timestamp >= self._last_t3_flush]
        if points:
            agg = self._aggregate(points, self._last_t3_flush)
            self._tier3.append(agg)
            self._trim_tier(self._tier3, self._retention.tier3_retain_s, now)
        self._last_t3_flush = now

    @staticmethod
    def _aggregate(points: list[MetricPoint], bucket_ts: float) -> MetricPoint:
        vals = [p.value for p in points]
        return MetricPoint(
            timestamp=bucket_ts,
            value=sum(vals) / len(vals),
            count=len(vals),
            min_val=min(vals),
            max_val=max(vals),
        )

    def recent(self, seconds: float, now: float | None = None) -> list[MetricPoint]:
        """Return tier-1 points within the last `seconds`."""
        t = now or time.monotonic()
        cutoff = t - seconds
        return [p for p in self._tier1 if p.timestamp >= cutoff]

    def history(
        self,
        tier: int = 1,
        limit: int | None = None,
        since: float | None = None,
        now: float | None = None,
    ) -> list[MetricPoint]:
        """
        Return data points from the requested tier (1, 2, or 3), newest first.

        tier=1: 1-second resolution, last 1 hour  (real-time monitoring)
        tier=2: 1-minute resolution, last 24 hours (recent history)
        tier=3: 15-minute resolution, last 30 days (long-term trends)

        since: monotonic timestamp; only return points newer than this.
        limit: cap the result length.
        """
        _t = now or time.monotonic()
        src: deque[MetricPoint]
        if tier == 1:
            src = self._tier1
        elif tier == 2:
            src = self._tier2
        elif tier == 3:
            src = self._tier3
        else:
            raise ValueError(f"tier must be 1, 2, or 3 (got {tier})")
        points = list(src)
        if since is not None:
            points = [p for p in points if p.timestamp > since]
        points.sort(key=lambda p: p.timestamp, reverse=True)
        if limit is not None:
            points = points[:limit]
        return points

    def to_dict(self) -> dict:
        return {
            "tier1_count": len(self._tier1),
            "tier2_count": len(self._tier2),
            "tier3_count": len(self._tier3),
            "retention": self._retention.to_dict(),
        }


class MetricStore:
    """Registry of MetricSeries objects keyed by (device_id, metric_key)."""

    def __init__(self, retention: MetricRetention | None = None) -> None:
        self._retention = retention or MetricRetention()
        self._series: dict[tuple[str, str], MetricSeries] = {}

    def record(
        self, device_id: str, metric_key: str, value: float, now: float | None = None
    ) -> None:
        key = (device_id, metric_key)
        if key not in self._series:
            self._series[key] = MetricSeries(self._retention)
        self._series[key].record(value, now)

    def get_series(self, device_id: str, metric_key: str) -> MetricSeries | None:
        return self._series.get((device_id, metric_key))

    def device_series_keys(self, device_id: str) -> list[str]:
        return [k for (did, k) in self._series if did == device_id]

    def to_dict(self) -> dict:
        return {
            "series_count": len(self._series),
            "retention": self._retention.to_dict(),
        }
