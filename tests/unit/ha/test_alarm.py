# SPDX-License-Identifier: MIT
"""Unit tests for ecosystem/ha-ozma alarm.py — expand_days, next_trigger, AlarmClock lifecycle."""
import asyncio
import sys
from datetime import datetime, time as dt_time, timezone
from pathlib import Path
from types import ModuleType
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

pytestmark = pytest.mark.unit

# ── Stub homeassistant imports ────────────────────────────────────────────────

def _stub_ha_modules():
    # homeassistant root — must be a package
    ha_mod = ModuleType("homeassistant")
    ha_mod.__path__ = []
    sys.modules["homeassistant"] = ha_mod

    # homeassistant.core
    core_mod = ModuleType("homeassistant.core")
    core_mod.HomeAssistant = object
    core_mod.callback = lambda fn: fn
    sys.modules["homeassistant.core"] = core_mod

    # homeassistant.config_entries
    entries_mod = ModuleType("homeassistant.config_entries")
    entries_mod.ConfigEntry = object
    sys.modules["homeassistant.config_entries"] = entries_mod

    # homeassistant.util + homeassistant.util.dt
    util_mod = ModuleType("homeassistant.util")
    util_mod.__path__ = []
    sys.modules["homeassistant.util"] = util_mod

    dt_mod = ModuleType("homeassistant.util.dt")
    _NOW = datetime(2026, 4, 6, 8, 0, 0, tzinfo=timezone.utc)
    dt_mod.now = MagicMock(return_value=_NOW)
    sys.modules["homeassistant.util.dt"] = dt_mod

    # homeassistant.helpers + submodules
    helpers_mod = ModuleType("homeassistant.helpers")
    helpers_mod.__path__ = []
    sys.modules["homeassistant.helpers"] = helpers_mod

    platform_mod = ModuleType("homeassistant.helpers.entity_platform")
    platform_mod.AddEntitiesCallback = object
    sys.modules["homeassistant.helpers.entity_platform"] = platform_mod

    event_mod = ModuleType("homeassistant.helpers.event")
    event_mod.async_track_time_change = MagicMock(return_value=MagicMock())
    sys.modules["homeassistant.helpers.event"] = event_mod

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

import importlib.util as _ilu

_HA_OZMA = Path(__file__).parent.parent.parent.parent / "ecosystem" / "ha-ozma" / "custom_components" / "ozma"

# Load const.py
_const_spec = _ilu.spec_from_file_location("ozma_const", _HA_OZMA / "const.py")
_const_mod = _ilu.module_from_spec(_const_spec)
sys.modules.setdefault("ozma", _const_mod)
sys.modules["ozma.const"] = _const_mod
_const_spec.loader.exec_module(_const_mod)

# Load alarm.py
_alarm_spec = _ilu.spec_from_file_location("ozma.alarm", _HA_OZMA / "alarm.py",
                                            submodule_search_locations=[])
_alarm_mod = _ilu.module_from_spec(_alarm_spec)
_alarm_mod.__package__ = "ozma"
sys.modules["ozma.alarm"] = _alarm_mod
_alarm_spec.loader.exec_module(_alarm_mod)

expand_days  = _alarm_mod.expand_days
next_trigger = _alarm_mod.next_trigger
AlarmClock   = _alarm_mod.AlarmClock

ALARM_STATE_SCHEDULED = _const_mod.ALARM_STATE_SCHEDULED
ALARM_STATE_RINGING   = _const_mod.ALARM_STATE_RINGING
ALARM_STATE_SNOOZED   = _const_mod.ALARM_STATE_SNOOZED
ALARM_STATE_DISABLED  = _const_mod.ALARM_STATE_DISABLED
DAYS_ALL      = _const_mod.DAYS_ALL
DAYS_WEEKDAYS = _const_mod.DAYS_WEEKDAYS
DAYS_WEEKENDS = _const_mod.DAYS_WEEKENDS

import homeassistant.util.dt as dt_util  # noqa: E402


# ── Helpers ───────────────────────────────────────────────────────────────────

def _set_now(dt: datetime) -> None:
    """Override the mocked dt_util.now() return value."""
    dt_util.now.return_value = dt


