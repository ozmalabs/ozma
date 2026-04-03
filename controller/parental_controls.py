# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Parental controls — child profiles, app whitelists, time budgets, schedules.

Concepts
--------
ChildProfile
    A named profile (not necessarily linked to a user account) that governs
    what a particular operator/seat/device is allowed to do.  Parents assign
    profiles to devices; the profile travels with the device regardless of
    who is logged in.

AppRule
    A single allow/block rule for an application.  Rules are evaluated top-
    down; first match wins.  The default policy (whitelist_only or blacklist)
    applies when nothing matches.

AppTimer
    A daily time budget for a group of applications (e.g. "2 hours of games",
    "30 minutes of YouTube").  Multiple patterns can share one timer.

ScheduleWindow
    A day-of-week + clock-time window during which usage is permitted.
    Outside all windows the device is locked.

BreakPolicy
    Forces a cooldown break after N minutes of continuous use.

PermissionResult
    Return value of check_permission(): action ("allow"/"block"/"warn"),
    reason, and how many minutes remain on the relevant timer (if any).

LockdownConfig
    Serialisable snapshot pushed to the agent every time the effective
    enforcement state changes.  The agent uses this to kill blocked
    processes and keep kiosk apps alive.

UsageRecord
    Per-profile, per-pattern daily usage stored to disk.  Agents POST
    minute-resolution increments; the manager accumulates them.

OverrideSession
    A parent-granted temporary bypass (PIN-protected) that suspends
    profile enforcement for a set duration.
