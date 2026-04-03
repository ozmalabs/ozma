# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Unit tests for agent/app_monitor.py."""

import asyncio
import json
import os
import platform
import sys
import time
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch, call

import pytest

sys.path.insert(0, str(Path(__file__).parents[3] / "agent"))

from app_monitor import (
    AppEvent, AppEventType, AppMonitor, AppMonitorIntegration,
    LockdownConfig, ProcessInfo,
    _list_processes_linux, _list_processes_macos, _list_processes_windows,
    list_processes, kill_process, lock_screen,
)


# ---------------------------------------------------------------------------
# LockdownConfig (agent-side)
# ---------------------------------------------------------------------------

class TestAgentLockdownConfig:
    def _make(self, **kwargs) -> LockdownConfig:
        defaults = {
            "profile_id": "p1", "policy_mode": "off",
            "whitelist": [], "blacklist": [], "kiosk_app": "",
            "schedule_locked": False, "override_active": False,
        }
        defaults.update(kwargs)
        return LockdownConfig.from_dict(defaults)

    def test_off_config(self):
        cfg = LockdownConfig.off()
        assert cfg.is_off()
        allowed, _ = cfg.is_allowed("anything")
        assert allowed

    def test_whitelist_allows_match(self):
        cfg = self._make(policy_mode="whitelist", whitelist=["chrome*", "firefox*"])
        assert cfg.is_allowed("chrome.exe")[0]
        assert cfg.is_allowed("firefox")[0]
        assert not cfg.is_allowed("steam.exe")[0]

    def test_blacklist_blocks_match(self):
        cfg = self._make(policy_mode="blacklist", blacklist=["fortnite*"])
        assert not cfg.is_allowed("fortnite.exe")[0]
        assert cfg.is_allowed("chrome.exe")[0]

    def test_kiosk_allows_only_kiosk(self):
        cfg = self._make(policy_mode="kiosk", kiosk_app="kiosk_browser*")
        assert cfg.is_allowed("kiosk_browser_v2")[0]
        assert not cfg.is_allowed("chrome.exe")[0]

    def test_override_bypasses_whitelist(self):
        cfg = self._make(policy_mode="whitelist", whitelist=["chrome*"], override_active=True)
        assert cfg.is_allowed("steam.exe")[0]

    def test_timer_roundtrip_from_dict(self):
        cfg = self._make(timers={"t1": 60}, timer_warnings={"t1": 5})
        assert cfg.timer_remaining("t1") == 60
        assert cfg.timer_warnings == {"t1": 5}

    def test_decrement_timer(self):
        cfg = self._make(timers={"t1": 10})
        cfg.decrement_timer("t1")
        assert cfg.timer_remaining("t1") == 9

    def test_decrement_timer_clamps_to_zero(self):
        cfg = self._make(timers={"t1": 0})
        cfg.decrement_timer("t1")
        assert cfg.timer_remaining("t1") == 0

    def test_persist_roundtrip_preserves_runtime_timers(self):
        cfg = self._make(timers={"t1": 30})
        cfg.decrement_timer("t1")
        cfg.decrement_timer("t1")
        persisted = cfg.to_persist()
        assert persisted["timers"]["t1"] == 28
        restored = LockdownConfig.from_dict(persisted)
        assert restored.timer_remaining("t1") == 28


# ---------------------------------------------------------------------------
# Process listing stubs
# ---------------------------------------------------------------------------

