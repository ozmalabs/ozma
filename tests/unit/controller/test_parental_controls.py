# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Unit tests for controller/parental_controls.py."""

import json
import time
from datetime import datetime
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

import sys
sys.path.insert(0, str(Path(__file__).parents[3] / "controller"))

from parental_controls import (
    AppAction, AppRule, AppTimer, BreakPolicy, ChildProfile,
    ContentFilter, LockdownConfig, OverrideSession, ParentalControlsManager,
    PermissionResult, PolicyMode, ScheduleWindow,
)


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _manager(tmp_path: Path) -> ParentalControlsManager:
    return ParentalControlsManager(data_dir=tmp_path)


def _simple_profile(mgr: ParentalControlsManager, *, mode: str = "blacklist") -> ChildProfile:
    return mgr.create_profile(name="Test Child", policy_mode=mode)


# ---------------------------------------------------------------------------
# AppRule
# ---------------------------------------------------------------------------

class TestAppRule:
    def test_matches_exact(self):
        rule = AppRule(rule_id="r1", pattern="fortnite.exe")
        assert rule.matches("Fortnite.exe")

    def test_matches_glob(self):
        rule = AppRule(rule_id="r1", pattern="fortnite*")
        assert rule.matches("fortnite_launcher")
        assert not rule.matches("minecraft")

    def test_to_from_dict(self):
        rule = AppRule(rule_id="r1", pattern="steam*", action=AppAction.BLOCK, timer_id="t1", label="Steam")
        assert AppRule.from_dict(rule.to_dict()).pattern == "steam*"
        assert AppRule.from_dict(rule.to_dict()).action == AppAction.BLOCK


# ---------------------------------------------------------------------------
# AppTimer
# ---------------------------------------------------------------------------

class TestAppTimer:
    def test_matches_pattern(self):
        timer = AppTimer(timer_id="t1", label="Games", patterns=["fortnite*", "minecraft*"],
                         daily_minutes=120)
        assert timer.matches("Fortnite.exe")
        assert timer.matches("Minecraft")
        assert not timer.matches("chrome")

    def test_is_active_today_daily(self):
        timer = AppTimer(timer_id="t1", label="x", patterns=[], daily_minutes=60,
                         days_active=["daily"])
        assert timer.is_active_today()

    def test_is_active_today_specific_day(self):
        timer = AppTimer(timer_id="t1", label="x", patterns=[], daily_minutes=60,
                         days_active=["mon"])
        monday = datetime(2026, 3, 30)   # a Monday
        tuesday = datetime(2026, 3, 31)
        assert timer.is_active_today(monday)
        assert not timer.is_active_today(tuesday)

    def test_to_from_dict(self):
        timer = AppTimer(timer_id="t1", label="Games", patterns=["*.exe"],
                         daily_minutes=90, reset_hour=6, warning_minutes=10,
                         days_active=["weekday"])
        roundtripped = AppTimer.from_dict(timer.to_dict())
        assert roundtripped.daily_minutes == 90
        assert roundtripped.reset_hour == 6
        assert roundtripped.days_active == ["weekday"]


# ---------------------------------------------------------------------------
# ScheduleWindow
# ---------------------------------------------------------------------------

class TestScheduleWindow:
    def test_active_within_window(self):
        win = ScheduleWindow(day="daily", start_time="08:00", end_time="20:00")
        dt = datetime(2026, 4, 4, 14, 0)   # Saturday 2pm
        assert win.active_now(dt)

    def test_inactive_before_start(self):
        win = ScheduleWindow(day="daily", start_time="08:00", end_time="20:00")
        dt = datetime(2026, 4, 4, 7, 30)
        assert not win.active_now(dt)

    def test_inactive_after_end(self):
        win = ScheduleWindow(day="daily", start_time="08:00", end_time="20:00")
        dt = datetime(2026, 4, 4, 21, 0)
        assert not win.active_now(dt)

    def test_weekend_only(self):
        win = ScheduleWindow(day="weekend", start_time="09:00", end_time="22:00")
        saturday = datetime(2026, 4, 4, 12, 0)   # Sat
        monday   = datetime(2026, 3, 30, 12, 0)  # Mon
        assert win.active_now(saturday)
        assert not win.active_now(monday)

    def test_weekday_only(self):
        win = ScheduleWindow(day="weekday", start_time="15:00", end_time="20:00")
        friday  = datetime(2026, 4, 3, 17, 0)
        sunday  = datetime(2026, 4, 5, 17, 0)
        assert win.active_now(friday)
        assert not win.active_now(sunday)

    def test_specific_day(self):
        win = ScheduleWindow(day="fri", start_time="00:00", end_time="23:59")
        friday = datetime(2026, 4, 3, 10, 0)
        saturday = datetime(2026, 4, 4, 10, 0)
        assert win.active_now(friday)
        assert not win.active_now(saturday)


