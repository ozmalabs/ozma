# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Active measurement and data quality system — Phase 5.

Implements QualifiedValue, RefreshSchedule, DeviceFreshness, and the
quality decay algorithm from docs/routing/quality.md.

Key concepts:
  - Every measured value carries provenance (QualifiedValue)
  - Values age from "fresh" through "stale" to "expired"
  - Quality degrades one level per threshold crossing
  - Refresh intervals adapt to device pressure, active pipelines, battery
  - DeviceFreshness tracks per-class staleness state per device
"""

from __future__ import annotations

import time
from dataclasses import dataclass, field
from enum import Enum
from typing import Any, Generic, TypeVar

from .model import InfoQuality

T = TypeVar("T")


# ── QualifiedValue ────────────────────────────────────────────────────────────

@dataclass
class QualifiedValue(Generic[T]):
    """
    A value annotated with its provenance and measurement metadata.

    Wraps every measured or reported property in the routing graph so that
    routing decisions can factor in data quality.
    """
    value: T
    quality: InfoQuality = InfoQuality.assumed
    source: str = ""                   # "iperf", "pw-dump", "lsusb", "user"
    measured_at: float | None = None   # epoch seconds; None for spec/assumed
    confidence: float | None = None    # 0.0–1.0 statistical confidence
    sample_count: int | None = None
    refresh_class: str | None = None

    def age_seconds(self, now: float | None = None) -> float | None:
        """Seconds since this value was measured. None if never measured."""
        if self.measured_at is None:
            return None
        return (now or time.monotonic()) - self.measured_at

    def effective_quality(
        self,
        fresh_threshold_s: float,
        stale_threshold_s: float,
        now: float | None = None,
    ) -> InfoQuality:
        """
        Apply quality decay based on age.

        Returns the degraded quality level:
          - age < fresh_threshold_s → base quality unchanged
          - age > stale_threshold_s → degrade one level
          - age > expired_threshold (2× stale) → degrade two levels

        `commanded` quality never decays.
        """
        if self.quality == InfoQuality.commanded:
            return self.quality
        age = self.age_seconds(now)
        if age is None:
            return self.quality
        expired_threshold_s = stale_threshold_s * 2
        if age <= fresh_threshold_s:
            return self.quality
        if age <= stale_threshold_s:
            return self.quality  # between fresh and stale: still base quality
        if age <= expired_threshold_s:
            return _degrade_once(self.quality)
        return _degrade_twice(self.quality)

    def to_dict(self) -> dict:
        return {
            "value": self.value,
            "quality": self.quality.value,
            "source": self.source,
            "measured_at": self.measured_at,
            "confidence": self.confidence,
            "sample_count": self.sample_count,
            "refresh_class": self.refresh_class,
        }


def _degrade_once(q: InfoQuality) -> InfoQuality:
    """Reduce quality by one trust level."""
    _order = [
        InfoQuality.user,
        InfoQuality.measured,
        InfoQuality.inferred,
        InfoQuality.reported,
        InfoQuality.commanded,
        InfoQuality.spec,
        InfoQuality.assumed,
    ]
    idx = _order.index(q)
    return _order[min(idx + 1, len(_order) - 1)]


def _degrade_twice(q: InfoQuality) -> InfoQuality:
    return _degrade_once(_degrade_once(q))


# ── Staleness policy ──────────────────────────────────────────────────────────

class StalenessAction(str, Enum):
    degrade_quality = "degrade_quality"
    flag_only = "flag_only"
    trigger_refresh = "trigger_refresh"
    remove_from_graph = "remove_from_graph"


@dataclass
class StalenessPolicy:
    fresh_threshold_s: float = 15.0
    stale_threshold_s: float = 60.0
    expired_threshold_s: float | None = None   # None → stale × 2
    action_on_stale: StalenessAction = StalenessAction.degrade_quality
    action_on_expired: StalenessAction = StalenessAction.degrade_quality

    @property
    def effective_expired_threshold_s(self) -> float:
        if self.expired_threshold_s is not None:
            return self.expired_threshold_s
        return self.stale_threshold_s * 2

    def state_for_age(self, age_s: float) -> str:
        if age_s < self.fresh_threshold_s:
            return "fresh"
        if age_s < self.stale_threshold_s:
            return "stale"
        return "expired"

    def to_dict(self) -> dict:
        return {
            "fresh_threshold_s": self.fresh_threshold_s,
            "stale_threshold_s": self.stale_threshold_s,
            "expired_threshold_s": self.effective_expired_threshold_s,
            "action_on_stale": self.action_on_stale.value,
            "action_on_expired": self.action_on_expired.value,
        }


# ── Refresh cost ──────────────────────────────────────────────────────────────

@dataclass
class RefreshCost:
    cpu_impact: str = "negligible"     # "negligible","low","moderate","high"
    io_impact: str = "none"            # "none","disk_read","disk_write","network"
    device_impact: str = "none"        # "none","wake_disk","interrupt_device","bus_contention"
    duration_ms: float | None = None
    notes: str = ""

    def to_dict(self) -> dict:
        return {
            "cpu_impact": self.cpu_impact,
            "io_impact": self.io_impact,
            "device_impact": self.device_impact,
            "duration_ms": self.duration_ms,
            "notes": self.notes,
        }


# ── Refresh class ─────────────────────────────────────────────────────────────

@dataclass
class RefreshClass:
    id: str
    name: str
    default_interval_s: float
    min_interval_s: float = 0.5
    adaptive: bool = True
    cost: RefreshCost = field(default_factory=RefreshCost)
    staleness: StalenessPolicy = field(default_factory=StalenessPolicy)
    trigger_refresh_on: list[str] = field(default_factory=list)

    def adapted_interval_s(
        self,
        under_pressure: bool = False,
        idle: bool = False,
        in_active_pipeline: bool = False,
        on_battery: bool = False,
    ) -> float:
        """
        Return the adaptive refresh interval given current conditions.

        Rules from spec:
        - under_pressure (thermal/power/resource near limit): 5× faster
        - idle: 10× slower
        - in_active_pipeline: 2× faster
        - on_battery: 3× slower
        - never below min_interval_s
        """
        if not self.adaptive:
            return self.default_interval_s
        interval = self.default_interval_s
        if under_pressure:
            interval /= 5.0
        if in_active_pipeline:
            interval /= 2.0
        if idle:
            interval *= 10.0
        if on_battery:
            interval *= 3.0
        return max(interval, self.min_interval_s)

    def to_dict(self) -> dict:
        return {
            "id": self.id,
            "name": self.name,
            "default_interval_s": self.default_interval_s,
            "min_interval_s": self.min_interval_s,
            "adaptive": self.adaptive,
            "cost": self.cost.to_dict(),
            "staleness": self.staleness.to_dict(),
        }


# ── Standard refresh schedule ─────────────────────────────────────────────────
# From docs/routing/quality.md §Standard Refresh Classes

STANDARD_REFRESH_CLASSES: dict[str, RefreshClass] = {

    "realtime_metrics": RefreshClass(
        id="realtime_metrics",
        name="Real-time metrics (CPU, memory, temp, fan)",
        default_interval_s=2.0,
        min_interval_s=0.5,
        cost=RefreshCost(cpu_impact="negligible", io_impact="none"),
        staleness=StalenessPolicy(fresh_threshold_s=15, stale_threshold_s=60,
                                  action_on_expired=StalenessAction.degrade_quality),
        trigger_refresh_on=["pipeline.activated"],
    ),

    "network_health": RefreshClass(
        id="network_health",
        name="Network health (latency, jitter, loss)",
        default_interval_s=7.5,
        min_interval_s=1.0,
        cost=RefreshCost(cpu_impact="low", io_impact="network"),
        staleness=StalenessPolicy(fresh_threshold_s=30, stale_threshold_s=120),
        trigger_refresh_on=["link.state_changed"],
    ),

    "link_bandwidth": RefreshClass(
        id="link_bandwidth",
        name="Available link bandwidth",
        default_interval_s=45.0,
        min_interval_s=5.0,
        cost=RefreshCost(cpu_impact="moderate", io_impact="network"),
        staleness=StalenessPolicy(fresh_threshold_s=300, stale_threshold_s=1800,
                                  action_on_expired=StalenessAction.degrade_quality),
    ),

    "usb_topology": RefreshClass(
        id="usb_topology",
        name="USB bus topology (lsusb -t)",
        default_interval_s=300.0,
        min_interval_s=5.0,
        cost=RefreshCost(cpu_impact="moderate", io_impact="none"),
        staleness=StalenessPolicy(fresh_threshold_s=600, stale_threshold_s=3600),
        trigger_refresh_on=["device.usb_hotplug"],
    ),

    "pcie_topology": RefreshClass(
        id="pcie_topology",
        name="PCIe device topology (lspci)",
        default_interval_s=3600.0,
        min_interval_s=60.0,
        cost=RefreshCost(cpu_impact="low", io_impact="none"),
        staleness=StalenessPolicy(fresh_threshold_s=7200, stale_threshold_s=86400),
        trigger_refresh_on=["device.boot"],
    ),

    "smart_health": RefreshClass(
        id="smart_health",
        name="Drive SMART health",
        default_interval_s=3600.0,
        min_interval_s=600.0,
        adaptive=False,
        cost=RefreshCost(cpu_impact="moderate", io_impact="disk_read",
                         device_impact="wake_disk"),
        staleness=StalenessPolicy(fresh_threshold_s=172800, stale_threshold_s=604800,
                                  action_on_expired=StalenessAction.flag_only),
    ),

    "sfp_dom": RefreshClass(
        id="sfp_dom",
        name="SFP/QSFP DOM (optical power, temp)",
        default_interval_s=30.0,
        min_interval_s=5.0,
        cost=RefreshCost(cpu_impact="low", io_impact="none",
                         device_impact="bus_contention"),
        staleness=StalenessPolicy(fresh_threshold_s=120, stale_threshold_s=600),
    ),

    "power_rails": RefreshClass(
        id="power_rails",
        name="Power rail voltage/current (INA219)",
        default_interval_s=2.0,
        min_interval_s=0.2,
        cost=RefreshCost(cpu_impact="negligible", io_impact="none"),
        staleness=StalenessPolicy(fresh_threshold_s=15, stale_threshold_s=60),
        trigger_refresh_on=["power.source_changed"],
    ),

    "bluetooth_connection": RefreshClass(
        id="bluetooth_connection",
        name="Bluetooth RSSI, codec, battery",
        default_interval_s=7.5,
        min_interval_s=1.0,
        cost=RefreshCost(cpu_impact="low", io_impact="none"),
        staleness=StalenessPolicy(fresh_threshold_s=30, stale_threshold_s=120),
        trigger_refresh_on=["bluetooth.connected", "bluetooth.disconnected"],
    ),

    "wifi_signal": RefreshClass(
        id="wifi_signal",
        name="WiFi signal quality (RSSI, channel util.)",
        default_interval_s=7.5,
        min_interval_s=1.0,
        cost=RefreshCost(cpu_impact="low", io_impact="none"),
        staleness=StalenessPolicy(fresh_threshold_s=30, stale_threshold_s=120),
        trigger_refresh_on=["wifi.roam", "wifi.reconnect"],
    ),

    "thermal_zone": RefreshClass(
        id="thermal_zone",
        name="Thermal zones (CPU, GPU, ambient)",
        default_interval_s=2.0,
        min_interval_s=0.5,
        cost=RefreshCost(cpu_impact="negligible", io_impact="none"),
        staleness=StalenessPolicy(fresh_threshold_s=15, stale_threshold_s=60),
    ),

    "firmware_versions": RefreshClass(
        id="firmware_versions",
        name="Firmware/OS version inventory",
        default_interval_s=86400.0,
        min_interval_s=300.0,
        adaptive=False,
        cost=RefreshCost(cpu_impact="low", io_impact="none"),
        staleness=StalenessPolicy(fresh_threshold_s=172800, stale_threshold_s=604800,
                                  action_on_stale=StalenessAction.flag_only),
        trigger_refresh_on=["device.boot"],
    ),

    "room_occupancy": RefreshClass(
        id="room_occupancy",
        name="Room occupancy / presence detection",
        default_interval_s=30.0,
        min_interval_s=2.0,
        cost=RefreshCost(cpu_impact="moderate", io_impact="none"),
        staleness=StalenessPolicy(fresh_threshold_s=300, stale_threshold_s=1800),
    ),
}


def get_refresh_class(class_id: str) -> RefreshClass | None:
    return STANDARD_REFRESH_CLASSES.get(class_id)


# ── Class freshness ───────────────────────────────────────────────────────────

@dataclass
class ClassFreshness:
    """Freshness state for one refresh class on one device."""
    class_id: str
    last_refreshed: float | None = None    # epoch seconds
    next_refresh: float | None = None
    state: str = "never_collected"         # "fresh", "stale", "expired", "never_collected"
    error: str | None = None

    def age_s(self, now: float | None = None) -> float | None:
        if self.last_refreshed is None:
            return None
        return (now or time.monotonic()) - self.last_refreshed

    def update(self, now: float | None = None) -> None:
        """Mark as just refreshed."""
        self.last_refreshed = now or time.monotonic()
        self.error = None

    def refresh_state(
        self,
        policy: StalenessPolicy,
        now: float | None = None,
    ) -> str:
        age = self.age_s(now)
        if age is None:
            return "never_collected"
        return policy.state_for_age(age)

    def to_dict(self) -> dict:
        return {
            "class_id": self.class_id,
            "last_refreshed": self.last_refreshed,
            "next_refresh": self.next_refresh,
            "state": self.state,
            "error": self.error,
        }


# ── Device freshness ──────────────────────────────────────────────────────────

@dataclass
class DeviceFreshness:
    """Overall freshness state for a device across all refresh classes."""
    online: bool = True
    last_contact: float = field(default_factory=time.monotonic)
    last_full_refresh: float | None = None
    per_class: dict[str, ClassFreshness] = field(default_factory=dict)

    def get_class(self, class_id: str) -> ClassFreshness:
        if class_id not in self.per_class:
            self.per_class[class_id] = ClassFreshness(class_id=class_id)
        return self.per_class[class_id]

    def mark_refreshed(self, class_id: str, now: float | None = None) -> None:
        cf = self.get_class(class_id)
        cf.update(now)
        cf.state = "fresh"
        self.last_contact = now or time.monotonic()

    def mark_error(self, class_id: str, error: str) -> None:
        cf = self.get_class(class_id)
        cf.error = error

    def update_states(
        self,
        now: float | None = None,
        policies: dict[str, StalenessPolicy] | None = None,
    ) -> None:
        """
        Recompute the freshness state for every class based on current time.

        policies: mapping from class_id → StalenessPolicy. Falls back to
        STANDARD_REFRESH_CLASSES if not provided.
        """
        t = now or time.monotonic()
        for class_id, cf in self.per_class.items():
            policy = None
            if policies:
                policy = policies.get(class_id)
            if policy is None:
                rc = STANDARD_REFRESH_CLASSES.get(class_id)
                policy = rc.staleness if rc else StalenessPolicy()
            cf.state = cf.refresh_state(policy, t)

    def worst_state(self) -> str:
        """Return the worst freshness state across all classes."""
        _rank = {"fresh": 0, "stale": 1, "expired": 2, "never_collected": 3}
        worst = "fresh"
        for cf in self.per_class.values():
            if _rank.get(cf.state, 0) > _rank.get(worst, 0):
                worst = cf.state
        return worst

    def to_dict(self) -> dict:
        return {
            "online": self.online,
            "last_contact": self.last_contact,
            "last_full_refresh": self.last_full_refresh,
            "per_class": {k: v.to_dict() for k, v in self.per_class.items()},
            "worst_state": self.worst_state(),
        }


# ── Measurement store ─────────────────────────────────────────────────────────

class MeasurementStore:
    """
    In-memory store of QualifiedValues keyed by (device_id, metric_key).

    The router queries this for quality-adjusted metric values.
    In Phase 5, this is populated by the active measurement engine.
    """

    def __init__(self) -> None:
        self._values: dict[tuple[str, str], QualifiedValue] = {}
        self._freshness: dict[str, DeviceFreshness] = {}

    def record(
        self,
        device_id: str,
        metric_key: str,
        value: Any,
        quality: InfoQuality,
        source: str = "",
        refresh_class: str | None = None,
        now: float | None = None,
    ) -> QualifiedValue:
        """Store a new measurement."""
        qv = QualifiedValue(
            value=value,
            quality=quality,
            source=source,
            measured_at=now or time.monotonic(),
            refresh_class=refresh_class,
        )
        self._values[(device_id, metric_key)] = qv
        # Update device freshness
        freshness = self._freshness.setdefault(device_id, DeviceFreshness())
        if refresh_class:
            freshness.mark_refreshed(refresh_class, now)
        return qv

    def get(
        self,
        device_id: str,
        metric_key: str,
        apply_decay: bool = True,
        now: float | None = None,
    ) -> QualifiedValue | None:
        """
        Retrieve a measurement. If apply_decay=True, quality is degraded
        based on how stale the measurement is (using standard staleness policy
        for its refresh_class, if known).
        """
        qv = self._values.get((device_id, metric_key))
        if qv is None:
            return None
        if not apply_decay or qv.refresh_class is None:
            return qv
        rc = STANDARD_REFRESH_CLASSES.get(qv.refresh_class)
        if rc is None:
            return qv
        # Return a new QV with decayed quality
        decayed_q = qv.effective_quality(
            fresh_threshold_s=rc.staleness.fresh_threshold_s,
            stale_threshold_s=rc.staleness.stale_threshold_s,
            now=now,
        )
        if decayed_q == qv.quality:
            return qv
        return QualifiedValue(
            value=qv.value,
            quality=decayed_q,
            source=qv.source,
            measured_at=qv.measured_at,
            confidence=qv.confidence,
            sample_count=qv.sample_count,
            refresh_class=qv.refresh_class,
        )

    def get_device_freshness(self, device_id: str) -> DeviceFreshness | None:
        return self._freshness.get(device_id)

    def all_device_ids(self) -> list[str]:
        return list(self._freshness.keys())

    def metrics_for_device(self, device_id: str) -> dict[str, QualifiedValue]:
        return {
            key: qv
            for (did, key), qv in self._values.items()
            if did == device_id
        }
