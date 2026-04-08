# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Unit tests for the active measurement and quality decay system (Phase 5)."""

from __future__ import annotations

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent.parent.parent.parent / "controller"))

import pytest
from routing.model import InfoQuality
from routing.measurement import (
    ClassFreshness,
    DeviceFreshness,
    MeasurementStore,
    QualifiedValue,
    RefreshClass,
    RefreshCost,
    STANDARD_REFRESH_CLASSES,
    StalenessAction,
    StalenessPolicy,
    _degrade_once,
    _degrade_twice,
    get_refresh_class,
)

pytestmark = pytest.mark.unit


# ── _degrade_once / _degrade_twice ────────────────────────────────────────────

class TestDegrade:
    def test_user_degrades_to_measured(self):
        assert _degrade_once(InfoQuality.user) == InfoQuality.measured

    def test_measured_degrades_to_inferred(self):
        assert _degrade_once(InfoQuality.measured) == InfoQuality.inferred

    def test_inferred_degrades_to_reported(self):
        assert _degrade_once(InfoQuality.inferred) == InfoQuality.reported

    def test_reported_degrades_to_commanded(self):
        assert _degrade_once(InfoQuality.reported) == InfoQuality.commanded

    def test_assumed_does_not_degrade_below_assumed(self):
        assert _degrade_once(InfoQuality.assumed) == InfoQuality.assumed

    def test_degrade_twice_from_user(self):
        assert _degrade_twice(InfoQuality.user) == InfoQuality.inferred

    def test_degrade_twice_from_measured(self):
        assert _degrade_twice(InfoQuality.measured) == InfoQuality.reported

    def test_degrade_twice_floors_at_assumed(self):
        assert _degrade_twice(InfoQuality.spec) == InfoQuality.assumed


# ── QualifiedValue ────────────────────────────────────────────────────────────

class TestQualifiedValue:
    def _now(self) -> float:
        import time
        return time.monotonic()

    def test_defaults(self):
        qv = QualifiedValue(value=42)
        assert qv.quality == InfoQuality.assumed
        assert qv.source == ""
        assert qv.measured_at is None

    def test_age_none_if_no_timestamp(self):
        qv = QualifiedValue(value=1.0)
        assert qv.age_seconds() is None

    def test_age_approximate(self):
        import time
        t0 = time.monotonic()
        qv = QualifiedValue(value=1.0, measured_at=t0 - 10.0)
        age = qv.age_seconds()
        assert 9.9 < age < 10.5

    def test_effective_quality_fresh_unchanged(self):
        import time
        qv = QualifiedValue(
            value=100,
            quality=InfoQuality.measured,
            measured_at=time.monotonic() - 5.0,  # 5s old, fresh_threshold=30
        )
        eq = qv.effective_quality(fresh_threshold_s=30, stale_threshold_s=120)
        assert eq == InfoQuality.measured

    def test_effective_quality_stale_degrades_once(self):
        import time
        qv = QualifiedValue(
            value=100,
            quality=InfoQuality.measured,
            measured_at=time.monotonic() - 150.0,  # between stale(120) and expired(240)
        )
        eq = qv.effective_quality(fresh_threshold_s=30, stale_threshold_s=120)
        assert eq == InfoQuality.inferred

    def test_effective_quality_expired_degrades_twice(self):
        import time
        qv = QualifiedValue(
            value=100,
            quality=InfoQuality.measured,
            measured_at=time.monotonic() - 300.0,  # beyond expired(240)
        )
        eq = qv.effective_quality(fresh_threshold_s=30, stale_threshold_s=120)
        assert eq == InfoQuality.reported

    def test_commanded_never_decays(self):
        import time
        qv = QualifiedValue(
            value="on",
            quality=InfoQuality.commanded,
            measured_at=time.monotonic() - 9999.0,
        )
        eq = qv.effective_quality(fresh_threshold_s=1, stale_threshold_s=2)
        assert eq == InfoQuality.commanded

    def test_no_timestamp_returns_base_quality(self):
        qv = QualifiedValue(value=1.0, quality=InfoQuality.spec)
        eq = qv.effective_quality(fresh_threshold_s=10, stale_threshold_s=60)
        assert eq == InfoQuality.spec

    def test_to_dict(self):
        qv = QualifiedValue(value=42, quality=InfoQuality.measured, source="iperf")
        d = qv.to_dict()
        assert d["value"] == 42
        assert d["quality"] == "measured"
        assert d["source"] == "iperf"

    def test_between_fresh_and_stale_unchanged(self):
        """Values between fresh_threshold and stale_threshold are still base quality."""
        import time
        qv = QualifiedValue(
            value=1.0,
            quality=InfoQuality.user,
            measured_at=time.monotonic() - 20.0,  # past fresh(15) but before stale(60)
        )
        eq = qv.effective_quality(fresh_threshold_s=15, stale_threshold_s=60)
        assert eq == InfoQuality.user