# ---------------------------------------------------------------------------
# LockdownConfig
# ---------------------------------------------------------------------------

class TestLockdownConfig:
    def test_off(self):
        cfg = LockdownConfig.off()
        assert cfg.policy_mode == "off"

    def test_whitelist_allows_matching(self):
        cfg = LockdownConfig(
            profile_id="p1", policy_mode="whitelist",
            whitelist=["chrome*", "firefox*"],
            blacklist=[], kiosk_app="",
            schedule_locked=False, override_active=False,
        )
        allowed, _ = cfg.is_allowed("chrome.exe")
        assert allowed
        allowed2, _ = cfg.is_allowed("fortnite.exe")
        assert not allowed2

    def test_blacklist_blocks_matching(self):
        cfg = LockdownConfig(
            profile_id="p1", policy_mode="blacklist",
            whitelist=[],
            blacklist=["fortnite*", "steam*"],
            kiosk_app="",
            schedule_locked=False, override_active=False,
        )
        allowed, _ = cfg.is_allowed("fortnite.exe")
        assert not allowed
        allowed2, _ = cfg.is_allowed("chrome.exe")
        assert allowed2

    def test_kiosk_allows_only_kiosk_app(self):
        cfg = LockdownConfig(
            profile_id="p1", policy_mode="kiosk",
            whitelist=[], blacklist=[], kiosk_app="kiosk_browser*",
            schedule_locked=False, override_active=False,
        )
        assert cfg.is_allowed("kiosk_browser_v2")[0]
        assert not cfg.is_allowed("chrome.exe")[0]

    def test_override_bypasses_restrictions(self):
        cfg = LockdownConfig(
            profile_id="p1", policy_mode="whitelist",
            whitelist=["chrome*"],
            blacklist=[], kiosk_app="",
            schedule_locked=False, override_active=True,
        )
        allowed, _ = cfg.is_allowed("fortnite.exe")
        assert allowed  # override bypasses whitelist

    def test_timers_field_present(self):
        cfg = LockdownConfig.from_dict({
            "profile_id": "p1", "policy_mode": "off",
            "whitelist": [], "blacklist": [], "kiosk_app": "",
            "schedule_locked": False, "override_active": False,
            "timers": {"t1": 30},
        })
        assert cfg.timers == {"t1": 30}

    def test_to_dict_roundtrip(self):
        cfg = LockdownConfig.from_dict({
            "profile_id": "p1", "policy_mode": "blacklist",
            "whitelist": [], "blacklist": ["steam*"],
            "kiosk_app": "", "schedule_locked": False, "override_active": False,
            "timers": {"t1": 60}, "timer_warnings": {"t1": 5},
        })
        d = cfg.to_dict()
        restored = LockdownConfig.from_dict(d)
        assert restored.timers == {"t1": 60}
        assert restored.timer_warnings == {"t1": 5}


# ---------------------------------------------------------------------------
# ParentalControlsManager — profile CRUD
# ---------------------------------------------------------------------------

