# SPDX-License-Identifier: MIT
"""Unit tests for ecosystem/ha-ozma timer.py — parse_duration, Timer state machine."""
import asyncio
import sys
import time
from pathlib import Path
from types import ModuleType
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

pytestmark = pytest.mark.unit

# ── Stub homeassistant imports (not installed in dev/CI) ─────────────────────

def _stub_ha_modules():
    ha_mod = ModuleType("homeassistant")
    ha_mod.__path__ = []  # mark as package so sub-imports work
    sys.modules["homeassistant"] = ha_mod

    core_mod = ModuleType("homeassistant.core")
    core_mod.HomeAssistant = object
    core_mod.callback = lambda fn: fn
    sys.modules["homeassistant.core"] = core_mod

    entries_mod = ModuleType("homeassistant.config_entries")
    entries_mod.ConfigEntry = object
    sys.modules["homeassistant.config_entries"] = entries_mod

    helpers_mod = ModuleType("homeassistant.helpers")
    helpers_mod.__path__ = []
    sys.modules["homeassistant.helpers"] = helpers_mod

    platform_mod = ModuleType("homeassistant.helpers.entity_platform")
    platform_mod.AddEntitiesCallback = object
    sys.modules["homeassistant.helpers.entity_platform"] = platform_mod

    restore_mod = ModuleType("homeassistant.helpers.restore_state")

    class RestoreEntity:
        hass: object = None
        _attr_should_poll = False
        _attr_has_entity_name = True
        _attr_unique_id: str = ""
        _attr_name: str = ""

        async def async_get_last_state(self):
            return None

        def async_write_ha_state(self):
            pass

    restore_mod.RestoreEntity = RestoreEntity
    sys.modules["homeassistant.helpers.restore_state"] = restore_mod

_stub_ha_modules()

# ── Now import the module under test ─────────────────────────────────────────

import importlib.util as _ilu

_HA_OZMA = Path(__file__).parent.parent.parent.parent / "ecosystem" / "ha-ozma" / "custom_components" / "ozma"

# Load const.py directly (no relative imports there)
_const_spec = _ilu.spec_from_file_location("ozma_const", _HA_OZMA / "const.py")
_const_mod = _ilu.module_from_spec(_const_spec)
sys.modules["ozma_const"] = _const_mod
# Also register as the relative name timer.py will try to resolve
sys.modules["ozma"] = _const_mod   # placeholder; only .const matters
sys.modules["ozma.const"] = _const_mod
_const_spec.loader.exec_module(_const_mod)

# Load timer.py — it does `from .const import ...` which needs ozma.const in sys.modules
_timer_spec = _ilu.spec_from_file_location("ozma.timer", _HA_OZMA / "timer.py",
                                            submodule_search_locations=[])
_timer_mod = _ilu.module_from_spec(_timer_spec)
_timer_mod.__package__ = "ozma"
sys.modules["ozma.timer"] = _timer_mod
_timer_spec.loader.exec_module(_timer_mod)

parse_duration = _timer_mod.parse_duration
fmt_duration   = _timer_mod.fmt_duration
Timer          = _timer_mod.Timer

TIMER_STATE_IDLE     = _const_mod.TIMER_STATE_IDLE
TIMER_STATE_ACTIVE   = _const_mod.TIMER_STATE_ACTIVE
TIMER_STATE_PAUSED   = _const_mod.TIMER_STATE_PAUSED
TIMER_STATE_FINISHED = _const_mod.TIMER_STATE_FINISHED


# ── parse_duration ────────────────────────────────────────────────────────────

class TestParseDuration:
    def test_integer_seconds(self):
        assert parse_duration(90) == 90

    def test_float_rounds(self):
        assert parse_duration(5.7) == 6

    def test_float_rounds_down(self):
        assert parse_duration(4.2) == 4

    def test_string_hh_mm(self):
        # Colon format is HH:MM — "8:30" = 8 hours 30 minutes
        assert parse_duration("8:30") == 8 * 3600 + 30 * 60

    def test_string_hh_mm_ss(self):
        assert parse_duration("1:30:00") == 5400

    def test_string_zero_hh_mm_ss_is_minutes_seconds(self):
        # "0:08:30" = 0h 8m 30s = 510s
        assert parse_duration("0:08:30") == 510

    def test_shorthand_minutes(self):
        assert parse_duration("8m") == 480

    def test_shorthand_hours(self):
        assert parse_duration("1h") == 3600

    def test_shorthand_seconds(self):
        assert parse_duration("90s") == 90

    def test_shorthand_combined(self):
        assert parse_duration("1h30m") == 5400

    def test_shorthand_combined_with_seconds(self):
        assert parse_duration("1h30m45s") == 5445

    def test_shorthand_case_insensitive(self):
        assert parse_duration("5M") == 300

    def test_string_integer_as_seconds(self):
        # plain integer string falls through to shorthand parser — no match → ValueError
        # but "300" has no unit letter so raises
        with pytest.raises(ValueError):
            parse_duration("300")

    def test_invalid_raises_value_error(self):
        with pytest.raises(ValueError):
            parse_duration("not-a-duration")

    def test_zero_seconds(self):
        assert parse_duration(0) == 0

    def test_float_string_not_accepted(self):
        # "5.5" has no unit letter and no colon — should raise
        with pytest.raises(ValueError):
            parse_duration("5.5")