# ── StalenessPolicy ───────────────────────────────────────────────────────────

class TestStalenessPolicy:
    def test_defaults(self):
        sp = StalenessPolicy()
        assert sp.fresh_threshold_s == 15.0
        assert sp.stale_threshold_s == 60.0

    def test_effective_expired_is_double_stale_by_default(self):
        sp = StalenessPolicy(stale_threshold_s=100)
        assert sp.effective_expired_threshold_s == 200.0

    def test_effective_expired_override(self):
        sp = StalenessPolicy(stale_threshold_s=100, expired_threshold_s=250)
        assert sp.effective_expired_threshold_s == 250.0

    def test_state_for_age_fresh(self):
        sp = StalenessPolicy(fresh_threshold_s=10, stale_threshold_s=60)
        assert sp.state_for_age(5.0) == "fresh"

    def test_state_for_age_stale(self):
        sp = StalenessPolicy(fresh_threshold_s=10, stale_threshold_s=60)
        assert sp.state_for_age(30.0) == "stale"

    def test_state_for_age_expired(self):
        sp = StalenessPolicy(fresh_threshold_s=10, stale_threshold_s=60)
        assert sp.state_for_age(100.0) == "expired"

    def test_to_dict(self):
        sp = StalenessPolicy()
        d = sp.to_dict()
        assert "fresh_threshold_s" in d
        assert "action_on_stale" in d
        assert d["action_on_expired"] == "degrade_quality"


# ── RefreshCost ───────────────────────────────────────────────────────────────

class TestRefreshCost:
    def test_defaults(self):
        rc = RefreshCost()
        assert rc.cpu_impact == "negligible"
        assert rc.io_impact == "none"

    def test_to_dict(self):
        rc = RefreshCost(cpu_impact="moderate", io_impact="network")
        d = rc.to_dict()
        assert d["cpu_impact"] == "moderate"
        assert d["io_impact"] == "network"


# ── RefreshClass ──────────────────────────────────────────────────────────────

class TestRefreshClass:
    def _rc(self, interval=10.0, min_interval=1.0, adaptive=True):
        return RefreshClass(
            id="test_class",
            name="Test",
            default_interval_s=interval,
            min_interval_s=min_interval,
            adaptive=adaptive,
        )

    def test_default_interval_unchanged_when_no_flags(self):
        rc = self._rc(interval=10.0)
        assert rc.adapted_interval_s() == 10.0

    def test_under_pressure_5x_faster(self):
        rc = self._rc(interval=10.0, min_interval=0.1)
        assert rc.adapted_interval_s(under_pressure=True) == pytest.approx(2.0)

    def test_idle_10x_slower(self):
        rc = self._rc(interval=10.0)
        assert rc.adapted_interval_s(idle=True) == pytest.approx(100.0)

    def test_active_pipeline_2x_faster(self):
        rc = self._rc(interval=10.0, min_interval=0.1)
        assert rc.adapted_interval_s(in_active_pipeline=True) == pytest.approx(5.0)

    def test_on_battery_3x_slower(self):
        rc = self._rc(interval=10.0)
        assert rc.adapted_interval_s(on_battery=True) == pytest.approx(30.0)

    def test_pressure_and_active_pipeline_combine(self):
        # 10 / 5 / 2 = 1.0
        rc = self._rc(interval=10.0, min_interval=0.05)
        assert rc.adapted_interval_s(under_pressure=True, in_active_pipeline=True) == pytest.approx(1.0)

    def test_never_below_min_interval(self):
        rc = self._rc(interval=1.0, min_interval=0.5)
        # under_pressure: 1.0/5 = 0.2, but min is 0.5
        assert rc.adapted_interval_s(under_pressure=True) == pytest.approx(0.5)

    def test_non_adaptive_always_returns_default(self):
        rc = self._rc(interval=3600.0, adaptive=False)
        assert rc.adapted_interval_s(under_pressure=True, in_active_pipeline=True) == 3600.0

    def test_to_dict(self):
        rc = self._rc()
        d = rc.to_dict()
        assert d["id"] == "test_class"
        assert "staleness" in d
        assert "cost" in d


# ── STANDARD_REFRESH_CLASSES ──────────────────────────────────────────────────