def _make_alarm(alarm_id="morning"):
    hass = MagicMock()
    hass.bus = MagicMock()
    hass.bus.async_fire = MagicMock()
    hass.async_create_task = MagicMock(side_effect=lambda coro, name=None: asyncio.ensure_future(coro))
    alarm = AlarmClock(hass=hass, alarm_id=alarm_id, coordinator=None)
    return alarm, hass


# ── expand_days ───────────────────────────────────────────────────────────────

class TestExpandDays:
    def test_daily_returns_all_seven(self):
        assert expand_days("daily") == set(range(7))

    def test_all_returns_all_seven(self):
        # DAYS_ALL == "daily"
        assert expand_days(DAYS_ALL) == set(range(7))

    def test_weekdays_returns_mon_fri(self):
        assert expand_days(DAYS_WEEKDAYS) == {0, 1, 2, 3, 4}

    def test_weekends_returns_sat_sun(self):
        assert expand_days(DAYS_WEEKENDS) == {5, 6}

    def test_comma_separated_day_names(self):
        assert expand_days("mon,wed,fri") == {0, 2, 4}

    def test_single_day_name(self):
        assert expand_days("sat") == {5}

    def test_numeric_strings(self):
        assert expand_days("0,6") == {0, 6}

    def test_list_of_day_names(self):
        assert expand_days(["mon", "tue"]) == {0, 1}

    def test_invalid_names_ignored(self):
        # "xyz" is not a valid day — only valid days are returned
        result = expand_days("mon,xyz")
        assert result == {0}

    def test_empty_string_falls_back_to_all(self):
        result = expand_days("")
        assert result == set(range(7))

    def test_out_of_range_numeric_ignored(self):
        result = expand_days("0,7,8")
        assert result == {0}

    def test_whitespace_trimmed(self):
        assert expand_days(" mon , fri ") == {0, 4}


# ── next_trigger ──────────────────────────────────────────────────────────────

class TestNextTrigger:
    def setup_method(self):
        # Reset to Monday 2026-04-06 08:00:00 before each test
        _set_now(datetime(2026, 4, 6, 8, 0, 0, tzinfo=timezone.utc))

    def test_alarm_today_in_future_returns_today(self):
        # Now is Mon 08:00; alarm at Mon 09:00 → today
        result = next_trigger(dt_time(9, 0), {0})  # Monday = 0
        assert result.weekday() == 0
        assert result.hour == 9

    def test_alarm_today_already_past_returns_next_week(self):
        # Now is Mon 08:00; alarm at Mon 07:00 → already passed → next Mon
        result = next_trigger(dt_time(7, 0), {0})
        # Should be 7 days from now (next Monday)
        assert result.weekday() == 0
        assert result > dt_util.now()

    def test_alarm_on_specific_future_day(self):
        # Now is Mon 08:00; alarm at Wed 07:00 → this Wednesday
        result = next_trigger(dt_time(7, 0), {2})  # Wednesday = 2
        assert result.weekday() == 2

    def test_weekdays_alarm_saturday_now_returns_monday(self):
        # Saturday; next weekday alarm → Monday
        _set_now(datetime(2026, 4, 11, 8, 0, 0, tzinfo=timezone.utc))  # Saturday
        result = next_trigger(dt_time(9, 0), {0, 1, 2, 3, 4})
        assert result.weekday() in {0, 1, 2, 3, 4}

    def test_empty_days_defaults_to_every_day(self):
        # Empty days set → treat as every day
        result = next_trigger(dt_time(9, 0), set())
        assert result > dt_util.now()

    def test_result_is_always_in_future(self):
        for hour in range(0, 24, 3):
            result = next_trigger(dt_time(hour, 0), set(range(7)))
            assert result > dt_util.now(), f"alarm at {hour:02d}:00 not in future"

    def test_single_day_within_14_day_window(self):
        # Any day should be found within 14 days
        for day in range(7):
            result = next_trigger(dt_time(9, 0), {day})
            diff = result - dt_util.now()
            assert 0 < diff.total_seconds() <= 14 * 86400


# ── AlarmClock state machine ──────────────────────────────────────────────────