# ── fmt_duration ──────────────────────────────────────────────────────────────

class TestFmtDuration:
    def test_under_one_minute(self):
        assert fmt_duration(45) == "0:45"

    def test_exact_minutes(self):
        assert fmt_duration(180) == "3:00"

    def test_minutes_and_seconds(self):
        assert fmt_duration(90) == "1:30"

    def test_one_hour(self):
        assert fmt_duration(3600) == "1:00:00"

    def test_hours_minutes_seconds(self):
        assert fmt_duration(3723) == "1:02:03"

    def test_zero(self):
        assert fmt_duration(0) == "0:00"

    def test_negative_treated_as_positive(self):
        assert fmt_duration(-90) == "1:30"


# ── Timer state machine ───────────────────────────────────────────────────────

def _make_hass():
    hass = MagicMock()
    hass.bus = MagicMock()
    hass.bus.async_fire = MagicMock()
    hass.async_create_task = MagicMock(side_effect=lambda coro, name=None: asyncio.ensure_future(coro))
    return hass


def _make_timer(timer_id="pasta", coordinator=None):
    hass = _make_hass()
    t = Timer(hass=hass, timer_id=timer_id, coordinator=coordinator)
    return t, hass


class TestTimerConfigure:
    def test_configure_sets_state_idle(self):
        t, _ = _make_timer()
        t.configure(duration_s=300, label="Pasta")
        assert t.state == TIMER_STATE_IDLE

    def test_configure_sets_duration(self):
        t, _ = _make_timer()
        t.configure(duration_s=300, label="Pasta")
        assert t._d.duration_s == 300

    def test_configure_sets_label(self):
        t, _ = _make_timer()
        t.configure(duration_s=300, label="Pasta")
        assert t._d.label == "Pasta"

    def test_configure_resets_remaining_to_duration(self):
        t, _ = _make_timer()
        t.configure(duration_s=480, label="x")
        assert t._d.remaining_s == 480

    def test_reconfigure_cancels_running_tick(self):
        t, _ = _make_timer()
        mock_task = MagicMock()
        t._tick = mock_task
        t.configure(duration_s=60, label="x")
        mock_task.cancel.assert_called_once()


class TestTimerStart:
    def test_start_changes_state_to_active(self):
        t, _ = _make_timer()
        t.configure(300, "x")
        t.start()
        assert t.state == TIMER_STATE_ACTIVE

    def test_start_fires_timer_started_event(self):
        t, hass = _make_timer()
        t.configure(300, "Pasta")
        t.start()
        hass.bus.async_fire.assert_called()
        fired_event = hass.bus.async_fire.call_args[0][0]
        assert fired_event == "timer_started"

    def test_start_event_includes_duration(self):
        t, hass = _make_timer()
        t.configure(300, "Pasta")
        t.start()
        payload = hass.bus.async_fire.call_args[0][1]
        assert payload["duration_s"] == 300

    def test_double_start_is_noop(self):
        t, hass = _make_timer()
        t.configure(300, "x")
        t.start()
        hass.bus.async_fire.reset_mock()
        t.start()
        hass.bus.async_fire.assert_not_called()

    def test_start_after_finish_resets_remaining(self):
        t, _ = _make_timer()
        t.configure(300, "x")
        t._d.state = TIMER_STATE_FINISHED
        t._d.remaining_s = 0
        t.start()
        assert t._d.remaining_s == 300


class TestTimerPause:
    def test_pause_changes_state_to_paused(self):
        t, _ = _make_timer()
        t.configure(300, "x")
        t.start()
        t.pause()
        assert t.state == TIMER_STATE_PAUSED

    def test_pause_fires_timer_paused_event(self):
        t, hass = _make_timer()
        t.configure(300, "x")
        t.start()
        hass.bus.async_fire.reset_mock()
        t.pause()
        fired_event = hass.bus.async_fire.call_args[0][0]
        assert fired_event == "timer_paused"

    def test_pause_on_idle_is_noop(self):
        t, hass = _make_timer()
        t.configure(300, "x")
        hass.bus.async_fire.reset_mock()
        t.pause()
        hass.bus.async_fire.assert_not_called()
        assert t.state == TIMER_STATE_IDLE

    def test_pause_preserves_remaining(self):
        t, _ = _make_timer()
        t.configure(300, "x")
        t._d.started_at = time.monotonic() - 10  # 10s elapsed
        t._d.state = TIMER_STATE_ACTIVE
        t._d.remaining_s = 300
        t.pause()
        assert t._d.remaining_s == pytest.approx(290, abs=2)