"""

from __future__ import annotations

import fnmatch
import hashlib
import json
import logging
import secrets
import time
from dataclasses import dataclass, field
from datetime import datetime, date
from enum import Enum
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.parental")

# Days of week abbreviations
_DAYS = ("mon", "tue", "wed", "thu", "fri", "sat", "sun")
_WEEKDAYS = frozenset(_DAYS[:5])
_WEEKEND   = frozenset(_DAYS[5:])

# ---------------------------------------------------------------------------
# Enumerations
# ---------------------------------------------------------------------------

class PolicyMode(str, Enum):
    WHITELIST = "whitelist"   # only listed apps allowed
    BLACKLIST = "blacklist"   # only listed apps blocked; everything else allowed
    KIOSK     = "kiosk"       # single app; everything else killed


class ContentFilter(str, Enum):
    OFF      = "off"
    MODERATE = "moderate"
    STRICT   = "strict"


class AppAction(str, Enum):
    ALLOW = "allow"
    BLOCK = "block"
    LIMIT = "limit"   # allow but subject to a timer


# ---------------------------------------------------------------------------
# Data models
# ---------------------------------------------------------------------------

@dataclass
class AppRule:
    """Single allow/block rule matched against an app name or path."""
    rule_id:   str
    pattern:   str          # glob: "fortnite*", "*.exe", "@games" category tag
    action:    AppAction    = AppAction.ALLOW
    timer_id:  str          = ""    # non-empty → subject to this timer
    label:     str          = ""

    def matches(self, app_name: str) -> bool:
        """Case-insensitive glob match."""
        return fnmatch.fnmatch(app_name.lower(), self.pattern.lower())

    def to_dict(self) -> dict[str, Any]:
        return {
            "rule_id":  self.rule_id,
            "pattern":  self.pattern,
            "action":   self.action,
            "timer_id": self.timer_id,
            "label":    self.label,
        }

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> "AppRule":
        return cls(
            rule_id  = d["rule_id"],
            pattern  = d["pattern"],
            action   = AppAction(d.get("action", "allow")),
            timer_id = d.get("timer_id", ""),
            label    = d.get("label", ""),
        )


@dataclass
class AppTimer:
    """Daily time budget for one or more application patterns."""
    timer_id:        str
    label:           str
    patterns:        list[str]     # patterns this timer covers
    daily_minutes:   int           # total budget per day
    reset_hour:      int   = 0    # hour (0-23) when budget resets each day
    warning_minutes: int   = 5    # warn when this many minutes remain
    days_active:     list[str] = field(default_factory=lambda: list(_DAYS))
    # "mon"-"sun", "weekday", "weekend", "daily" — resolved at check time

    def matches(self, app_name: str) -> bool:
        return any(fnmatch.fnmatch(app_name.lower(), p.lower()) for p in self.patterns)

    def is_active_today(self, now: datetime | None = None) -> bool:
        now = now or datetime.now()
        day = _DAYS[now.weekday()]
        for d in self.days_active:
            if d == "daily":
                return True
            if d == "weekday" and day in _WEEKDAYS:
                return True
            if d == "weekend" and day in _WEEKEND:
                return True
            if d == day:
                return True
        return False

    def to_dict(self) -> dict[str, Any]:
        return {
            "timer_id":        self.timer_id,
            "label":           self.label,
            "patterns":        self.patterns,
            "daily_minutes":   self.daily_minutes,
            "reset_hour":      self.reset_hour,
            "warning_minutes": self.warning_minutes,
            "days_active":     self.days_active,
        }

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> "AppTimer":
        return cls(
            timer_id        = d["timer_id"],
            label           = d["label"],
            patterns        = d.get("patterns", []),
            daily_minutes   = int(d.get("daily_minutes", 60)),
            reset_hour      = int(d.get("reset_hour", 0)),
            warning_minutes = int(d.get("warning_minutes", 5)),
            days_active     = d.get("days_active", list(_DAYS)),
        )


@dataclass
class ScheduleWindow:
    """A day-of-week + clock window in which usage is permitted."""
    day:        str    # "mon"-"sun" | "weekday" | "weekend" | "daily"
    start_time: str    # "HH:MM" (24-hour, local time)
    end_time:   str    # "HH:MM"
    label:      str = ""

    def active_now(self, now: datetime | None = None) -> bool:
        now = now or datetime.now()
        day = _DAYS[now.weekday()]
        # Check day match
        if self.day == "daily":
            pass
        elif self.day == "weekday" and day not in _WEEKDAYS:
            return False
        elif self.day == "weekend" and day not in _WEEKEND:
            return False
        elif self.day not in ("daily", "weekday", "weekend") and self.day != day:
            return False
        # Check time range
        clock = now.strftime("%H:%M")
        return self.start_time <= clock < self.end_time

    def to_dict(self) -> dict[str, Any]:
        return {"day": self.day, "start_time": self.start_time,
                "end_time": self.end_time, "label": self.label}

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> "ScheduleWindow":
        return cls(
            day        = d["day"],
            start_time = d["start_time"],
            end_time   = d["end_time"],
            label      = d.get("label", ""),
        )


@dataclass
class BreakPolicy:
    """Force a cooldown break after continuous use."""
    enabled:          bool = False
    play_minutes:     int  = 45   # play session length before break
    break_minutes:    int  = 15   # required break duration
    warning_minutes:  int  = 5    # warn before session ends

    def to_dict(self) -> dict[str, Any]:
        return {
            "enabled":         self.enabled,
            "play_minutes":    self.play_minutes,
            "break_minutes":   self.break_minutes,
            "warning_minutes": self.warning_minutes,
        }

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> "BreakPolicy":
        return cls(
            enabled         = bool(d.get("enabled", False)),
            play_minutes    = int(d.get("play_minutes", 45)),
            break_minutes   = int(d.get("break_minutes", 15)),
            warning_minutes = int(d.get("warning_minutes", 5)),
        )


@dataclass
class ChildProfile:
    """
    Parental-control profile applied to one or more devices.

    Not necessarily linked to a user account — a shared family TV can
    have a profile without any logged-in user.
    """
    profile_id:     str
    name:           str
    age:            int            = 0
    device_ids:     list[str]      = field(default_factory=list)
    linked_user_id: str            = ""   # optional link to a User account

    # Policy
    policy_mode:    PolicyMode     = PolicyMode.BLACKLIST
    kiosk_app:      str            = ""   # if mode == KIOSK
    rules:          list[AppRule]  = field(default_factory=list)
    timers:         list[AppTimer] = field(default_factory=list)

    # Schedule (empty list = always allowed)
    schedule:       list[ScheduleWindow] = field(default_factory=list)
    bedtime:        str            = ""   # "HH:MM" hard lockout; "" = disabled

    # Break enforcement
    break_policy:   BreakPolicy    = field(default_factory=BreakPolicy)

    # Content filter
    content_filter: ContentFilter  = ContentFilter.OFF

    # Override PIN (SHA-256 hex of 6-digit PIN, empty = disabled)
    override_pin_hash: str         = ""

    # Metadata
    created_at:     float          = field(default_factory=time.time)
    updated_at:     float          = field(default_factory=time.time)
    notes:          str            = ""

    # ------------------------------------------------------------------

    def is_in_schedule(self, now: datetime | None = None) -> bool:
        """Return True if current time falls within any scheduled window."""
        now = now or datetime.now()
        if not self.schedule:
            return True   # no schedule = always allowed
        # Check bedtime first
        if self.bedtime:
            clock = now.strftime("%H:%M")
            if clock >= self.bedtime:
                return False
        return any(w.active_now(now) for w in self.schedule)

    def get_timer(self, timer_id: str) -> AppTimer | None:
        return next((t for t in self.timers if t.timer_id == timer_id), None)

    def find_rule(self, app_name: str) -> AppRule | None:
        """Return first matching rule, or None."""
        return next((r for r in self.rules if r.matches(app_name)), None)

    def find_timer(self, app_name: str, now: datetime | None = None) -> AppTimer | None:
        """Return the first active timer that covers this app."""
        now = now or datetime.now()
        for t in self.timers:
            if t.matches(app_name) and t.is_active_today(now):
                return t
        return None

    def check_pin(self, pin: str) -> bool:
        if not self.override_pin_hash:
            return False
        h = hashlib.sha256(pin.encode()).hexdigest()
        return secrets.compare_digest(h, self.override_pin_hash)

    def to_dict(self) -> dict[str, Any]:
        return {
            "profile_id":        self.profile_id,
            "name":              self.name,
            "age":               self.age,
            "device_ids":        self.device_ids,
            "linked_user_id":    self.linked_user_id,
            "policy_mode":       self.policy_mode,
            "kiosk_app":         self.kiosk_app,
            "rules":             [r.to_dict() for r in self.rules],
            "timers":            [t.to_dict() for t in self.timers],
            "schedule":          [s.to_dict() for s in self.schedule],
            "bedtime":           self.bedtime,
            "break_policy":      self.break_policy.to_dict(),
            "content_filter":    self.content_filter,
            "override_pin_hash": self.override_pin_hash,
            "created_at":        self.created_at,
            "updated_at":        self.updated_at,
            "notes":             self.notes,
        }

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> "ChildProfile":
        p = cls(
            profile_id     = d["profile_id"],
            name           = d["name"],
            age            = int(d.get("age", 0)),
            device_ids     = d.get("device_ids", []),
            linked_user_id = d.get("linked_user_id", ""),
            policy_mode    = PolicyMode(d.get("policy_mode", "blacklist")),
            kiosk_app      = d.get("kiosk_app", ""),
            bedtime        = d.get("bedtime", ""),
            content_filter = ContentFilter(d.get("content_filter", "off")),
            override_pin_hash = d.get("override_pin_hash", ""),
            created_at     = d.get("created_at", time.time()),
            updated_at     = d.get("updated_at", time.time()),
            notes          = d.get("notes", ""),
        )
        p.rules        = [AppRule.from_dict(r) for r in d.get("rules", [])]
        p.timers       = [AppTimer.from_dict(t) for t in d.get("timers", [])]
        p.schedule     = [ScheduleWindow.from_dict(s) for s in d.get("schedule", [])]
        p.break_policy = BreakPolicy.from_dict(d.get("break_policy", {}))
        return p


# ---------------------------------------------------------------------------
# Usage tracking
# ---------------------------------------------------------------------------

@dataclass
class UsageRecord:
    """Daily usage accumulator for one profile + app pattern."""
    profile_id:   str
    date:         str    # "YYYY-MM-DD"
    pattern:      str    # matched pattern key (e.g. timer label or app name)
    minutes_used: float  = 0.0
    sessions:     int    = 0

    def to_dict(self) -> dict[str, Any]:
        return {
            "profile_id":   self.profile_id,
            "date":         self.date,
            "pattern":      self.pattern,
            "minutes_used": self.minutes_used,
            "sessions":     self.sessions,
        }

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> "UsageRecord":
        return cls(
            profile_id   = d["profile_id"],
            date         = d["date"],
            pattern      = d["pattern"],
            minutes_used = float(d.get("minutes_used", 0)),
            sessions     = int(d.get("sessions", 0)),
        )


# ---------------------------------------------------------------------------
# Permission result
# ---------------------------------------------------------------------------

@dataclass
class PermissionResult:
    """Outcome of check_permission()."""
    allowed:           bool
    action:            str     # "allow" | "block" | "warn"
    reason:            str
    timer_id:          str     = ""
    timer_label:       str     = ""
    timer_remaining:   int     = -1  # minutes remaining today; -1 = no timer
    timer_warning:     bool    = False
    schedule_locked:   bool    = False
    override_active:   bool    = False

    def to_dict(self) -> dict[str, Any]:
        return {
            "allowed":         self.allowed,
            "action":          self.action,
            "reason":          self.reason,
            "timer_id":        self.timer_id,
            "timer_label":     self.timer_label,
            "timer_remaining": self.timer_remaining,
            "timer_warning":   self.timer_warning,
            "schedule_locked": self.schedule_locked,
            "override_active": self.override_active,
        }


# ---------------------------------------------------------------------------
# Override session
# ---------------------------------------------------------------------------

@dataclass
class OverrideSession:
    """Parent-granted temporary bypass of profile restrictions."""
    override_id: str
    profile_id:  str
    device_id:   str
    expires_at:  float
    granted_by:  str    = ""   # user_id of granting parent
    reason:      str    = ""

    def is_active(self) -> bool:
        return time.time() < self.expires_at

    def to_dict(self) -> dict[str, Any]:
        return {
            "override_id": self.override_id,
            "profile_id":  self.profile_id,
            "device_id":   self.device_id,
            "expires_at":  self.expires_at,
            "granted_by":  self.granted_by,
            "reason":      self.reason,
            "active":      self.is_active(),
        }


# ---------------------------------------------------------------------------
# LockdownConfig (pushed to agent)
# ---------------------------------------------------------------------------

@dataclass
class LockdownConfig:
    """
    Serialisable enforcement snapshot sent to the agent.

    The agent applies this without contacting the controller again until
    the next push.  It is also persisted locally on the agent so
    enforcement continues across agent restarts and controller outages.
    """
    profile_id:       str
    policy_mode:      str          # "off" | "whitelist" | "blacklist" | "kiosk"
    whitelist:        list[str]    # patterns allowed (whitelist/kiosk mode)
    blacklist:        list[str]    # patterns blocked (blacklist mode)
    kiosk_app:        str          # exact app to keep alive in kiosk mode
    schedule_locked:  bool         # True if outside schedule window right now
    override_active:  bool         # True if a parent override is in effect
    override_until:   float        = 0.0
    timers:           dict[str, int] = field(default_factory=dict)
    # timer_id → remaining minutes today (agent counts down locally)
    timer_warnings:   dict[str, int] = field(default_factory=dict)
    # timer_id → warning threshold in minutes
    break_policy:     dict[str, Any] = field(default_factory=dict)
    content_filter:   str          = "off"
    generated_at:     float        = field(default_factory=time.time)

    def is_off(self) -> bool:
        return self.policy_mode == "off"

    def is_allowed(self, app_name: str) -> tuple[bool, str]:
        """Return (allowed, timer_id). Convenience mirror of agent-side method."""
        if self.is_off() or self.override_active:
            return True, ""
        name = app_name.lower()
        if self.policy_mode == "kiosk":
            return fnmatch.fnmatch(name, self.kiosk_app.lower()), ""
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

    def to_dict(self) -> dict[str, Any]:
        return {
            "profile_id":      self.profile_id,
            "policy_mode":     self.policy_mode,
            "whitelist":       self.whitelist,
            "blacklist":       self.blacklist,
            "kiosk_app":       self.kiosk_app,
            "schedule_locked": self.schedule_locked,
            "override_active": self.override_active,
            "override_until":  self.override_until,
            "timers":          self.timers,
            "timer_warnings":  self.timer_warnings,
            "break_policy":    self.break_policy,
            "content_filter":  self.content_filter,
            "generated_at":    self.generated_at,
        }

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> "LockdownConfig":
        return cls(
            profile_id      = d.get("profile_id", ""),
            policy_mode     = d.get("policy_mode", "off"),
            whitelist       = d.get("whitelist", []),
            blacklist       = d.get("blacklist", []),
            kiosk_app       = d.get("kiosk_app", ""),
            schedule_locked = bool(d.get("schedule_locked", False)),
            override_active = bool(d.get("override_active", False)),
            override_until  = float(d.get("override_until", 0)),
            timers          = d.get("timers", {}),
            timer_warnings  = d.get("timer_warnings", {}),
            break_policy    = d.get("break_policy", {}),
            content_filter  = d.get("content_filter", "off"),
            generated_at    = float(d.get("generated_at", 0)),
        )

    @classmethod
    def off(cls) -> "LockdownConfig":
        """No restrictions."""
        return cls(
            profile_id="", policy_mode="off",
            whitelist=[], blacklist=[], kiosk_app="",
            schedule_locked=False, override_active=False,
        )


# ---------------------------------------------------------------------------
# ParentalControlsManager
# ---------------------------------------------------------------------------

class ParentalControlsManager:
    """
    Central manager for child profiles, usage tracking, and enforcement.

    Responsibilities
    ----------------
    * CRUD for ChildProfile objects (persisted to profiles.json)
    * Usage accumulation from agent reports (persisted to usage.json)
    * check_permission() — evaluates schedule + rules + timers
    * get_enforcement_state() — builds LockdownConfig for a device
    * grant_override() / revoke_override() — parent PIN bypass
    * Background enforcement loop — pushes updated LockdownConfig to
      agents when schedule transitions or timers expire
    """

    _PUSH_INTERVAL = 30.0   # seconds between agent enforcement pushes

    def __init__(
        self,
        data_dir: Path | None = None,
        agent_client: Any | None = None,   # for pushing to agents
    ) -> None:
        self._data_dir = data_dir or Path("/var/lib/ozma/parental")
        self._agent_client = agent_client
        self._profiles:  dict[str, ChildProfile]   = {}  # profile_id → profile
        self._usage:     dict[str, UsageRecord]    = {}  # "{pid}:{date}:{pat}" → record
        self._overrides: dict[str, OverrideSession] = {}  # override_id → session
        self._tasks:     list[Any]                  = []
        self._load()

    # ------------------------------------------------------------------
    # Lifecycle
    # ------------------------------------------------------------------

    async def start(self) -> None:
        import asyncio
        self._data_dir.mkdir(parents=True, exist_ok=True)
        self._tasks.append(
            asyncio.create_task(self._enforcement_loop(), name="parental:enforcement")
        )
        log.info("ParentalControlsManager started (%d profiles)", len(self._profiles))

    async def stop(self) -> None:
        import asyncio
        for t in self._tasks:
            t.cancel()
        await asyncio.gather(*self._tasks, return_exceptions=True)
        self._tasks.clear()

    # ------------------------------------------------------------------
    # Profile CRUD
    # ------------------------------------------------------------------

    def create_profile(
        self,
        name: str,
        age: int = 0,
        device_ids: list[str] | None = None,
        policy_mode: PolicyMode = PolicyMode.BLACKLIST,
        override_pin: str | None = None,
        content_filter: str = "off",
        **kwargs: Any,
    ) -> ChildProfile:
        profile_id = secrets.token_hex(8)
        p = ChildProfile(
            profile_id     = profile_id,
            name           = name,
            age            = age,
            device_ids     = list(device_ids or []),
            policy_mode    = PolicyMode(policy_mode) if isinstance(policy_mode, str) else policy_mode,
            content_filter = ContentFilter(content_filter),
            **{k: v for k, v in kwargs.items() if hasattr(ChildProfile, k)},
        )
        if override_pin:
            p.override_pin_hash = hashlib.sha256(override_pin.encode()).hexdigest()
        self._profiles[profile_id] = p
        self._save_profiles()
        log.info("Child profile created: %s (%s)", name, profile_id)
        return p

    def update_profile(self, profile_id: str, **kwargs: Any) -> ChildProfile | None:
        p = self._profiles.get(profile_id)
        if not p:
            return None
        for k, v in kwargs.items():
            if k == "rules":
                p.rules = [AppRule.from_dict(r) for r in v]
            elif k == "timers":
                p.timers = [AppTimer.from_dict(t) for t in v]
            elif k == "schedule":
                p.schedule = [ScheduleWindow.from_dict(s) for s in v]
            elif k == "break_policy":
                p.break_policy = BreakPolicy.from_dict(v)
            elif k == "policy_mode":
                p.policy_mode = PolicyMode(v)
            elif k == "content_filter":
                p.content_filter = ContentFilter(v)
            elif k == "override_pin":
                p.override_pin_hash = (
                    hashlib.sha256(str(v).encode()).hexdigest() if v else ""
                )
            elif hasattr(p, k):
                setattr(p, k, v)
        p.updated_at = time.time()
        self._save_profiles()
        return p

    def delete_profile(self, profile_id: str) -> bool:
        if profile_id not in self._profiles:
            return False
        del self._profiles[profile_id]
        self._save_profiles()
        return True

    def get_profile(self, profile_id: str) -> ChildProfile | None:
        return self._profiles.get(profile_id)

    def list_profiles(self) -> list[dict[str, Any]]:
        return [p.to_dict() for p in self._profiles.values()]

    def get_profile_for_device(self, device_id: str) -> ChildProfile | None:
        for p in self._profiles.values():
            if device_id in p.device_ids:
                return p
        return None

    def assign_device(self, profile_id: str, device_id: str) -> bool:
        p = self._profiles.get(profile_id)
        if not p:
            return False
        if device_id not in p.device_ids:
            p.device_ids.append(device_id)
            self._save_profiles()
        return True

    def unassign_device(self, device_id: str) -> bool:
        changed = False
        for p in self._profiles.values():
            if device_id in p.device_ids:
                p.device_ids.remove(device_id)
                changed = True
        if changed:
            self._save_profiles()
        return changed

    # ------------------------------------------------------------------
    # Rule / timer helpers
    # ------------------------------------------------------------------

    def add_rule(self, profile_id: str, pattern: str, action: str,
                 timer_id: str = "", label: str = "") -> AppRule | None:
        p = self._profiles.get(profile_id)
        if not p:
            return None
        rule = AppRule(
            rule_id  = secrets.token_hex(4),
            pattern  = pattern,
            action   = AppAction(action),
            timer_id = timer_id,
            label    = label,
        )
        p.rules.append(rule)
        p.updated_at = time.time()
        self._save_profiles()
        return rule

    def remove_rule(self, profile_id: str, rule_id: str) -> bool:
        p = self._profiles.get(profile_id)
        if not p:
            return False
        before = len(p.rules)
        p.rules = [r for r in p.rules if r.rule_id != rule_id]
        if len(p.rules) < before:
            p.updated_at = time.time()
            self._save_profiles()
            return True
        return False

    def add_timer(
        self,
        profile_id:      str,
        label:           str,
        patterns:        list[str],
        daily_minutes:   int,
        reset_hour:      int = 0,
        warning_minutes: int = 5,
        days_active:     list[str] | None = None,
    ) -> AppTimer | None:
        p = self._profiles.get(profile_id)
        if not p:
            return None
        timer = AppTimer(
            timer_id        = secrets.token_hex(4),
            label           = label,
            patterns        = patterns,
            daily_minutes   = daily_minutes,
            reset_hour      = reset_hour,
            warning_minutes = warning_minutes,
            days_active     = days_active or list(_DAYS),
        )
        p.timers.append(timer)
        p.updated_at = time.time()
        self._save_profiles()
        return timer

    def remove_timer(self, profile_id: str, timer_id: str) -> bool:
        p = self._profiles.get(profile_id)
        if not p:
            return False
        before = len(p.timers)
        p.timers = [t for t in p.timers if t.timer_id != timer_id]
        if len(p.timers) < before:
            p.updated_at = time.time()
            self._save_profiles()
            return True
        return False

    # ------------------------------------------------------------------
    # Permission checking
    # ------------------------------------------------------------------

    def check_permission(
        self,
        profile_id: str,
        app_name:   str,
        device_id:  str = "",
        now:        datetime | None = None,
    ) -> PermissionResult:
        """
        Evaluate whether app_name is permitted under profile_id right now.
        """
        now = now or datetime.now()
        p = self._profiles.get(profile_id)
        if not p:
            return PermissionResult(allowed=True, action="allow", reason="no profile")

        # Check for active override first
        override = self._active_override(profile_id, device_id)
        if override:
            return PermissionResult(
                allowed=True, action="allow",
                reason="parent override active",
                override_active=True,
            )

        # Schedule check
        if not p.is_in_schedule(now):
            return PermissionResult(
                allowed=False, action="block",
                reason="outside allowed schedule",
                schedule_locked=True,
            )

        # Kiosk mode: only the kiosk app is allowed
        if p.policy_mode == PolicyMode.KIOSK:
            allowed = fnmatch.fnmatch(app_name.lower(), p.kiosk_app.lower())
            return PermissionResult(
                allowed=allowed,
                action="allow" if allowed else "block",
                reason="kiosk mode" if not allowed else "kiosk app",
            )

        # Evaluate rules
        rule = p.find_rule(app_name)

        if rule:
            if rule.action == AppAction.BLOCK:
                return PermissionResult(
                    allowed=False, action="block",
                    reason=f"blocked by rule: {rule.label or rule.pattern}",
                )
            # Allow or Limit — check timer if present
            timer_id = rule.timer_id
        else:
            # No rule matched — apply default policy
            if p.policy_mode == PolicyMode.WHITELIST:
                return PermissionResult(
                    allowed=False, action="block",
                    reason="not in whitelist",
                )
            # Blacklist default: check if there's a timer for this app anyway
            timer_id = ""

        # Check timer (from rule or from any timer whose patterns match)
        timer = (
            p.get_timer(timer_id) if timer_id
            else p.find_timer(app_name, now)
        )
        if timer:
            used = self._get_usage_minutes(profile_id, timer.timer_id, now)
            remaining = max(0, timer.daily_minutes - int(used))
            if remaining <= 0:
                return PermissionResult(
                    allowed=False, action="block",
                    reason=f"daily limit reached: {timer.label}",
                    timer_id=timer.timer_id,
                    timer_label=timer.label,
                    timer_remaining=0,
                )
            warn = remaining <= timer.warning_minutes
            return PermissionResult(
                allowed=True,
                action="warn" if warn else "allow",
                reason=f"{remaining}m remaining on {timer.label}",
                timer_id=timer.timer_id,
                timer_label=timer.label,
                timer_remaining=remaining,
                timer_warning=warn,
            )

        return PermissionResult(allowed=True, action="allow", reason="permitted")

    # ------------------------------------------------------------------
    # Usage tracking
    # ------------------------------------------------------------------

    def record_usage(
        self,
        profile_id: str,
        app_name:   str,
        minutes:    float,
        day:        str | None = None,
    ) -> None:
        """
        Add minutes of usage for app_name under profile_id.

        Called by agent reports (typically every 60 seconds).
        The key is the matched timer label if a timer covers this app,
        otherwise the raw app_name.
        """
        if not day:
            day = date.today().isoformat()

        p = self._profiles.get(profile_id)
        key_pattern = app_name
        if p:
            timer = p.find_timer(app_name)
            if timer:
                key_pattern = timer.timer_id

        rec_key = f"{profile_id}:{day}:{key_pattern}"
        if rec_key not in self._usage:
            self._usage[rec_key] = UsageRecord(
                profile_id=profile_id, date=day, pattern=key_pattern
            )
        rec = self._usage[rec_key]
        rec.minutes_used += minutes
        rec.sessions = max(rec.sessions, 1)
        self._save_usage()

    def increment_session(self, profile_id: str, app_name: str, day: str | None = None) -> None:
        if not day:
            day = date.today().isoformat()
        p = self._profiles.get(profile_id)
        key_pattern = app_name
        if p:
            timer = p.find_timer(app_name)
            if timer:
                key_pattern = timer.timer_id
        rec_key = f"{profile_id}:{day}:{key_pattern}"
        if rec_key not in self._usage:
            self._usage[rec_key] = UsageRecord(
                profile_id=profile_id, date=day, pattern=key_pattern
            )
        self._usage[rec_key].sessions += 1
        self._save_usage()

    def get_usage_summary(
        self, profile_id: str, day: str | None = None
    ) -> dict[str, Any]:
        """Return usage summary for a profile on a given date."""
        if not day:
            day = date.today().isoformat()
        p = self._profiles.get(profile_id)
        records = [
            r.to_dict() for r in self._usage.values()
            if r.profile_id == profile_id and r.date == day
        ]
        # Attach remaining minutes to timer records
        if p:
            now = datetime.now()
            timer_states = []
            for t in p.timers:
                if not t.is_active_today(now):
                    continue
                used = self._get_usage_minutes(profile_id, t.timer_id, now)
                timer_states.append({
                    "timer_id":      t.timer_id,
                    "label":         t.label,
                    "daily_minutes": t.daily_minutes,
                    "used_minutes":  round(used, 1),
                    "remaining":     max(0, t.daily_minutes - int(used)),
                })
        else:
            timer_states = []
        return {
            "profile_id": profile_id,
            "date":       day,
            "records":    records,
            "timers":     timer_states,
        }

    def reset_timer(self, profile_id: str, timer_id: str, day: str | None = None) -> bool:
        """Parent manually resets a timer (zeroes today's usage for that timer)."""
        if not day:
            day = date.today().isoformat()
        rec_key = f"{profile_id}:{day}:{timer_id}"
        if rec_key in self._usage:
            self._usage[rec_key].minutes_used = 0.0
            self._save_usage()
        log.info("Timer %s reset for profile %s on %s", timer_id, profile_id, day)
        return True

    def _get_usage_minutes(
        self, profile_id: str, timer_id: str, now: datetime | None = None
    ) -> float:
        now = now or datetime.now()
        # Determine the reset boundary for today
        p = self._profiles.get(profile_id)
        reset_hour = 0
        if p:
            timer = p.get_timer(timer_id)
            if timer:
                reset_hour = timer.reset_hour
        # If before reset_hour, use yesterday's key
        if now.hour < reset_hour:
            from datetime import timedelta
            day = (now.date() - timedelta(days=1)).isoformat()
        else:
            day = now.date().isoformat()
        rec_key = f"{profile_id}:{day}:{timer_id}"
        rec = self._usage.get(rec_key)
        return rec.minutes_used if rec else 0.0

    # ------------------------------------------------------------------
    # Override management
    # ------------------------------------------------------------------

    def grant_override(
        self,
        profile_id:    str,
        device_id:     str,
        duration_mins: int,
        pin:           str = "",
        granted_by:    str = "",
        reason:        str = "",
    ) -> OverrideSession | None:
        """
        Grant a temporary bypass.  If the profile has an override PIN,
        it must be supplied correctly.
        """
        p = self._profiles.get(profile_id)
        if not p:
            return None
        if p.override_pin_hash and not p.check_pin(pin):
            log.warning("Override PIN mismatch for profile %s", profile_id)
            return None
        override = OverrideSession(
            override_id = secrets.token_hex(8),
            profile_id  = profile_id,
            device_id   = device_id,
            expires_at  = time.time() + duration_mins * 60,
            granted_by  = granted_by,
            reason      = reason,
        )
        self._overrides[override.override_id] = override
        log.info("Override granted for %s on %s for %dm", profile_id, device_id, duration_mins)
        return override

    def revoke_override(self, override_id: str) -> bool:
        if override_id in self._overrides:
            del self._overrides[override_id]
            return True
        return False

    def list_overrides(self, profile_id: str | None = None) -> list[dict[str, Any]]:
        now = time.time()
        # Prune expired
        expired = [oid for oid, o in self._overrides.items() if o.expires_at < now - 60]
        for oid in expired:
            del self._overrides[oid]
        return [
            o.to_dict() for o in self._overrides.values()
            if profile_id is None or o.profile_id == profile_id
        ]

    def _active_override(self, profile_id: str, device_id: str) -> OverrideSession | None:
        now = time.time()
        for o in self._overrides.values():
            if (o.profile_id == profile_id
                    and (not o.device_id or o.device_id == device_id)
                    and o.expires_at > now):
                return o
        return None

    # ------------------------------------------------------------------
    # Enforcement state
    # ------------------------------------------------------------------

    def get_enforcement_state(
        self, device_id: str, now: datetime | None = None
    ) -> LockdownConfig:
        """
        Build the current LockdownConfig for a device.

        Called by the agent polling endpoint and by the enforcement loop
        before pushing to agents.
        """
        now = now or datetime.now()
        profile = self.get_profile_for_device(device_id)
        if not profile:
            return LockdownConfig.off()

        override = self._active_override(profile.profile_id, device_id)
        schedule_ok = profile.is_in_schedule(now)

        # Collect timer states
        timers: dict[str, int] = {}
        warnings: dict[str, int] = {}
        for t in profile.timers:
            if not t.is_active_today(now):
                continue
            used = self._get_usage_minutes(profile.profile_id, t.timer_id, now)
            remaining = max(0, t.daily_minutes - int(used))
            timers[t.timer_id]   = remaining
            warnings[t.timer_id] = t.warning_minutes

        # Build whitelist / blacklist from rules
        whitelist = [r.pattern for r in profile.rules if r.action == AppAction.ALLOW]
        blacklist = [r.pattern for r in profile.rules if r.action == AppAction.BLOCK]

        return LockdownConfig(
            profile_id      = profile.profile_id,
            policy_mode     = profile.policy_mode.value,
            whitelist       = whitelist,
            blacklist       = blacklist,
            kiosk_app       = profile.kiosk_app,
            schedule_locked = not schedule_ok and not override,
            override_active = override is not None,
            override_until  = override.expires_at if override else 0.0,
            timers          = timers,
            timer_warnings  = warnings,
            break_policy    = profile.break_policy.to_dict(),
            content_filter  = profile.content_filter.value,
        )

    # ------------------------------------------------------------------
    # Enforcement loop
    # ------------------------------------------------------------------

    async def _enforcement_loop(self) -> None:
        """
        Periodically push updated LockdownConfig to agents when state changes.
        """
        import asyncio
        _last_states: dict[str, str] = {}   # device_id → JSON fingerprint

        while True:
            try:
                await asyncio.sleep(self._PUSH_INTERVAL)
                for profile in list(self._profiles.values()):
                    for device_id in profile.device_ids:
                        cfg = self.get_enforcement_state(device_id)
                        fingerprint = json.dumps(cfg.to_dict(), sort_keys=True)
                        if _last_states.get(device_id) != fingerprint:
                            _last_states[device_id] = fingerprint
                            await self._push_to_agent(device_id, cfg)
            except Exception:
                log.exception("Parental enforcement loop error")

    async def _push_to_agent(self, device_id: str, cfg: LockdownConfig) -> None:
        """POST the LockdownConfig to the agent running on device_id."""
        if not self._agent_client:
            return
        try:
            await self._agent_client.push_lockdown(device_id, cfg.to_dict())
            log.debug("Pushed lockdown config to %s (mode=%s)", device_id, cfg.policy_mode)
        except Exception as e:
            log.debug("Failed to push lockdown to %s: %s", device_id, e)

    # ------------------------------------------------------------------
    # Persistence
    # ------------------------------------------------------------------

    def _save_profiles(self) -> None:
        self._data_dir.mkdir(parents=True, exist_ok=True)
        p = self._data_dir / "profiles.json"
        tmp = p.with_suffix(".tmp")
        tmp.write_text(json.dumps(
            {pid: prof.to_dict() for pid, prof in self._profiles.items()}, indent=2
        ))
        tmp.chmod(0o600)
        tmp.rename(p)

    def _save_usage(self) -> None:
        self._data_dir.mkdir(parents=True, exist_ok=True)
        p = self._data_dir / "usage.json"
        tmp = p.with_suffix(".tmp")
        tmp.write_text(json.dumps(
            {k: r.to_dict() for k, r in self._usage.items()}, indent=2
        ))
        tmp.rename(p)

    def _load(self) -> None:
        profiles_path = self._data_dir / "profiles.json"
        if profiles_path.exists():
            try:
                data = json.loads(profiles_path.read_text())
                for pid, d in data.items():
                    self._profiles[pid] = ChildProfile.from_dict(d)
            except Exception:
                log.exception("Failed to load child profiles")

        usage_path = self._data_dir / "usage.json"
        if usage_path.exists():
            try:
                data = json.loads(usage_path.read_text())
                for k, d in data.items():
                    self._usage[k] = UsageRecord.from_dict(d)
            except Exception:
                log.exception("Failed to load parental usage records")

    # ------------------------------------------------------------------
    # Prune old usage records
    # ------------------------------------------------------------------

    def prune_usage(self, keep_days: int = 90) -> int:
        """Remove usage records older than keep_days days. Returns count removed."""
        cutoff = date.fromtimestamp(time.time() - keep_days * 86400).isoformat()
        old_keys = [k for k, r in self._usage.items() if r.date < cutoff]
        for k in old_keys:
            del self._usage[k]
        if old_keys:
            self._save_usage()
        return len(old_keys)