class TestStandardRefreshClasses:
    def test_all_thirteen_defined(self):
        expected = [
            "realtime_metrics", "network_health", "link_bandwidth",
            "usb_topology", "pcie_topology", "smart_health",
            "sfp_dom", "power_rails", "bluetooth_connection",
            "wifi_signal", "thermal_zone", "firmware_versions",
            "room_occupancy",
        ]
        for k in expected:
            assert k in STANDARD_REFRESH_CLASSES, f"Missing: {k}"

    def test_realtime_metrics_fast(self):
        rc = STANDARD_REFRESH_CLASSES["realtime_metrics"]
        assert rc.default_interval_s <= 2.0

    def test_firmware_versions_daily(self):
        rc = STANDARD_REFRESH_CLASSES["firmware_versions"]
        assert rc.default_interval_s >= 86400.0
        assert not rc.adaptive

    def test_smart_health_not_adaptive(self):
        rc = STANDARD_REFRESH_CLASSES["smart_health"]
        assert not rc.adaptive

    def test_pcie_topology_slow(self):
        rc = STANDARD_REFRESH_CLASSES["pcie_topology"]
        assert rc.default_interval_s >= 3600.0

    def test_get_refresh_class_known(self):
        rc = get_refresh_class("network_health")
        assert rc is not None
        assert rc.id == "network_health"

    def test_get_refresh_class_unknown_returns_none(self):
        assert get_refresh_class("nonexistent_class") is None


# ── ClassFreshness ────────────────────────────────────────────────────────────

class TestClassFreshness:
    def test_initial_state_never_collected(self):
        cf = ClassFreshness(class_id="network_health")
        assert cf.state == "never_collected"
        assert cf.age_s() is None

    def test_update_sets_last_refreshed(self):
        import time
        cf = ClassFreshness(class_id="network_health")
        t = time.monotonic()
        cf.update(now=t)
        assert cf.last_refreshed == t
        assert cf.error is None

    def test_refresh_state_fresh(self):
        import time
        policy = StalenessPolicy(fresh_threshold_s=30, stale_threshold_s=120)
        cf = ClassFreshness(class_id="x", last_refreshed=time.monotonic() - 5.0)
        assert cf.refresh_state(policy) == "fresh"

    def test_refresh_state_stale(self):
        import time
        policy = StalenessPolicy(fresh_threshold_s=10, stale_threshold_s=60)
        cf = ClassFreshness(class_id="x", last_refreshed=time.monotonic() - 40.0)
        assert cf.refresh_state(policy) == "stale"

    def test_refresh_state_expired(self):
        import time
        policy = StalenessPolicy(fresh_threshold_s=10, stale_threshold_s=60)
        cf = ClassFreshness(class_id="x", last_refreshed=time.monotonic() - 200.0)
        assert cf.refresh_state(policy) == "expired"

    def test_refresh_state_never_collected_if_no_timestamp(self):
        policy = StalenessPolicy()
        cf = ClassFreshness(class_id="x")
        assert cf.refresh_state(policy) == "never_collected"

    def test_to_dict(self):
        cf = ClassFreshness(class_id="sfp_dom")
        d = cf.to_dict()
        assert d["class_id"] == "sfp_dom"
        assert d["state"] == "never_collected"


# ── DeviceFreshness ───────────────────────────────────────────────────────────

class TestDeviceFreshness:
    def test_initial_worst_state_fresh_with_no_classes(self):
        df = DeviceFreshness()
        # No classes tracked → worst_state = "fresh" (vacuously)
        assert df.worst_state() == "fresh"

    def test_mark_refreshed_creates_class(self):
        import time
        df = DeviceFreshness()
        df.mark_refreshed("network_health")
        cf = df.per_class["network_health"]
        assert cf.state == "fresh"

    def test_mark_error_sets_error(self):
        df = DeviceFreshness()
        df.mark_error("wifi_signal", "timeout")
        assert df.per_class["wifi_signal"].error == "timeout"

    def test_update_states_transitions_to_stale(self):
        import time
        df = DeviceFreshness()
        # Manually set class as freshly updated a long time ago
        t_old = time.monotonic() - 500.0
        df.per_class["network_health"] = ClassFreshness(
            class_id="network_health",
            last_refreshed=t_old,
            state="fresh",
        )
        df.update_states()
        # network_health: fresh=30, stale=120 → 500s = expired
        assert df.per_class["network_health"].state == "expired"

    def test_worst_state_ordering(self):
        df = DeviceFreshness()
        df.per_class["a"] = ClassFreshness(class_id="a", state="fresh")
        df.per_class["b"] = ClassFreshness(class_id="b", state="stale")
        assert df.worst_state() == "stale"

    def test_worst_state_never_collected_is_worst(self):
        df = DeviceFreshness()
        df.per_class["a"] = ClassFreshness(class_id="a", state="expired")
        df.per_class["b"] = ClassFreshness(class_id="b", state="never_collected")
        assert df.worst_state() == "never_collected"

    def test_to_dict(self):
        df = DeviceFreshness()
        df.mark_refreshed("realtime_metrics")
        d = df.to_dict()
        assert "per_class" in d
        assert "worst_state" in d
        assert d["online"] is True


