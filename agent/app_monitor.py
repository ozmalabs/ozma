# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
App Monitor — agent-side parental controls enforcement.

Responsibilities
----------------
* Cross-platform process watcher (Linux /proc, macOS via osascript/ps,
  Windows via psutil/WMI).
* AppEvent hook system — callbacks fired on launch, exit, blocked,
  timer_warning, timer_expired, break_due, schedule_locked.
* LockdownConfig enforcement — kill blocked processes, keep kiosk app
  alive, lock the screen when schedule window closes.
* Local timer countdown — counts down from the controller's last push;
  re-syncs on reconnect.  Agent never increases its own budget.
* Break enforcement — tracks continuous session time, locks screen when
  break is due.
* Usage reporting — POSTs per-pattern minute increments to the controller
  every 60 seconds.

Platform notes
--------------
Linux
    /proc/<pid>/comm and /proc/<pid>/cmdline for process names.
    kill(pid, SIGTERM) then SIGKILL after 2 s for enforcement.
    Optionally locks screen via loginctl lock-session or xdg-screensaver.

macOS
    ``ps -eo pid,comm`` polling.  ``kill`` for enforcement.
    Locks screen via ``open -a ScreenSaverEngine`` or ``pmset sleepnow``.

Windows
    ``psutil.process_iter()`` when psutil is available, else
    ``tasklist /NH /FO CSV`` via subprocess.  ``taskkill /PID /F`` for
    enforcement.  Locks via ``ctypes.windll.user32.LockWorkStation()``.