class TestTimerCancel:
    def test_cancel_resets_to_idle(self):
        t, _ = _make_timer()
        t.configure(300, "x")
        t.start()
        t.cancel()
        assert t.state == TIMER_STATE_IDLE

    def test_cancel_resets_remaining_to_duration(self):
        t, _ = _make_timer()
        t.configure(300, "x")
        t.start()
        t._d.remaining_s = 100
        t.cancel()
        assert t._d.remaining_s == 300

    def test_cancel_fires_cancelled_event(self):
        t, hass = _make_timer()
        t.configure(300, "x")
        t.start()
        hass.bus.async_fire.reset_mock()
        t.cancel()
        fired_event = hass.bus.async_fire.call_args[0][0]
        assert fired_event == "timer_cancelled"


# ── extra_state_attributes ────────────────────────────────────────────────────

class TestTimerAttributes:
    def test_attrs_include_timer_id(self):
        t, _ = _make_timer("pasta")
        t.configure(300, "Pasta")
        assert t.extra_state_attributes["timer_id"] == "pasta"

    def test_attrs_include_duration(self):
        t, _ = _make_timer()
        t.configure(300, "x")
        assert t.extra_state_attributes["duration_s"] == 300

    def test_attrs_include_formatted_duration(self):
        t, _ = _make_timer()
        t.configure(90, "x")
        assert t.extra_state_attributes["duration"] == "1:30"

    def test_attrs_remaining_clamped_to_zero(self):
        t, _ = _make_timer()
        t.configure(10, "x")
        t._d.state = TIMER_STATE_FINISHED
        t._d.remaining_s = 0
        assert t.extra_state_attributes["remaining_s"] == 0

    def test_attrs_ends_at_ts_only_when_active(self):
        t, _ = _make_timer()
        t.configure(300, "x")
        assert "ends_at_ts" not in t.extra_state_attributes
        t.start()
        assert "ends_at_ts" in t.extra_state_attributes


# ── Restore from HA state ─────────────────────────────────────────────────────

class TestTimerRestore:
    @pytest.mark.asyncio
    async def test_restore_active_resumes_if_time_remaining(self):
        t, _ = _make_timer()
        old_state = MagicMock()
        old_state.state = TIMER_STATE_ACTIVE
        ends_at = time.time() + 120
        old_state.attributes = {
            "duration_s": 300,
            "label": "Pasta",
            "ends_at_ts": ends_at,
        }
        t.async_get_last_state = AsyncMock(return_value=old_state)
        await t.async_added_to_hass()
        assert t.state == TIMER_STATE_ACTIVE
        assert t._d.remaining_s == pytest.approx(120, abs=2)

    @pytest.mark.asyncio
    async def test_restore_active_expired_becomes_finished(self):
        t, _ = _make_timer()
        old_state = MagicMock()
        old_state.state = TIMER_STATE_ACTIVE
        old_state.attributes = {
            "duration_s": 300,
            "label": "x",
            "ends_at_ts": time.time() - 10,   # already expired
        }
        t.async_get_last_state = AsyncMock(return_value=old_state)
        await t.async_added_to_hass()
        assert t.state == TIMER_STATE_FINISHED

    @pytest.mark.asyncio
    async def test_restore_paused_preserves_remaining(self):
        t, _ = _make_timer()
        old_state = MagicMock()
        old_state.state = TIMER_STATE_PAUSED
        old_state.attributes = {"duration_s": 300, "label": "x", "remaining_s": 150}
        t.async_get_last_state = AsyncMock(return_value=old_state)
        await t.async_added_to_hass()
        assert t.state == TIMER_STATE_PAUSED
        assert t._d.remaining_s == 150

    @pytest.mark.asyncio
    async def test_restore_no_state_leaves_idle(self):
        t, _ = _make_timer()
        t.async_get_last_state = AsyncMock(return_value=None)
        await t.async_added_to_hass()
        assert t.state == TIMER_STATE_IDLE


# ── Finish → Ozma alert push ──────────────────────────────────────────────────

class TestTimerFinishAlert:
    @pytest.mark.asyncio
    async def test_finish_pushes_ozma_alert(self):
        coordinator = MagicMock()
        coordinator.async_api_post = AsyncMock()
        t, _ = _make_timer(coordinator=coordinator)
        t.configure(1, "Pasta")
        await t._finish()
        coordinator.async_api_post.assert_awaited_once()
        call_kwargs = coordinator.async_api_post.call_args[1]
        assert call_kwargs["json"]["kind"] == "timer"
        assert call_kwargs["json"]["title"] == "Pasta"

    @pytest.mark.asyncio
    async def test_finish_push_failure_does_not_raise(self):
        coordinator = MagicMock()
        coordinator.async_api_post = AsyncMock(side_effect=Exception("connection refused"))
        t, _ = _make_timer(coordinator=coordinator)
        t.configure(1, "Pasta")
        # Should not raise
        await t._finish()
        assert t.state == TIMER_STATE_FINISHED

    @pytest.mark.asyncio
    async def test_finish_fires_timer_finished_event(self):
        t, hass = _make_timer()
        t.configure(300, "Pasta")
        await t._finish()
        fired = [c[0][0] for c in hass.bus.async_fire.call_args_list]
        assert "timer_finished" in fired

    @pytest.mark.asyncio
    async def test_finish_sets_remaining_to_zero(self):
        t, _ = _make_timer()
        t.configure(300, "x")
        await t._finish()
        assert t._d.remaining_s == 0
