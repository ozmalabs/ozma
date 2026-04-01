# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Scheduled scenario switching — cron-like time triggers.

Rules are evaluated every minute.  When a rule matches, the associated
scenario is activated automatically.

Schedule format (in schedule.json):
  [
    {"time": "09:00", "days": "mon,tue,wed,thu,fri", "scenario": "work"},
    {"time": "18:00", "days": "mon,tue,wed,thu,fri", "scenario": "media"},
    {"time": "22:00", "days": "*", "scenario": "night"}
  ]

API:
  GET  /api/v1/schedule          — list rules
  POST /api/v1/schedule          — add a rule
  DELETE /api/v1/schedule/{index} — remove a rule
"""

from __future__ import annotations

import asyncio
import json
import logging
import time
from dataclasses import dataclass
from datetime import datetime
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.scheduler")

_DAY_MAP = {"mon": 0, "tue": 1, "wed": 2, "thu": 3, "fri": 4, "sat": 5, "sun": 6}


@dataclass
class ScheduleRule:
    time: str          # "HH:MM"
    days: str          # "mon,tue,wed" or "*" for every day
    scenario: str      # scenario ID to activate
    enabled: bool = True

    def matches_now(self) -> bool:
        if not self.enabled:
            return False
        now = datetime.now()
        # Check time (match to the minute)
        if now.strftime("%H:%M") != self.time:
            return False
        # Check day
        if self.days == "*":
            return True
        day_names = [d.strip().lower()[:3] for d in self.days.split(",")]
        return now.weekday() in [_DAY_MAP.get(d, -1) for d in day_names]

    def to_dict(self) -> dict[str, Any]:
        return {"time": self.time, "days": self.days, "scenario": self.scenario, "enabled": self.enabled}


class Scheduler:
    """Cron-like scheduler for automatic scenario switching."""

    def __init__(self, scenarios: Any) -> None:
        self._scenarios = scenarios
        self._rules: list[ScheduleRule] = []
        self._task: asyncio.Task | None = None
        self._path = Path(__file__).parent / "schedule.json"
        self._last_fired: str = ""  # prevent double-firing in the same minute
        self._load()

    async def start(self) -> None:
        self._task = asyncio.create_task(self._loop(), name="scheduler")
        log.info("Scheduler started with %d rule(s)", len(self._rules))

    async def stop(self) -> None:
        if self._task:
            self._task.cancel()

    def list_rules(self) -> list[dict[str, Any]]:
        return [r.to_dict() for i, r in enumerate(self._rules)]

    def add_rule(self, time: str, days: str, scenario: str) -> dict[str, Any]:
        rule = ScheduleRule(time=time, days=days, scenario=scenario)
        self._rules.append(rule)
        self._save()
        return rule.to_dict()

    def remove_rule(self, index: int) -> bool:
        if 0 <= index < len(self._rules):
            self._rules.pop(index)
            self._save()
            return True
        return False

    async def _loop(self) -> None:
        while True:
            try:
                now_key = datetime.now().strftime("%Y-%m-%d %H:%M")
                if now_key != self._last_fired:
                    for rule in self._rules:
                        if rule.matches_now():
                            self._last_fired = now_key
                            log.info("Schedule trigger: %s → scenario %s", rule.time, rule.scenario)
                            try:
                                await self._scenarios.activate(rule.scenario)
                            except KeyError:
                                log.warning("Scheduled scenario not found: %s", rule.scenario)
                            break  # Only fire one rule per minute
                await asyncio.sleep(15)  # Check 4× per minute
            except asyncio.CancelledError:
                return

    def _save(self) -> None:
        try:
            self._path.write_text(json.dumps([r.to_dict() for r in self._rules], indent=2))
        except Exception as e:
            log.warning("Failed to save schedule: %s", e)

    def _load(self) -> None:
        if not self._path.exists():
            return
        try:
            data = json.loads(self._path.read_text())
            self._rules = [ScheduleRule(**r) for r in data]
        except Exception as e:
            log.warning("Failed to load schedule: %s", e)