"""

from __future__ import annotations

import asyncio
import fnmatch
import json
import logging
import os
import platform
import signal
import subprocess
import sys
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable, Awaitable

log = logging.getLogger("ozma.app_monitor")

_SYSTEM = platform.system()   # "Linux" | "Darwin" | "Windows"

# ---------------------------------------------------------------------------
# Event types
# ---------------------------------------------------------------------------

class AppEventType:
    LAUNCH          = "launch"
    EXIT            = "exit"
    BLOCKED         = "blocked"       # process was killed by enforcement
    TIMER_WARNING   = "timer_warning" # N minutes left on timer
    TIMER_EXPIRED   = "timer_expired" # budget exhausted, app killed
    BREAK_DUE       = "break_due"     # continuous session limit reached
    SCHEDULE_LOCKED = "schedule_locked"   # schedule window closed
    KIOSK_RESTART   = "kiosk_restart"     # kiosk app restarted


@dataclass
class AppEvent:
    event_type: str
    app_name:   str
    pid:        int    = 0
    timer_id:   str    = ""
    timer_remaining: int = -1
    message:    str    = ""
    ts:         float  = field(default_factory=time.time)

    def to_dict(self) -> dict[str, Any]:
        return {
            "event_type":      self.event_type,
            "app_name":        self.app_name,
            "pid":             self.pid,
            "timer_id":        self.timer_id,
            "timer_remaining": self.timer_remaining,
            "message":         self.message,
            "ts":              self.ts,
        }


# ---------------------------------------------------------------------------
# LockdownConfig (mirror of controller model — parsed from pushed JSON)
# ---------------------------------------------------------------------------

@dataclass
class LockdownConfig:
    profile_id:      str
    policy_mode:     str            # "off" | "whitelist" | "blacklist" | "kiosk"
    whitelist:       list[str]
    blacklist:       list[str]
    kiosk_app:       str
    schedule_locked: bool
    override_active: bool
    override_until:  float = 0.0
    timers:          dict[str, int] = field(default_factory=dict)
    timer_warnings:  dict[str, int] = field(default_factory=dict)
    break_policy:    dict[str, Any] = field(default_factory=dict)
    content_filter:  str = "off"
    generated_at:    float = 0.0

    # Runtime state (not serialised from controller)
    _timer_runtime:  dict[str, int]  = field(default_factory=dict, repr=False)
    # timer_id → minutes remaining in *this agent session* (counts down)
    _timer_warned:   set[str]        = field(default_factory=set, repr=False)
    # timer_ids for which we have already fired the warning event

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> "LockdownConfig":
        cfg = cls(
            profile_id      = d.get("profile_id", ""),
            policy_mode     = d.get("policy_mode", "off"),
            whitelist       = d.get("whitelist", []),
            blacklist       = d.get("blacklist", []),
            kiosk_app       = d.get("kiosk_app", ""),
            schedule_locked = bool(d.get("schedule_locked", False)),
            override_active = bool(d.get("override_active", False)),
            override_until  = float(d.get("override_until", 0)),
            timers          = {k: int(v) for k, v in d.get("timers", {}).items()},
            timer_warnings  = {k: int(v) for k, v in d.get("timer_warnings", {}).items()},
            break_policy    = d.get("break_policy", {}),
            content_filter  = d.get("content_filter", "off"),
            generated_at    = float(d.get("generated_at", 0)),
        )
        cfg._timer_runtime = dict(cfg.timers)
        return cfg

    @classmethod
    def off(cls) -> "LockdownConfig":
        return cls(
            profile_id="", policy_mode="off",
            whitelist=[], blacklist=[], kiosk_app="",
            schedule_locked=False, override_active=False,
        )

    def is_off(self) -> bool:
        return self.policy_mode == "off"

    def is_allowed(self, app_name: str) -> tuple[bool, str]:
        """
        Return (allowed, timer_id).

        Does not check schedule_locked (caller handles that).
        """
        if self.is_off() or self.override_active:
            return True, ""

        name = app_name.lower()

        if self.policy_mode == "kiosk":
            allowed = fnmatch.fnmatch(name, self.kiosk_app.lower())
            return allowed, ""

        if self.policy_mode == "whitelist":
            for pat in self.whitelist:
                if fnmatch.fnmatch(name, pat.lower()):
                    return True, ""
            return False, ""

        if self.policy_mode == "blacklist":
            for pat in self.blacklist:
                if fnmatch.fnmatch(name, pat.lower()):
                    return False, ""
            return True, ""

        return True, ""

    def find_timer_for(self, app_name: str) -> str:
        """Return the timer_id whose patterns match app_name, or ""."""
        # The controller encodes timer pattern membership in the timer_warnings
        # keys — we need the whitelist/blacklist to carry timer_id info.
        # Since we receive a flat whitelist of patterns from the controller,
        # we store timer info separately in timer_patterns (populated by AppMonitor).
        return ""

    def timer_remaining(self, timer_id: str) -> int:
        """Minutes remaining for a timer (from runtime countdown)."""
        return self._timer_runtime.get(timer_id, self.timers.get(timer_id, -1))

    def decrement_timer(self, timer_id: str) -> int:
        """Decrement timer by 1 minute. Returns new remaining value."""
        current = self._timer_runtime.get(timer_id, self.timers.get(timer_id, 0))
        new_val = max(0, current - 1)
        self._timer_runtime[timer_id] = new_val
        return new_val

    def to_persist(self) -> dict[str, Any]:
        """Snapshot for local persistence (survives agent restart)."""
        return {
            "profile_id":      self.profile_id,
            "policy_mode":     self.policy_mode,
            "whitelist":       self.whitelist,
            "blacklist":       self.blacklist,
            "kiosk_app":       self.kiosk_app,
            "schedule_locked": self.schedule_locked,
            "override_active": self.override_active,
            "override_until":  self.override_until,
            "timers":          self._timer_runtime,  # save runtime state
            "timer_warnings":  self.timer_warnings,
            "break_policy":    self.break_policy,
            "content_filter":  self.content_filter,
            "generated_at":    self.generated_at,
        }


# ---------------------------------------------------------------------------
# Process snapshot
# ---------------------------------------------------------------------------

@dataclass
class ProcessInfo:
    pid:      int
    name:     str     # basename of executable, lowercased
    cmdline:  str     # full command line (for matching)


# ---------------------------------------------------------------------------
# Platform-specific process listing
# ---------------------------------------------------------------------------

def _list_processes_linux() -> list[ProcessInfo]:
    procs: list[ProcessInfo] = []
    try:
        proc_path = Path("/proc")
        for entry in proc_path.iterdir():
            if not entry.name.isdigit():
                continue
            pid = int(entry.name)
            try:
                comm_file = entry / "comm"
                cmdline_file = entry / "cmdline"
                name = comm_file.read_text().strip().lower() if comm_file.exists() else ""
                cmdline = cmdline_file.read_bytes().replace(b"\x00", b" ").decode(errors="replace").strip() if cmdline_file.exists() else ""
                if name:
                    procs.append(ProcessInfo(pid=pid, name=name, cmdline=cmdline))
            except (PermissionError, FileNotFoundError):
                pass
    except Exception:
        pass
    return procs


def _list_processes_macos() -> list[ProcessInfo]:
    procs: list[ProcessInfo] = []
    try:
        result = subprocess.run(
            ["ps", "-eo", "pid,comm"],
            capture_output=True, text=True, timeout=5,
        )
        for line in result.stdout.splitlines()[1:]:
            parts = line.strip().split(None, 1)
            if len(parts) == 2:
                try:
                    pid = int(parts[0])
                    name = Path(parts[1]).name.lower()
                    procs.append(ProcessInfo(pid=pid, name=name, cmdline=parts[1]))
                except ValueError:
                    pass
    except Exception:
        pass
    return procs


def _list_processes_windows() -> list[ProcessInfo]:
    procs: list[ProcessInfo] = []
    try:
        import psutil  # type: ignore
        for p in psutil.process_iter(["pid", "name", "cmdline"]):
            try:
                info = p.info
                name = (info["name"] or "").lower()
                cmdline = " ".join(info.get("cmdline") or [])
                if name:
                    procs.append(ProcessInfo(pid=info["pid"], name=name, cmdline=cmdline))
            except Exception:
                pass
    except ImportError:
        # Fallback: tasklist
        try:
            result = subprocess.run(
                ["tasklist", "/NH", "/FO", "CSV"],
                capture_output=True, text=True, timeout=10,
            )
            for line in result.stdout.splitlines():
                line = line.strip().strip('"')
                parts = line.split('","')
                if len(parts) >= 2:
                    try:
                        name = parts[0].lower()
                        pid = int(parts[1])
                        procs.append(ProcessInfo(pid=pid, name=name, cmdline=""))
                    except ValueError:
                        pass
        except Exception:
            pass
    return procs


def list_processes() -> list[ProcessInfo]:
    if _SYSTEM == "Linux":
        return _list_processes_linux()
    elif _SYSTEM == "Darwin":
        return _list_processes_macos()
    elif _SYSTEM == "Windows":
        return _list_processes_windows()
    return []


# ---------------------------------------------------------------------------
# Platform-specific process termination
# ---------------------------------------------------------------------------

async def kill_process(pid: int, name: str) -> bool:
    """Terminate a process gracefully then forcefully."""
    loop = asyncio.get_event_loop()
    try:
        if _SYSTEM == "Windows":
            await loop.run_in_executor(
                None, lambda: subprocess.run(
                    ["taskkill", "/PID", str(pid), "/F"],
                    capture_output=True, timeout=5,
                )
            )
        else:
            os.kill(pid, signal.SIGTERM)
            await asyncio.sleep(2.0)
            # Check if still alive, then SIGKILL
            try:
                os.kill(pid, 0)   # probe — raises if dead
                os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        log.info("Killed process pid=%d name=%s", pid, name)
        return True
    except ProcessLookupError:
        return True   # already gone
    except PermissionError:
        log.warning("No permission to kill pid=%d name=%s", pid, name)
        return False
    except Exception as exc:
        log.warning("kill_process(%d) error: %s", pid, exc)
        return False


# ---------------------------------------------------------------------------
# Screen lock
# ---------------------------------------------------------------------------

async def lock_screen() -> None:
    """Lock the workstation screen."""
    loop = asyncio.get_event_loop()
    try:
        if _SYSTEM == "Linux":
            # Try loginctl first (systemd), then xdg-screensaver, then xlock
            for cmd in [
                ["loginctl", "lock-session"],
                ["xdg-screensaver", "lock"],
                ["gnome-screensaver-command", "--lock"],
                ["xlock", "-mode", "blank"],
            ]:
                try:
                    await asyncio.create_subprocess_exec(
                        *cmd,
                        stdout=asyncio.subprocess.DEVNULL,
                        stderr=asyncio.subprocess.DEVNULL,
                    )
                    return
                except FileNotFoundError:
                    continue
        elif _SYSTEM == "Darwin":
            await asyncio.create_subprocess_exec(
                "pmset", "displaysleepnow",
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.DEVNULL,
            )
        elif _SYSTEM == "Windows":
            await loop.run_in_executor(
                None,
                lambda: __import__("ctypes").windll.user32.LockWorkStation(),
            )
    except Exception as exc:
        log.warning("lock_screen failed: %s", exc)


# ---------------------------------------------------------------------------
# AppMonitor
# ---------------------------------------------------------------------------

HookFn = Callable[[AppEvent], Awaitable[None]] | Callable[[AppEvent], None]


class AppMonitor:
    """
    Cross-platform process watcher + LockdownConfig enforcer.

    Usage
    -----
    monitor = AppMonitor(
        controller_url="http://10.0.0.1:7380",
        device_id="node-abc",
        token="<jwt>",
        data_dir=Path("/var/lib/ozma"),
    )
    monitor.add_hook(my_async_callback)
    await monitor.start()
    # ...
    await monitor.stop()
    """

    _POLL_INTERVAL  = 2.0    # seconds between /proc scans
    _REPORT_INTERVAL = 60.0  # seconds between usage POSTs to controller
    _FETCH_INTERVAL  = 30.0  # seconds between lockdown config fetches
    _KIOSK_RESPAWN_DELAY = 3.0  # seconds before restarting dead kiosk app

    def __init__(
        self,
        controller_url: str,
        device_id: str,
        token: str = "",
        data_dir: Path | None = None,
    ) -> None:
        self._controller_url = controller_url.rstrip("/")
        self._device_id      = device_id
        self._token          = token
        self._data_dir       = data_dir or Path(os.path.expanduser("~/.ozma"))
        self._data_dir.mkdir(parents=True, exist_ok=True)

        self._lockdown_file = self._data_dir / "lockdown_config.json"

        self._config: LockdownConfig = self._load_persisted_config()
        self._hooks: list[HookFn] = []

        # Runtime tracking
        self._known_pids:   dict[int, ProcessInfo] = {}   # pid → info at launch
        self._running_apps: dict[str, set[int]]    = {}   # app_name → set of pids

        # Usage accumulator: app_name → minutes in current report window
        self._usage_acc: dict[str, int] = {}

        # Timer patterns: timer_id → list of glob patterns (populated from API)
        self._timer_patterns: dict[str, list[str]] = {}

        # Continuous session tracking (for break policy)
        self._session_start:  float | None = None   # when last non-break period began
        self._on_break:       bool  = False
        self._break_until:    float = 0.0

        self._tasks: list[asyncio.Task] = []
        self._stopped = False

    # ------------------------------------------------------------------
    # Public API
    # ------------------------------------------------------------------

    def add_hook(self, fn: HookFn) -> None:
        """Register a callback for app events."""
        self._hooks.append(fn)

    def remove_hook(self, fn: HookFn) -> None:
        self._hooks.discard(fn) if hasattr(self._hooks, "discard") else None
        if fn in self._hooks:
            self._hooks.remove(fn)

    def apply_config(self, config: LockdownConfig) -> None:
        """
        Apply a new LockdownConfig pushed from the controller.

        Merges runtime timer state so we don't reset the countdown
        when the controller re-sends the same budget.
        """
        # Preserve runtime timer countdown if the new config has the same
        # timer_id and hasn't increased the budget
        for tid, new_minutes in config.timers.items():
            old_runtime = self._config._timer_runtime.get(tid)
            old_budget  = self._config.timers.get(tid)
            if old_runtime is None:
                # First time seeing this timer — use new budget
                config._timer_runtime[tid] = new_minutes
            elif old_budget is not None and new_minutes > old_budget:
                # Parent explicitly increased the budget — apply the increase
                config._timer_runtime[tid] = new_minutes
            elif old_runtime <= new_minutes:
                # Normal re-push of same/decreased budget; keep runtime countdown
                config._timer_runtime[tid] = old_runtime
            else:
                # Budget was cut below current runtime — clamp to new budget
                config._timer_runtime[tid] = new_minutes

        # Preserve warned set for timers that still exist
        for tid in config.timers:
            if tid in self._config._timer_warned:
                config._timer_warned.add(tid)

        self._config = config
        self._persist_config()
        log.info("Applied lockdown config profile=%s mode=%s",
                 config.profile_id, config.policy_mode)

    async def start(self) -> None:
        self._stopped = False
        self._tasks = [
            asyncio.create_task(self._poll_loop(),    name="app_monitor.poll"),
            asyncio.create_task(self._timer_loop(),   name="app_monitor.timer"),
            asyncio.create_task(self._report_loop(),  name="app_monitor.report"),
            asyncio.create_task(self._fetch_loop(),   name="app_monitor.fetch"),
            asyncio.create_task(self._schedule_loop(), name="app_monitor.schedule"),
        ]
        log.info("AppMonitor started (platform=%s)", _SYSTEM)

    async def stop(self) -> None:
        self._stopped = True
        for t in self._tasks:
            t.cancel()
        await asyncio.gather(*self._tasks, return_exceptions=True)
        self._tasks.clear()

    # ------------------------------------------------------------------
    # Background loops
    # ------------------------------------------------------------------

    async def _poll_loop(self) -> None:
        """Detect process launches and exits; enforce lockdown."""
        while not self._stopped:
            await asyncio.sleep(self._POLL_INTERVAL)
            try:
                await self._scan_processes()
            except Exception:
                log.exception("poll_loop error")

    async def _timer_loop(self) -> None:
        """Tick timers down once per minute; fire warnings and kill apps."""
        while not self._stopped:
            await asyncio.sleep(60.0)
            try:
                await self._tick_timers()
            except Exception:
                log.exception("timer_loop error")

    async def _report_loop(self) -> None:
        """POST accumulated usage to the controller."""
        while not self._stopped:
            await asyncio.sleep(self._REPORT_INTERVAL)
            try:
                await self._post_usage_report()
            except Exception:
                log.exception("report_loop error")

    async def _fetch_loop(self) -> None:
        """Poll the controller for updated LockdownConfig."""
        while not self._stopped:
            await asyncio.sleep(self._FETCH_INTERVAL)
            try:
                await self._fetch_lockdown_config()
            except Exception:
                log.exception("fetch_loop error")

    async def _schedule_loop(self) -> None:
        """Monitor schedule window transitions; lock screen when window closes."""
        was_locked = self._config.schedule_locked
        while not self._stopped:
            await asyncio.sleep(10.0)
            try:
                is_locked = self._config.schedule_locked
                if is_locked and not was_locked:
                    log.info("Schedule window closed — locking screen")
                    await self._fire(AppEvent(
                        event_type=AppEventType.SCHEDULE_LOCKED,
                        app_name="",
                        message="Schedule window closed",
                    ))
                    await lock_screen()
                    # Kill all non-system processes
                    await self._kill_all_user_apps()
                was_locked = is_locked
            except Exception:
                log.exception("schedule_loop error")

    # ------------------------------------------------------------------
    # Core scan + enforcement
    # ------------------------------------------------------------------

    async def _scan_processes(self) -> None:
        cfg = self._config

        current = {p.pid: p for p in list_processes()}
        current_pids = set(current)
        known_pids   = set(self._known_pids)

        launched = current_pids - known_pids
        exited   = known_pids   - current_pids

        # Handle exits
        for pid in exited:
            info = self._known_pids.pop(pid, None)
            if info:
                self._running_apps.get(info.name, set()).discard(pid)
                if not self._running_apps.get(info.name):
                    self._running_apps.pop(info.name, None)
                await self._fire(AppEvent(
                    event_type=AppEventType.EXIT,
                    app_name=info.name,
                    pid=pid,
                ))

        # Handle launches
        for pid in launched:
            info = current[pid]
            self._known_pids[pid] = info
            self._running_apps.setdefault(info.name, set()).add(pid)

            await self._fire(AppEvent(
                event_type=AppEventType.LAUNCH,
                app_name=info.name,
                pid=pid,
            ))

            # Enforce lockdown
            if cfg.is_off():
                continue

            if cfg.schedule_locked and not cfg.override_active:
                await self._block(info, "schedule_locked")
                continue

            # Check if on break
            if self._on_break and not cfg.override_active:
                if time.time() < self._break_until:
                    await self._block(info, "break_due")
                    continue
                else:
                    self._on_break = False

            allowed, timer_id = cfg.is_allowed(info.name)
            if not allowed:
                await self._block(info, "policy")
                continue

            # Check timer exhaustion
            if timer_id and cfg.timer_remaining(timer_id) <= 0:
                await self._block(info, f"timer_expired:{timer_id}")
                continue

        # Kiosk keep-alive
        if cfg.policy_mode == "kiosk" and cfg.kiosk_app and not cfg.schedule_locked:
            await self._ensure_kiosk_app_running(cfg.kiosk_app)

        # Enforce blacklist on already-running processes (catches pre-existing)
        await self._enforce_existing(cfg)

    async def _enforce_existing(self, cfg: LockdownConfig) -> None:
        """Kill already-running blocked processes (runs on every scan)."""
        if cfg.is_off():
            return

        for pid, info in list(self._known_pids.items()):
            if cfg.schedule_locked and not cfg.override_active:
                await self._block(info, "schedule_locked")
                continue

            if self._on_break and time.time() < self._break_until and not cfg.override_active:
                await self._block(info, "break_due")
                continue

            allowed, timer_id = cfg.is_allowed(info.name)
            if not allowed:
                await self._block(info, "policy")
                continue

            if timer_id and cfg.timer_remaining(timer_id) <= 0:
                await self._block(info, f"timer_expired:{timer_id}")

    async def _block(self, info: ProcessInfo, reason: str) -> None:
        event_type = (
            AppEventType.TIMER_EXPIRED if reason.startswith("timer_expired")
            else AppEventType.BLOCKED
        )
        log.info("Blocking pid=%d name=%s reason=%s", info.pid, info.name, reason)
        await self._fire(AppEvent(
            event_type=event_type,
            app_name=info.name,
            pid=info.pid,
            message=reason,
        ))
        await kill_process(info.pid, info.name)

    async def _kill_all_user_apps(self) -> None:
        """Kill all tracked user-space processes (for schedule lock)."""
        for pid, info in list(self._known_pids.items()):
            await kill_process(pid, info.name)

    # ------------------------------------------------------------------
    # Kiosk keep-alive
    # ------------------------------------------------------------------

    async def _ensure_kiosk_app_running(self, app_pattern: str) -> None:
        """If the kiosk app is not running, restart it."""
        for name in self._running_apps:
            if fnmatch.fnmatch(name.lower(), app_pattern.lower()):
                return  # already running
        # Not running — attempt restart
        log.info("Kiosk app '%s' not running — attempting restart", app_pattern)
        await self._fire(AppEvent(
            event_type=AppEventType.KIOSK_RESTART,
            app_name=app_pattern,
            message="Kiosk app not found; restarting",
        ))
        await asyncio.sleep(self._KIOSK_RESPAWN_DELAY)
        await self._launch_app(app_pattern)

    async def _launch_app(self, app_name: str) -> None:
        """Best-effort app launch (for kiosk mode)."""
        try:
            if _SYSTEM == "Windows":
                subprocess.Popen(["start", app_name], shell=True)
            elif _SYSTEM == "Darwin":
                await asyncio.create_subprocess_exec(
                    "open", "-a", app_name,
                    stdout=asyncio.subprocess.DEVNULL,
                    stderr=asyncio.subprocess.DEVNULL,
                )
            else:
                await asyncio.create_subprocess_exec(
                    app_name,
                    stdout=asyncio.subprocess.DEVNULL,
                    stderr=asyncio.subprocess.DEVNULL,
                    start_new_session=True,
                )
        except Exception as exc:
            log.warning("Failed to launch kiosk app '%s': %s", app_name, exc)

    # ------------------------------------------------------------------
    # Timer countdown
    # ------------------------------------------------------------------

    async def _tick_timers(self) -> None:
        """Called every 60 s — decrement timers for active apps."""
        cfg = self._config
        if cfg.is_off():
            return

        # Find which timers have active apps
        active_timer_ids: set[str] = set()
        for app_name in list(self._running_apps.keys()):
            for timer_id, patterns in self._timer_patterns.items():
                if any(fnmatch.fnmatch(app_name.lower(), p.lower()) for p in patterns):
                    active_timer_ids.add(timer_id)
                    # Accumulate usage
                    self._usage_acc[app_name] = self._usage_acc.get(app_name, 0) + 1

        for timer_id in active_timer_ids:
            if timer_id not in cfg.timers:
                continue
            remaining = cfg.decrement_timer(timer_id)
            warn_threshold = cfg.timer_warnings.get(timer_id, 5)

            if remaining == 0:
                # Kill apps using this timer
                log.info("Timer '%s' exhausted — killing tracked apps", timer_id)
                for app_name, pids in list(self._running_apps.items()):
                    patterns = self._timer_patterns.get(timer_id, [])
                    if any(fnmatch.fnmatch(app_name.lower(), p.lower()) for p in patterns):
                        for pid in list(pids):
                            info = self._known_pids.get(pid)
                            if info:
                                await self._fire(AppEvent(
                                    event_type=AppEventType.TIMER_EXPIRED,
                                    app_name=app_name,
                                    pid=pid,
                                    timer_id=timer_id,
                                    timer_remaining=0,
                                    message=f"Timer '{timer_id}' exhausted",
                                ))
                                await kill_process(pid, app_name)
            elif remaining <= warn_threshold and timer_id not in cfg._timer_warned:
                cfg._timer_warned.add(timer_id)
                await self._fire(AppEvent(
                    event_type=AppEventType.TIMER_WARNING,
                    app_name="",
                    timer_id=timer_id,
                    timer_remaining=remaining,
                    message=f"{remaining} minutes remaining for timer '{timer_id}'",
                ))

        self._persist_config()

        # Break policy
        bp = cfg.break_policy
        if bp.get("enabled") and self._running_apps and not self._on_break:
            play_minutes = int(bp.get("play_minutes", 60))
            if self._session_start is None:
                self._session_start = time.time()
            elapsed_minutes = (time.time() - self._session_start) / 60
            warn_minutes = int(bp.get("warning_minutes", 5))
            if elapsed_minutes >= play_minutes:
                await self._trigger_break(bp)
            elif elapsed_minutes >= play_minutes - warn_minutes:
                await self._fire(AppEvent(
                    event_type=AppEventType.BREAK_DUE,
                    app_name="",
                    message=f"Break due in ~{int(play_minutes - elapsed_minutes)} minutes",
                ))

    async def _trigger_break(self, bp: dict[str, Any]) -> None:
        break_minutes = int(bp.get("break_minutes", 10))
        self._on_break   = True
        self._break_until = time.time() + break_minutes * 60
        self._session_start = None
        log.info("Break triggered — %d minutes", break_minutes)
        await self._fire(AppEvent(
            event_type=AppEventType.BREAK_DUE,
            app_name="",
            message=f"Break started — {break_minutes} minutes required",
        ))
        await self._kill_all_user_apps()
        await lock_screen()

    # ------------------------------------------------------------------
    # Controller communication
    # ------------------------------------------------------------------

    async def _fetch_lockdown_config(self) -> None:
        """GET /api/v1/parental/enforcement/{device_id} from controller."""
        try:
            import aiohttp  # type: ignore
        except ImportError:
            log.debug("aiohttp not available — skipping config fetch")
            return

        url = f"{self._controller_url}/api/v1/parental/enforcement/{self._device_id}"
        headers = {"Authorization": f"Bearer {self._token}"} if self._token else {}
        try:
            async with aiohttp.ClientSession() as session:
                async with session.get(url, headers=headers, timeout=aiohttp.ClientTimeout(total=10)) as resp:
                    if resp.status == 200:
                        data = await resp.json()
                        new_cfg = LockdownConfig.from_dict(data)
                        if new_cfg.generated_at != self._config.generated_at:
                            self.apply_config(new_cfg)
                    elif resp.status == 404:
                        # No profile assigned — go unrestricted
                        if not self._config.is_off():
                            self.apply_config(LockdownConfig.off())
        except Exception as exc:
            log.debug("fetch_lockdown_config error: %s", exc)

    async def _post_usage_report(self) -> None:
        """POST minute-resolution usage increments to controller."""
        if not self._usage_acc:
            return

        report = {
            "device_id":  self._device_id,
            "profile_id": self._config.profile_id,
            "increments": {k: v for k, v in self._usage_acc.items()},
            "reported_at": time.time(),
        }
        self._usage_acc.clear()

        try:
            import aiohttp  # type: ignore
        except ImportError:
            return

        url = f"{self._controller_url}/api/v1/parental/usage/report"
        headers = {
            "Content-Type": "application/json",
            **({"Authorization": f"Bearer {self._token}"} if self._token else {}),
        }
        try:
            async with aiohttp.ClientSession() as session:
                async with session.post(
                    url, json=report, headers=headers,
                    timeout=aiohttp.ClientTimeout(total=10),
                ) as resp:
                    if resp.status not in (200, 204):
                        log.warning("usage report returned %d", resp.status)
        except Exception as exc:
            log.debug("post_usage_report error: %s", exc)

    # ------------------------------------------------------------------
    # Persistence
    # ------------------------------------------------------------------

    def _persist_config(self) -> None:
        try:
            tmp = self._lockdown_file.with_suffix(".tmp")
            tmp.write_text(json.dumps(self._config.to_persist(), indent=2))
            tmp.chmod(0o600)
            tmp.replace(self._lockdown_file)
        except Exception as exc:
            log.warning("Failed to persist lockdown config: %s", exc)

    def _load_persisted_config(self) -> LockdownConfig:
        if not self._lockdown_file.exists():
            return LockdownConfig.off()
        try:
            data = json.loads(self._lockdown_file.read_text())
            cfg = LockdownConfig.from_dict(data)
            log.info("Loaded persisted lockdown config profile=%s", cfg.profile_id)
            return cfg
        except Exception as exc:
            log.warning("Failed to load lockdown config: %s — using off", exc)
            return LockdownConfig.off()

    # ------------------------------------------------------------------
    # Hook dispatch
    # ------------------------------------------------------------------

    async def _fire(self, event: AppEvent) -> None:
        log.debug("AppEvent %s app=%s pid=%d", event.event_type, event.app_name, event.pid)
        for hook in self._hooks:
            try:
                result = hook(event)
                if asyncio.iscoroutine(result):
                    await result
            except Exception:
                log.exception("Hook error for %s", event.event_type)


# ---------------------------------------------------------------------------
# Convenience: standalone usage (e.g. ozma-desktop-agent integration)
# ---------------------------------------------------------------------------

class AppMonitorIntegration:
    """
    Drop-in integration helper for ozma_desktop_agent.py.

    Example
    -------
    integration = AppMonitorIntegration(agent)
    await integration.start()
    """

    def __init__(self, agent: Any) -> None:
        self._agent   = agent
        self._monitor: AppMonitor | None = None

    async def start(self) -> None:
        agent = self._agent
        monitor = AppMonitor(
            controller_url = getattr(agent, "controller_url", "http://localhost:7380"),
            device_id      = getattr(agent, "device_id", ""),
            token          = getattr(agent, "_token", ""),
            data_dir       = Path(os.path.expanduser("~/.ozma")),
        )

        async def _notify(event: AppEvent) -> None:
            # Forward notable events to the agent's event queue if available
            queue = getattr(agent, "_event_queue", None)
            if queue is not None:
                await queue.put({"type": "app_event", **event.to_dict()})

        monitor.add_hook(_notify)
        self._monitor = monitor
        await monitor.start()

    async def stop(self) -> None:
        if self._monitor:
            await self._monitor.stop()

    def apply_lockdown(self, config_dict: dict[str, Any]) -> None:
        if self._monitor:
            self._monitor.apply_config(LockdownConfig.from_dict(config_dict))