class TestAlarmClockConfigure:
    def test_configure_enabled_sets_scheduled(self):
        alarm, _ = _make_alarm()
        alarm.configure(dt_time(7, 0), "weekdays", "Wake up", enabled=True)
        assert alarm.state == ALARM_STATE_SCHEDULED

    def test_configure_disabled_sets_disabled(self):
        alarm, _ = _make_alarm()
        alarm.configure(dt_time(7, 0), "weekdays", "Wake up", enabled=False)
        assert alarm.state == ALARM_STATE_DISABLED

    def test_configure_stores_time(self):
        alarm, _ = _make_alarm()
        alarm.configure(dt_time(7, 30), "daily", "x")
        assert alarm._time == dt_time(7, 30)

    def test_configure_stores_label(self):
        alarm, _ = _make_alarm()
        alarm.configure(dt_time(7, 0), "daily", "Morning")
        assert alarm._label == "Morning"

    def test_configure_stores_snooze_minutes(self):
        alarm, _ = _make_alarm()
        alarm.configure(dt_time(7, 0), "daily", "x", snooze_minutes=5)
        assert alarm._snooze_minutes == 5


class TestAlarmClockEnable:
    def test_enable_from_disabled_sets_scheduled(self):
        alarm, _ = _make_alarm()
        alarm._state = ALARM_STATE_DISABLED
        alarm.enable()
        assert alarm.state == ALARM_STATE_SCHEDULED

    def test_enable_fires_alarm_enabled_event(self):
        alarm, hass = _make_alarm()
        alarm._state = ALARM_STATE_DISABLED
        alarm.enable()
        fired = hass.bus.async_fire.call_args[0][0]
        assert fired == "alarm_enabled"

    def test_enable_when_already_scheduled_is_noop(self):
        alarm, hass = _make_alarm()
        alarm._state = ALARM_STATE_SCHEDULED
        alarm._unlisten = MagicMock()
        alarm.enable()
        hass.bus.async_fire.assert_not_called()


class TestAlarmClockDisable:
    def test_disable_sets_disabled(self):
        alarm, _ = _make_alarm()
        alarm.configure(dt_time(7, 0), "daily", "x", enabled=True)
        alarm.disable()
        assert alarm.state == ALARM_STATE_DISABLED

    def test_disable_fires_alarm_disabled_event(self):
        alarm, hass = _make_alarm()
        alarm.configure(dt_time(7, 0), "daily", "x", enabled=True)
        hass.bus.async_fire.reset_mock()
        alarm.disable()
        fired = hass.bus.async_fire.call_args[0][0]
        assert fired == "alarm_disabled"

    def test_disable_cancels_snooze_task(self):
        alarm, _ = _make_alarm()
        mock_task = MagicMock()
        alarm._snooze_task = mock_task
        alarm._state = ALARM_STATE_SNOOZED
        alarm.disable()
        mock_task.cancel.assert_called_once()
        assert alarm._snooze_task is None


class TestAlarmClockSnooze:
    def test_snooze_sets_snoozed_state(self):
        alarm, _ = _make_alarm()
        alarm._state = ALARM_STATE_RINGING
        alarm.snooze(minutes=5)
        assert alarm.state == ALARM_STATE_SNOOZED

    def test_snooze_fires_alarm_snoozed_event(self):
        alarm, hass = _make_alarm()
        alarm._state = ALARM_STATE_RINGING
        alarm.snooze(minutes=9)
        fired = hass.bus.async_fire.call_args[0][0]
        assert fired == "alarm_snoozed"

    def test_snooze_event_includes_minutes(self):
        alarm, hass = _make_alarm()
        alarm._state = ALARM_STATE_RINGING
        alarm.snooze(minutes=9)
        payload = hass.bus.async_fire.call_args[0][1]
        assert payload["snooze_minutes"] == 9

    def test_snooze_uses_default_minutes_when_none(self):
        alarm, hass = _make_alarm()
        alarm._snooze_minutes = 7
        alarm._state = ALARM_STATE_RINGING
        alarm.snooze(minutes=None)
        payload = hass.bus.async_fire.call_args[0][1]
        assert payload["snooze_minutes"] == 7