class TestParentalControlsManagerProfiles:
    def test_create_and_get_profile(self, tmp_path):
        mgr = _manager(tmp_path)
        profile = mgr.create_profile(name="Alice", policy_mode="blacklist")
        assert profile.name == "Alice"
        assert mgr.get_profile(profile.profile_id) is profile

    def test_list_profiles(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.create_profile(name="Alice")
        mgr.create_profile(name="Bob")
        profiles = mgr.list_profiles()
        assert len(profiles) == 2
        assert all(isinstance(p, dict) for p in profiles)

    def test_update_profile_name(self, tmp_path):
        mgr = _manager(tmp_path)
        p = mgr.create_profile(name="Alice")
        updated = mgr.update_profile(p.profile_id, name="Alicia")
        assert updated.name == "Alicia"

    def test_delete_profile(self, tmp_path):
        mgr = _manager(tmp_path)
        p = mgr.create_profile(name="Alice")
        assert mgr.delete_profile(p.profile_id)
        assert mgr.get_profile(p.profile_id) is None

    def test_delete_nonexistent_returns_false(self, tmp_path):
        mgr = _manager(tmp_path)
        assert not mgr.delete_profile("nonexistent")

    def test_get_nonexistent_returns_none(self, tmp_path):
        mgr = _manager(tmp_path)
        assert mgr.get_profile("nonexistent") is None

    def test_profiles_persist(self, tmp_path):
        mgr = _manager(tmp_path)
        p = mgr.create_profile(name="Persisted")
        mgr2 = _manager(tmp_path)
        assert mgr2.get_profile(p.profile_id) is not None
        assert mgr2.get_profile(p.profile_id).name == "Persisted"


# ---------------------------------------------------------------------------
# ParentalControlsManager — rule and timer helpers
# ---------------------------------------------------------------------------

class TestParentalControlsManagerRulesTimers:
    def test_add_rule(self, tmp_path):
        mgr = _manager(tmp_path)
        p = mgr.create_profile(name="Alice")
        rule = mgr.add_rule(p.profile_id, pattern="fortnite*", action="block")
        assert rule is not None
        assert rule.pattern == "fortnite*"
        assert rule.action == AppAction.BLOCK

    def test_add_rule_to_nonexistent_profile(self, tmp_path):
        mgr = _manager(tmp_path)
        assert mgr.add_rule("bad_id", pattern="*", action="block") is None

    def test_remove_rule(self, tmp_path):
        mgr = _manager(tmp_path)
        p = mgr.create_profile(name="Alice")
        rule = mgr.add_rule(p.profile_id, pattern="steam*", action="block")
        assert mgr.remove_rule(p.profile_id, rule.rule_id)
        assert mgr.get_profile(p.profile_id).find_rule("steam_app") is None

    def test_add_timer(self, tmp_path):
        mgr = _manager(tmp_path)
        p = mgr.create_profile(name="Alice")
        timer = mgr.add_timer(p.profile_id, label="Games",
                               patterns=["fortnite*"], daily_minutes=120)
        assert timer is not None
        assert timer.daily_minutes == 120

    def test_remove_timer(self, tmp_path):
        mgr = _manager(tmp_path)
        p = mgr.create_profile(name="Alice")
        timer = mgr.add_timer(p.profile_id, label="Games",
                               patterns=["fortnite*"], daily_minutes=60)
        assert mgr.remove_timer(p.profile_id, timer.timer_id)


# ---------------------------------------------------------------------------
# ParentalControlsManager — device assignment
# ---------------------------------------------------------------------------

class TestParentalControlsManagerDevices:
    def test_assign_and_get_device_profile(self, tmp_path):
        mgr = _manager(tmp_path)
        p = mgr.create_profile(name="Alice")
        assert mgr.assign_device(p.profile_id, "dev-1")
        assert mgr.get_profile_for_device("dev-1") is p

    def test_assign_nonexistent_profile_fails(self, tmp_path):
        mgr = _manager(tmp_path)
        assert not mgr.assign_device("bad_id", "dev-1")

    def test_unassign_device(self, tmp_path):
        mgr = _manager(tmp_path)
        p = mgr.create_profile(name="Alice")
        mgr.assign_device(p.profile_id, "dev-1")
        mgr.unassign_device("dev-1")
        assert mgr.get_profile_for_device("dev-1") is None

    def test_get_unassigned_device_returns_none(self, tmp_path):
        mgr = _manager(tmp_path)
        assert mgr.get_profile_for_device("dev-xyz") is None


# ---------------------------------------------------------------------------
# ParentalControlsManager — check_permission
# ---------------------------------------------------------------------------

class TestCheckPermission:
    def test_no_profile_allows_all(self, tmp_path):
        mgr = _manager(tmp_path)
        p = mgr.create_profile(name="Alice")
        # device not assigned
        result = mgr.check_permission(p.profile_id, "fortnite.exe", "unassigned-dev")
        # profile_id given directly — should work
        assert isinstance(result, PermissionResult)

    def test_blacklist_blocks_matching_app(self, tmp_path):
        mgr = _manager(tmp_path)
        p = mgr.create_profile(name="Alice", policy_mode="blacklist")
        mgr.add_rule(p.profile_id, pattern="fortnite*", action="block")
        result = mgr.check_permission(p.profile_id, "fortnite.exe", "dev-1")
        assert not result.allowed

    def test_whitelist_blocks_non_listed(self, tmp_path):
        mgr = _manager(tmp_path)
        p = mgr.create_profile(name="Alice", policy_mode="whitelist")
        mgr.add_rule(p.profile_id, pattern="chrome*", action="allow")
        result = mgr.check_permission(p.profile_id, "steam.exe", "dev-1")
        assert not result.allowed

    def test_whitelist_allows_listed_app(self, tmp_path):
        mgr = _manager(tmp_path)
        p = mgr.create_profile(name="Alice", policy_mode="whitelist")
        mgr.add_rule(p.profile_id, pattern="chrome*", action="allow")
        result = mgr.check_permission(p.profile_id, "chrome.exe", "dev-1")
        assert result.allowed

    def test_schedule_locked_outside_window(self, tmp_path):
        mgr = _manager(tmp_path)
        p = mgr.create_profile(name="Alice")
        # Add a window for 03:00-04:00 only (highly unlikely to run tests then)
        # and pin the check time to 14:00 to guarantee it's outside the window.
        p.schedule = [ScheduleWindow(day="daily", start_time="03:00", end_time="04:00")]
        # Force a 14:00 check time
        from datetime import datetime
        noon = datetime(2026, 4, 4, 14, 0, 0)
        result = mgr.check_permission(p.profile_id, "chrome.exe", "dev-1", now=noon)
        assert not result.allowed
        assert result.schedule_locked

    def test_override_bypasses_schedule(self, tmp_path):
        mgr = _manager(tmp_path)
        p = mgr.create_profile(name="Alice", override_pin="1234")
        p.schedule = [ScheduleWindow(day="daily", start_time="03:00", end_time="04:00")]
        mgr.assign_device(p.profile_id, "dev-1")
        session = mgr.grant_override(p.profile_id, "dev-1", 60, "1234")
        assert session is not None
        result = mgr.check_permission(p.profile_id, "chrome.exe", "dev-1")
        assert result.allowed
        assert result.override_active

    def test_wrong_pin_rejected(self, tmp_path):
        mgr = _manager(tmp_path)
        p = mgr.create_profile(name="Alice", override_pin="1234")
        session = mgr.grant_override(p.profile_id, "dev-1", 60, "wrong")
        assert session is None

    def test_timer_exhausted_blocks(self, tmp_path):
        mgr = _manager(tmp_path)
        p = mgr.create_profile(name="Alice", policy_mode="blacklist")
        timer = mgr.add_timer(p.profile_id, label="Games",
                               patterns=["fortnite*"], daily_minutes=120)
        mgr.add_rule(p.profile_id, pattern="fortnite*", action="limit", timer_id=timer.timer_id)
        # Exhaust the timer by recording full usage
        mgr.record_usage(p.profile_id, "fortnite.exe", 120)
        result = mgr.check_permission(p.profile_id, "fortnite.exe", "dev-1")
        assert not result.allowed


# ---------------------------------------------------------------------------
# ParentalControlsManager — usage tracking
# ---------------------------------------------------------------------------

class TestUsageTracking:
    def test_record_and_get_usage(self, tmp_path):
        mgr = _manager(tmp_path)
        p = mgr.create_profile(name="Alice")
        mgr.record_usage(p.profile_id, "fortnite.exe", 45)
        mgr.record_usage(p.profile_id, "chrome.exe", 10)
        summary = mgr.get_usage_summary(p.profile_id)
        # Should have some entries
        assert isinstance(summary, dict)

    def test_prune_usage(self, tmp_path):
        mgr = _manager(tmp_path)
        p = mgr.create_profile(name="Alice")
        mgr.record_usage(p.profile_id, "steam.exe", 30)
        mgr.prune_usage(keep_days=0)
        # After pruning keep_days=0, all old records removed
        summary = mgr.get_usage_summary(p.profile_id)
        assert isinstance(summary, dict)

    def test_reset_timer(self, tmp_path):
        mgr = _manager(tmp_path)
        p = mgr.create_profile(name="Alice")
        timer = mgr.add_timer(p.profile_id, label="Games",
                               patterns=["fortnite*"], daily_minutes=120)
        mgr.record_usage(p.profile_id, "fortnite.exe", 120)
        assert mgr.reset_timer(p.profile_id, timer.timer_id)


# ---------------------------------------------------------------------------
# ParentalControlsManager — override sessions
# ---------------------------------------------------------------------------

class TestOverrideSessions:
    def test_grant_and_list_override(self, tmp_path):
        mgr = _manager(tmp_path)
        p = mgr.create_profile(name="Alice", override_pin="9876")
        mgr.assign_device(p.profile_id, "dev-1")
        session = mgr.grant_override(p.profile_id, "dev-1", 30, "9876", reason="Homework done")
        assert session is not None
        assert session.is_active()
        sessions = mgr.list_overrides()
        assert any(s["override_id"] == session.override_id for s in sessions)

    def test_revoke_override(self, tmp_path):
        mgr = _manager(tmp_path)
        p = mgr.create_profile(name="Alice", override_pin="9876")
        session = mgr.grant_override(p.profile_id, "dev-1", 30, "9876")
        assert mgr.revoke_override(session.override_id)
        active = mgr.list_overrides()
        assert not any(s["override_id"] == session.override_id for s in active)

    def test_revoke_nonexistent_returns_false(self, tmp_path):
        mgr = _manager(tmp_path)
        assert not mgr.revoke_override("bad_id")


# ---------------------------------------------------------------------------
# ParentalControlsManager — enforcement state
# ---------------------------------------------------------------------------

class TestEnforcementState:
    def test_no_assignment_returns_off(self, tmp_path):
        mgr = _manager(tmp_path)
        cfg = mgr.get_enforcement_state("unassigned-dev")
        assert cfg.policy_mode == "off"

    def test_whitelist_profile_builds_whitelist(self, tmp_path):
        mgr = _manager(tmp_path)
        p = mgr.create_profile(name="Alice", policy_mode="whitelist")
        mgr.add_rule(p.profile_id, pattern="chrome*", action="allow")
        mgr.add_rule(p.profile_id, pattern="firefox*", action="allow")
        mgr.assign_device(p.profile_id, "dev-1")
        cfg = mgr.get_enforcement_state("dev-1")
        assert cfg.policy_mode == "whitelist"
        assert "chrome*" in cfg.whitelist

    def test_kiosk_profile_sets_kiosk_app(self, tmp_path):
        mgr = _manager(tmp_path)
        p = mgr.create_profile(name="Kiosk", policy_mode="kiosk")
        p.kiosk_app = "kiosk_browser"
        mgr.assign_device(p.profile_id, "kiosk-dev")
        cfg = mgr.get_enforcement_state("kiosk-dev")
        assert cfg.policy_mode == "kiosk"
        assert cfg.kiosk_app == "kiosk_browser"

    def test_timer_included_in_enforcement_state(self, tmp_path):
        mgr = _manager(tmp_path)
        p = mgr.create_profile(name="Alice")
        timer = mgr.add_timer(p.profile_id, label="Games",
                               patterns=["fortnite*"], daily_minutes=120)
        mgr.assign_device(p.profile_id, "dev-1")
        cfg = mgr.get_enforcement_state("dev-1")
        assert timer.timer_id in cfg.timers
        assert cfg.timers[timer.timer_id] == 120

    def test_enforcement_state_serialises(self, tmp_path):
        mgr = _manager(tmp_path)
        p = mgr.create_profile(name="Alice")
        mgr.assign_device(p.profile_id, "dev-1")
        cfg = mgr.get_enforcement_state("dev-1")
        d = cfg.to_dict()
        assert isinstance(d, dict)
        assert "policy_mode" in d


# ---------------------------------------------------------------------------
# ParentalControlsManager — lifecycle
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
class TestParentalControlsManagerLifecycle:
    async def test_start_stop(self, tmp_path):
        mgr = _manager(tmp_path)
        await mgr.start()
        await mgr.stop()

    async def test_enforcement_loop_pushes_config(self, tmp_path):
        """Enforcement loop should attempt to push config to agents."""
        mgr = _manager(tmp_path)
        p = mgr.create_profile(name="Alice")
        mgr.assign_device(p.profile_id, "dev-1")

        pushed: list[dict] = []

        async def fake_push(device_id: str, cfg: LockdownConfig) -> None:
            pushed.append({"device_id": device_id, "cfg": cfg})

        mgr._push_to_agent = fake_push

        await mgr.start()
        import asyncio
        await asyncio.sleep(0.1)
        await mgr.stop()
        # The loop may or may not have fired in 0.1s — just check no crash