# ── MeasurementStore ──────────────────────────────────────────────────────────

class TestMeasurementStore:
    def test_record_and_get_basic(self):
        store = MeasurementStore()
        qv = store.record("dev-1", "latency_ms", 12.5, InfoQuality.measured, source="ping")
        assert qv.value == 12.5
        assert qv.quality == InfoQuality.measured
        retrieved = store.get("dev-1", "latency_ms", apply_decay=False)
        assert retrieved is not None
        assert retrieved.value == 12.5

    def test_get_nonexistent_returns_none(self):
        store = MeasurementStore()
        assert store.get("dev-x", "no_such_key") is None

    def test_record_updates_device_freshness(self):
        store = MeasurementStore()
        store.record("dev-1", "rtt", 5.0, InfoQuality.measured, refresh_class="network_health")
        freshness = store.get_device_freshness("dev-1")
        assert freshness is not None
        assert "network_health" in freshness.per_class

    def test_no_refresh_class_no_decay(self):
        import time
        store = MeasurementStore()
        old_time = time.monotonic() - 9999.0
        qv = QualifiedValue(
            value=1.0, quality=InfoQuality.measured, measured_at=old_time
        )
        store._values[("dev-1", "x")] = qv
        result = store.get("dev-1", "x", apply_decay=True)
        # No refresh_class → no decay applied
        assert result.quality == InfoQuality.measured

    def test_decay_applied_for_stale_value(self):
        import time
        store = MeasurementStore()
        old_time = time.monotonic() - 200.0  # network_health: fresh=30, stale=120, expired=240
        store._values[("dev-1", "rtt")] = QualifiedValue(
            value=10.0,
            quality=InfoQuality.measured,
            measured_at=old_time,
            refresh_class="network_health",
        )
        result = store.get("dev-1", "rtt", apply_decay=True)
        # 200s > stale(120) and < expired(240): degrade once → inferred
        assert result.quality == InfoQuality.inferred

    def test_decay_applied_for_expired_value(self):
        import time
        store = MeasurementStore()
        old_time = time.monotonic() - 500.0  # network_health: expired=240
        store._values[("dev-1", "rtt")] = QualifiedValue(
            value=10.0,
            quality=InfoQuality.measured,
            measured_at=old_time,
            refresh_class="network_health",
        )
        result = store.get("dev-1", "rtt", apply_decay=True)
        # 500s > expired(240): degrade twice → reported
        assert result.quality == InfoQuality.reported

    def test_fresh_value_not_decayed(self):
        import time
        store = MeasurementStore()
        now = time.monotonic()
        store.record("dev-1", "rtt", 5.0, InfoQuality.measured,
                     refresh_class="network_health")
        result = store.get("dev-1", "rtt", apply_decay=True, now=now + 5.0)
        # 5s < fresh_threshold(30): no decay
        assert result.quality == InfoQuality.measured

    def test_all_device_ids(self):
        store = MeasurementStore()
        store.record("alpha", "x", 1, InfoQuality.measured)
        store.record("beta", "y", 2, InfoQuality.measured)
        ids = store.all_device_ids()
        assert "alpha" in ids
        assert "beta" in ids

    def test_metrics_for_device(self):
        store = MeasurementStore()
        store.record("dev-1", "rtt", 5.0, InfoQuality.measured)
        store.record("dev-1", "jitter", 0.5, InfoQuality.measured)
        store.record("dev-2", "rtt", 8.0, InfoQuality.measured)
        m = store.metrics_for_device("dev-1")
        assert "rtt" in m
        assert "jitter" in m
        assert "dev-2" not in str(m)

    def test_overwrite_metric(self):
        store = MeasurementStore()
        store.record("dev-1", "temp", 45.0, InfoQuality.measured)
        store.record("dev-1", "temp", 50.0, InfoQuality.measured)
        result = store.get("dev-1", "temp", apply_decay=False)
        assert result.value == 50.0

    def test_no_decay_flag_returns_raw_quality(self):
        import time
        store = MeasurementStore()
        old_time = time.monotonic() - 5000.0
        store._values[("dev-1", "rtt")] = QualifiedValue(
            value=5.0,
            quality=InfoQuality.measured,
            measured_at=old_time,
            refresh_class="network_health",
        )
        result = store.get("dev-1", "rtt", apply_decay=False)
        assert result.quality == InfoQuality.measured

    def test_commanded_quality_never_decays_in_store(self):
        import time
        store = MeasurementStore()
        old_time = time.monotonic() - 5000.0
        store._values[("dev-1", "display_input")] = QualifiedValue(
            value="hdmi1",
            quality=InfoQuality.commanded,
            measured_at=old_time,
            refresh_class="realtime_metrics",
        )
        result = store.get("dev-1", "display_input", apply_decay=True)
        assert result.quality == InfoQuality.commanded