class TestAlarmClockDismiss:
    def test_dismiss_from_ringing_returns_to_scheduled(self):
        alarm, _ = _make_alarm()
        alarm._state = ALARM_STATE_RINGING
        alarm.dismiss()
        assert alarm.state == ALARM_STATE_SCHEDULED

    def test_dismiss_fires_alarm_dismissed_event(self):
        alarm, hass = _make_alarm()
        alarm._state = ALARM_STATE_RINGING
        hass.bus.async_fire.reset_mock()
        alarm.dismiss()
        fired = hass.bus.async_fire.call_args[0][0]
        assert fired == "alarm_dismissed"

    def test_dismiss_cancels_snooze_task(self):
        alarm, _ = _make_alarm()
        mock_task = MagicMock()
        alarm._snooze_task = mock_task
        alarm._state = ALARM_STATE_SNOOZED
        alarm.dismiss()
        mock_task.cancel.assert_called_once()
        assert alarm._snooze_task is None

    def test_dismiss_while_disabled_does_not_reschedule(self):
        alarm, _ = _make_alarm()
        alarm._state = ALARM_STATE_DISABLED
        alarm.dismiss()
        assert alarm.state == ALARM_STATE_DISABLED


# ── _on_time callback ─────────────────────────────────────────────────────────

class TestOnTime:
    def test_on_time_triggers_when_scheduled_and_correct_day(self):
        alarm, hass = _make_alarm()
        alarm._state = ALARM_STATE_SCHEDULED
        alarm._days = set(range(7))   # every day
        # Monday = weekday 0
        fake_now = datetime(2026, 4, 6, 7, 0, 0, tzinfo=timezone.utc)
        alarm._on_time(fake_now)
        assert alarm.state == ALARM_STATE_RINGING

    def test_on_time_noop_when_day_not_in_schedule(self):
        alarm, _ = _make_alarm()
        alarm._state = ALARM_STATE_SCHEDULED
        alarm._days = {5, 6}   # weekends only
        # Monday = 0 — should not trigger
        fake_now = datetime(2026, 4, 6, 7, 0, 0, tzinfo=timezone.utc)
        alarm._on_time(fake_now)
        assert alarm.state == ALARM_STATE_SCHEDULED

    def test_on_time_noop_when_not_scheduled(self):
        alarm, _ = _make_alarm()
        alarm._state = ALARM_STATE_DISABLED
        alarm._days = set(range(7))
        alarm._on_time(datetime(2026, 4, 6, 7, 0, tzinfo=timezone.utc))
        assert alarm.state == ALARM_STATE_DISABLED


# ── extra_state_attributes ────────────────────────────────────────────────────

class TestAlarmAttributes:
    def setup_method(self):
        _set_now(datetime(2026, 4, 6, 8, 0, 0, tzinfo=timezone.utc))

    def test_attrs_include_alarm_id(self):
        alarm, _ = _make_alarm("morning")
        alarm.configure(dt_time(7, 0), "weekdays", "Wake up")
        assert alarm.extra_state_attributes["alarm_id"] == "morning"

    def test_attrs_include_time(self):
        alarm, _ = _make_alarm()
        alarm.configure(dt_time(7, 30), "weekdays", "x")
        assert alarm.extra_state_attributes["time"] == "07:30"

    def test_attrs_include_days(self):
        alarm, _ = _make_alarm()
        alarm.configure(dt_time(7, 0), "weekdays", "x")
        assert alarm.extra_state_attributes["days"] == "weekdays"

    def test_attrs_no_next_trigger_when_disabled(self):
        alarm, _ = _make_alarm()
        alarm.configure(dt_time(7, 0), "daily", "x", enabled=False)
        assert "next_trigger" not in alarm.extra_state_attributes

    def test_attrs_has_next_trigger_when_scheduled(self):
        alarm, _ = _make_alarm()
        alarm.configure(dt_time(9, 0), "daily", "x", enabled=True)
        attrs = alarm.extra_state_attributes
        assert "next_trigger" in attrs
        assert "next_trigger_in" in attrs

    def test_next_trigger_in_shows_hours(self):
        # Now 08:00; alarm 09:00 same day → "1h 00m"
        alarm, _ = _make_alarm()
        alarm.configure(dt_time(9, 0), "daily", "x", enabled=True)
        attrs = alarm.extra_state_attributes
        assert "h" in attrs["next_trigger_in"]


