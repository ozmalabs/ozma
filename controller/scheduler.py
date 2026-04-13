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
    trigger_type: str = "time"  # "time" or "calendar"
    calendar_query: str = ""    # For calendar-based triggers

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
        return {
            "time": self.time, 
            "days": self.days, 
            "scenario": self.scenario, 
            "enabled": self.enabled,
            "trigger_type": self.trigger_type,
            "calendar_query": self.calendar_query
        }


class Scheduler:
    """Cron-like scheduler for automatic scenario switching."""

    def __init__(self, scenarios: Any, calendar_reader: Any = None) -> None:
        self._scenarios = scenarios
        self._calendar_reader = calendar_reader
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

    def add_rule(self, time: str, days: str, scenario: str, trigger_type: str = "time", calendar_query: str = "") -> dict[str, Any]:
        rule = ScheduleRule(
            time=time, 
            days=days, 
            scenario=scenario,
            trigger_type=trigger_type,
            calendar_query=calendar_query
        )
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
                        should_trigger = False
                        
                        # Time-based trigger
                        if rule.trigger_type == "time" and rule.matches_now():
                            should_trigger = True
                            
                        # Calendar-based trigger
                        elif rule.trigger_type == "calendar" and self._calendar_reader and rule.calendar_query:
                            try:
                                context = await self._calendar_reader.get_context(rule.calendar_query)
                                # Simple heuristic: if context contains relevant keywords, trigger
                                if "meeting" in context.lower() or "event" in context.lower():
                                    should_trigger = True
                            except Exception as e:
                                log.warning("Failed to check calendar trigger: %s", e)
                        
                        if should_trigger:
                            self._last_fired = now_key
                            log.info("Schedule trigger: %s → scenario %s", rule.time or rule.calendar_query, rule.scenario)
                            try:
                                await self._scenarios.activate(rule.scenario)
                            except KeyError:
                                log.warning("Scheduled scenario not found: %s", rule.scenario)
                            except Exception as e:
                                log.error("Failed to activate scheduled scenario %s: %s", rule.scenario, e)
                            break  # Only fire one rule per minute
                await asyncio.sleep(15)  # Check 4× per minute
            except asyncio.CancelledError:
                return
            except Exception as e:
                log.error("Scheduler loop error: %s", e)
                await asyncio.sleep(15)

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