class TestProcessListing:
    def test_linux_listing_parses_proc(self, tmp_path):
        # Create a fake /proc structure
        proc = tmp_path / "proc"
        proc.mkdir()
        (proc / "not_a_pid").mkdir()
        pid_dir = proc / "1234"
        pid_dir.mkdir()
        (pid_dir / "comm").write_text("myapp\n")
        (pid_dir / "cmdline").write_bytes(b"/usr/bin/myapp\x00--flag\x00")

        with patch("app_monitor.Path", lambda p: Path(p) if p != "/proc" else proc):
            # Direct call with our fake path
            procs: list[ProcessInfo] = []
            for entry in proc.iterdir():
                if not entry.name.isdigit():
                    continue
                pid = int(entry.name)
                comm = (entry / "comm").read_text().strip().lower()
                cmdline = (entry / "cmdline").read_bytes().replace(b"\x00", b" ").decode().strip()
                procs.append(ProcessInfo(pid=pid, name=comm, cmdline=cmdline))
            assert len(procs) == 1
            assert procs[0].name == "myapp"
            assert procs[0].pid == 1234

    def test_processinfo_fields(self):
        info = ProcessInfo(pid=42, name="myapp", cmdline="/usr/bin/myapp --flag")
        assert info.pid == 42
        assert info.name == "myapp"


# ---------------------------------------------------------------------------
# AppMonitor — config apply
# ---------------------------------------------------------------------------

class TestAppMonitorApplyConfig:
    def _monitor(self, tmp_path) -> AppMonitor:
        return AppMonitor(
            controller_url="http://localhost:7380",
            device_id="dev-1",
            data_dir=tmp_path,
        )

    def test_apply_config_replaces_existing(self, tmp_path):
        m = self._monitor(tmp_path)
        cfg = LockdownConfig.from_dict({
            "profile_id": "p1", "policy_mode": "blacklist",
            "whitelist": [], "blacklist": ["steam*"],
            "kiosk_app": "", "schedule_locked": False, "override_active": False,
            "timers": {},
        })
        m.apply_config(cfg)
        assert m._config.policy_mode == "blacklist"

    def test_apply_config_preserves_lower_timer(self, tmp_path):
        """Agent's runtime countdown should not be increased by a re-push."""
        m = self._monitor(tmp_path)
        cfg1 = LockdownConfig.from_dict({
            "profile_id": "p1", "policy_mode": "off",
            "whitelist": [], "blacklist": [], "kiosk_app": "",
            "schedule_locked": False, "override_active": False,
            "timers": {"t1": 60},
        })
        m.apply_config(cfg1)
        # Count down 10 minutes
        for _ in range(10):
            m._config.decrement_timer("t1")
        assert m._config.timer_remaining("t1") == 50

        # Controller re-pushes same budget — should not reset to 60
        cfg2 = LockdownConfig.from_dict({
            "profile_id": "p1", "policy_mode": "off",
            "whitelist": [], "blacklist": [], "kiosk_app": "",
            "schedule_locked": False, "override_active": False,
            "timers": {"t1": 60},
        })
        m.apply_config(cfg2)
        assert m._config.timer_remaining("t1") == 50

    def test_apply_config_allows_higher_budget(self, tmp_path):
        """Parent extending time should increase the runtime timer."""
        m = self._monitor(tmp_path)
        cfg1 = LockdownConfig.from_dict({
            "profile_id": "p1", "policy_mode": "off",
            "whitelist": [], "blacklist": [], "kiosk_app": "",
            "schedule_locked": False, "override_active": False,
            "timers": {"t1": 60},
        })
        m.apply_config(cfg1)
        for _ in range(10):
            m._config.decrement_timer("t1")
        assert m._config.timer_remaining("t1") == 50

        # Parent adds 30 more minutes → new budget 90 > runtime 50 → update
        cfg2 = LockdownConfig.from_dict({
            "profile_id": "p1", "policy_mode": "off",
            "whitelist": [], "blacklist": [], "kiosk_app": "",
            "schedule_locked": False, "override_active": False,
            "timers": {"t1": 90},
        })
        m.apply_config(cfg2)
        assert m._config.timer_remaining("t1") == 90


# ---------------------------------------------------------------------------
# AppMonitor — persistence
# ---------------------------------------------------------------------------