# ── Restore from HA state ─────────────────────────────────────────────────────

class TestAlarmRestore:
    @pytest.mark.asyncio
    async def test_restore_scheduled_resumes_listening(self):
        alarm, _ = _make_alarm()
        old_state = MagicMock()
        old_state.state = ALARM_STATE_SCHEDULED
        old_state.attributes = {
            "time": "07:00",
            "days": "weekdays",
            "label": "Wake up",
            "snooze_minutes": "9",
        }
        alarm.async_get_last_state = AsyncMock(return_value=old_state)
        alarm._listen = MagicMock()
        await alarm.async_added_to_hass()
        assert alarm.state == ALARM_STATE_SCHEDULED
        alarm._listen.assert_called_once()

    @pytest.mark.asyncio
    async def test_restore_disabled_stays_disabled(self):
        alarm, _ = _make_alarm()
        old_state = MagicMock()
        old_state.state = ALARM_STATE_DISABLED
        old_state.attributes = {
            "time": "07:00",
            "days": "daily",
            "label": "",
            "snooze_minutes": "9",
        }
        alarm.async_get_last_state = AsyncMock(return_value=old_state)
        alarm._listen = MagicMock()
        await alarm.async_added_to_hass()
        assert alarm.state == ALARM_STATE_DISABLED
        alarm._listen.assert_not_called()

    @pytest.mark.asyncio
    async def test_restore_ringing_becomes_scheduled(self):
        """Ringing at restart → resume as scheduled (user can re-trigger manually)."""
        alarm, _ = _make_alarm()
        old_state = MagicMock()
        old_state.state = ALARM_STATE_RINGING
        old_state.attributes = {
            "time": "07:00",
            "days": "daily",
            "label": "x",
            "snooze_minutes": "9",
        }
        alarm.async_get_last_state = AsyncMock(return_value=old_state)
        alarm._listen = MagicMock()
        await alarm.async_added_to_hass()
        assert alarm.state == ALARM_STATE_SCHEDULED

    @pytest.mark.asyncio
    async def test_restore_bad_time_uses_default(self):
        alarm, _ = _make_alarm()
        old_state = MagicMock()
        old_state.state = ALARM_STATE_DISABLED
        old_state.attributes = {
            "time": "not-a-time",
            "days": "daily",
            "label": "",
            "snooze_minutes": "9",
        }
        alarm.async_get_last_state = AsyncMock(return_value=old_state)
        await alarm.async_added_to_hass()
        assert alarm._time == dt_time(7, 0)   # default unchanged

    @pytest.mark.asyncio
    async def test_restore_no_state_leaves_defaults(self):
        alarm, _ = _make_alarm()
        alarm.async_get_last_state = AsyncMock(return_value=None)
        await alarm.async_added_to_hass()
        assert alarm.state == ALARM_STATE_DISABLED


# ── Ozma alert push ───────────────────────────────────────────────────────────

class TestAlarmAlertPush:
    @pytest.mark.asyncio
    async def test_trigger_pushes_ozma_alert(self):
        coordinator = MagicMock()
        coordinator.async_api_post = AsyncMock()
        alarm, _ = _make_alarm()
        alarm._coordinator = coordinator
        alarm._label = "Morning"
        alarm._time = dt_time(7, 0)
        alarm._id = "morning"
        alarm._snooze_minutes = 9
        await alarm._trigger()
        coordinator.async_api_post.assert_awaited_once()
        call_kwargs = coordinator.async_api_post.call_args[1]
        assert call_kwargs["json"]["kind"] == "alarm"
        assert call_kwargs["json"]["title"] == "Morning"

    @pytest.mark.asyncio
    async def test_trigger_push_failure_does_not_raise(self):
        coordinator = MagicMock()
        coordinator.async_api_post = AsyncMock(side_effect=Exception("network error"))
        alarm, hass = _make_alarm()
        alarm._coordinator = coordinator
        alarm._state = ALARM_STATE_RINGING
        # Should not raise
        await alarm._trigger()
        # But the HA event should still fire
        fired = [c[0][0] for c in hass.bus.async_fire.call_args_list]
        assert "alarm_triggered" in fired