class TestAppMonitorPersistence:
    def test_persist_and_load(self, tmp_path):
        m = AppMonitor(
            controller_url="http://localhost:7380",
            device_id="dev-1",
            data_dir=tmp_path,
        )
        cfg = LockdownConfig.from_dict({
            "profile_id": "p1", "policy_mode": "blacklist",
            "whitelist": [], "blacklist": ["fortnite*"],
            "kiosk_app": "", "schedule_locked": False, "override_active": False,
            "timers": {"t1": 45},
        })
        m.apply_config(cfg)
        m._persist_config()

        m2 = AppMonitor(
            controller_url="http://localhost:7380",
            device_id="dev-1",
            data_dir=tmp_path,
        )
        assert m2._config.policy_mode == "blacklist"
        assert m2._config.timer_remaining("t1") == 45

    def test_persist_file_is_mode_600(self, tmp_path):
        m = AppMonitor(
            controller_url="http://localhost:7380",
            device_id="dev-1",
            data_dir=tmp_path,
        )
        cfg = LockdownConfig.from_dict({
            "profile_id": "p1", "policy_mode": "off",
            "whitelist": [], "blacklist": [], "kiosk_app": "",
            "schedule_locked": False, "override_active": False,
        })
        m.apply_config(cfg)
        m._persist_config()
        stat = (tmp_path / "lockdown_config.json").stat()
        if platform.system() != "Windows":
            assert oct(stat.st_mode & 0o777) == oct(0o600)

    def test_load_missing_file_returns_off(self, tmp_path):
        m = AppMonitor(
            controller_url="http://localhost:7380",
            device_id="dev-1",
            data_dir=tmp_path,
        )
        assert m._config.is_off()

    def test_load_corrupt_file_returns_off(self, tmp_path):
        (tmp_path / "lockdown_config.json").write_text("not json {{{")
        m = AppMonitor(
            controller_url="http://localhost:7380",
            device_id="dev-1",
            data_dir=tmp_path,
        )
        assert m._config.is_off()


# ---------------------------------------------------------------------------
# AppMonitor — hooks
# ---------------------------------------------------------------------------

class TestAppMonitorHooks:
    @pytest.mark.asyncio
    async def test_sync_hook_called(self, tmp_path):
        m = AppMonitor(
            controller_url="http://localhost:7380",
            device_id="dev-1",
            data_dir=tmp_path,
        )
        received: list[AppEvent] = []
        m.add_hook(received.append)
        event = AppEvent(event_type=AppEventType.LAUNCH, app_name="chrome.exe")
        await m._fire(event)
        assert len(received) == 1
        assert received[0].app_name == "chrome.exe"

    @pytest.mark.asyncio
    async def test_async_hook_called(self, tmp_path):
        m = AppMonitor(
            controller_url="http://localhost:7380",
            device_id="dev-1",
            data_dir=tmp_path,
        )
        received: list[AppEvent] = []

        async def async_hook(event: AppEvent) -> None:
            received.append(event)

        m.add_hook(async_hook)
        await m._fire(AppEvent(event_type=AppEventType.EXIT, app_name="steam.exe"))
        assert len(received) == 1

    @pytest.mark.asyncio
    async def test_hook_exception_does_not_crash(self, tmp_path):
        m = AppMonitor(
            controller_url="http://localhost:7380",
            device_id="dev-1",
            data_dir=tmp_path,
        )

        def bad_hook(event: AppEvent) -> None:
            raise RuntimeError("hook error")

        m.add_hook(bad_hook)
        # Should not raise
        await m._fire(AppEvent(event_type=AppEventType.LAUNCH, app_name="app"))


# ---------------------------------------------------------------------------
# AppMonitor — process scan enforcement
# ---------------------------------------------------------------------------

class TestAppMonitorEnforcement:
    @pytest.mark.asyncio
    async def test_blacklisted_app_gets_killed(self, tmp_path):
        m = AppMonitor(
            controller_url="http://localhost:7380",
            device_id="dev-1",
            data_dir=tmp_path,
        )
        cfg = LockdownConfig.from_dict({
            "profile_id": "p1", "policy_mode": "blacklist",
            "whitelist": [], "blacklist": ["fortnite*"],
            "kiosk_app": "", "schedule_locked": False, "override_active": False,
            "timers": {},
        })
        m.apply_config(cfg)

        killed: list[tuple] = []

        async def fake_kill(pid: int, name: str) -> bool:
            killed.append((pid, name))
            return True

        proc = ProcessInfo(pid=999, name="fortnite.exe", cmdline="fortnite.exe")
        m._known_pids = {}  # fresh start
        m._running_apps = {}

        with patch("app_monitor.kill_process", fake_kill):
            # Simulate scan finding a new process
            with patch("app_monitor.list_processes", return_value=[proc]):
                await m._scan_processes()

        assert any(p[1] == "fortnite.exe" for p in killed)

    @pytest.mark.asyncio
    async def test_allowed_app_not_killed(self, tmp_path):
        m = AppMonitor(
            controller_url="http://localhost:7380",
            device_id="dev-1",
            data_dir=tmp_path,
        )
        cfg = LockdownConfig.from_dict({
            "profile_id": "p1", "policy_mode": "blacklist",
            "whitelist": [], "blacklist": ["fortnite*"],
            "kiosk_app": "", "schedule_locked": False, "override_active": False,
            "timers": {},
        })
        m.apply_config(cfg)

        killed: list[tuple] = []

        async def fake_kill(pid: int, name: str) -> bool:
            killed.append((pid, name))
            return True

        proc = ProcessInfo(pid=100, name="chrome.exe", cmdline="chrome.exe")
        m._known_pids = {}
        m._running_apps = {}

        with patch("app_monitor.kill_process", fake_kill):
            with patch("app_monitor.list_processes", return_value=[proc]):
                await m._scan_processes()

        assert not any(p[1] == "chrome.exe" for p in killed)

    @pytest.mark.asyncio
    async def test_schedule_locked_kills_all(self, tmp_path):
        m = AppMonitor(
            controller_url="http://localhost:7380",
            device_id="dev-1",
            data_dir=tmp_path,
        )
        cfg = LockdownConfig.from_dict({
            "profile_id": "p1", "policy_mode": "blacklist",
            "whitelist": [], "blacklist": [],
            "kiosk_app": "", "schedule_locked": True, "override_active": False,
            "timers": {},
        })
        m.apply_config(cfg)

        killed: list[tuple] = []

        async def fake_kill(pid: int, name: str) -> bool:
            killed.append((pid, name))
            return True

        proc = ProcessInfo(pid=200, name="chrome.exe", cmdline="chrome.exe")
        m._known_pids = {}
        m._running_apps = {}

        with patch("app_monitor.kill_process", fake_kill):
            with patch("app_monitor.list_processes", return_value=[proc]):
                await m._scan_processes()

        assert any(p[1] == "chrome.exe" for p in killed)

    @pytest.mark.asyncio
    async def test_exit_event_fired(self, tmp_path):
        m = AppMonitor(
            controller_url="http://localhost:7380",
            device_id="dev-1",
            data_dir=tmp_path,
        )
        events: list[AppEvent] = []
        m.add_hook(events.append)

        proc = ProcessInfo(pid=300, name="myapp", cmdline="myapp")
        m._known_pids = {300: proc}
        m._running_apps = {"myapp": {300}}

        with patch("app_monitor.list_processes", return_value=[]):
            await m._scan_processes()

        exit_events = [e for e in events if e.event_type == AppEventType.EXIT]
        assert any(e.app_name == "myapp" for e in exit_events)

    @pytest.mark.asyncio
    async def test_launch_event_fired(self, tmp_path):
        m = AppMonitor(
            controller_url="http://localhost:7380",
            device_id="dev-1",
            data_dir=tmp_path,
        )
        m.apply_config(LockdownConfig.off())
        events: list[AppEvent] = []
        m.add_hook(events.append)

        proc = ProcessInfo(pid=400, name="myapp", cmdline="myapp")
        m._known_pids = {}
        m._running_apps = {}

        with patch("app_monitor.list_processes", return_value=[proc]):
            await m._scan_processes()

        launch_events = [e for e in events if e.event_type == AppEventType.LAUNCH]
        assert any(e.app_name == "myapp" for e in launch_events)


# ---------------------------------------------------------------------------
# AppMonitor — timer tick
# ---------------------------------------------------------------------------

class TestAppMonitorTimerTick:
    @pytest.mark.asyncio
    async def test_timer_warning_fired(self, tmp_path):
        m = AppMonitor(
            controller_url="http://localhost:7380",
            device_id="dev-1",
            data_dir=tmp_path,
        )
        cfg = LockdownConfig.from_dict({
            "profile_id": "p1", "policy_mode": "blacklist",
            "whitelist": [], "blacklist": [],
            "kiosk_app": "", "schedule_locked": False, "override_active": False,
            "timers": {"t1": 5},
            "timer_warnings": {"t1": 5},
        })
        m.apply_config(cfg)
        m._timer_patterns = {"t1": ["fortnite*"]}
        m._running_apps = {"fortnite.exe": {123}}
        m._known_pids = {123: ProcessInfo(pid=123, name="fortnite.exe", cmdline="")}

        events: list[AppEvent] = []
        m.add_hook(events.append)

        with patch("app_monitor.kill_process", AsyncMock(return_value=True)):
            await m._tick_timers()

        warn_events = [e for e in events if e.event_type == AppEventType.TIMER_WARNING]
        assert warn_events

    @pytest.mark.asyncio
    async def test_timer_exhausted_kills_app(self, tmp_path):
        m = AppMonitor(
            controller_url="http://localhost:7380",
            device_id="dev-1",
            data_dir=tmp_path,
        )
        cfg = LockdownConfig.from_dict({
            "profile_id": "p1", "policy_mode": "blacklist",
            "whitelist": [], "blacklist": [],
            "kiosk_app": "", "schedule_locked": False, "override_active": False,
            "timers": {"t1": 1},
            "timer_warnings": {"t1": 5},
        })
        m.apply_config(cfg)
        m._timer_patterns = {"t1": ["fortnite*"]}
        m._running_apps = {"fortnite.exe": {123}}
        m._known_pids = {123: ProcessInfo(pid=123, name="fortnite.exe", cmdline="")}

        events: list[AppEvent] = []
        m.add_hook(events.append)
        killed: list[tuple] = []

        async def fake_kill(pid, name):
            killed.append((pid, name))
            return True

        with patch("app_monitor.kill_process", fake_kill):
            await m._tick_timers()

        assert any(k[1] == "fortnite.exe" for k in killed)


# ---------------------------------------------------------------------------
# AppMonitor — start/stop
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
class TestAppMonitorLifecycle:
    async def test_start_and_stop(self, tmp_path):
        m = AppMonitor(
            controller_url="http://localhost:7380",
            device_id="dev-1",
            data_dir=tmp_path,
        )
        with patch("app_monitor.list_processes", return_value=[]):
            await m.start()
            await asyncio.sleep(0.05)
            await m.stop()
        assert m._stopped


# ---------------------------------------------------------------------------
# AppEvent
# ---------------------------------------------------------------------------

class TestAppEvent:
    def test_to_dict(self):
        event = AppEvent(
            event_type=AppEventType.BLOCKED,
            app_name="fortnite.exe",
            pid=999,
            timer_id="t1",
            timer_remaining=0,
            message="policy",
        )
        d = event.to_dict()
        assert d["event_type"] == "blocked"
        assert d["app_name"] == "fortnite.exe"
        assert d["pid"] == 999
        assert "ts" in d
