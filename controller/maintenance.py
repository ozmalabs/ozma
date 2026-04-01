# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Scheduled maintenance — automated maintenance windows with orchestration.

Combines scheduler + fleet manager + automation + notifications for
unattended maintenance operations.

Example:
  "Every Sunday 2am: run firmware update on all Enterprise Nodes.
   If any fail, create Jira ticket. Send summary to Slack at 6am."

  "First Monday of month: run compliance scan on all production servers.
   Generate report. Email to compliance team."

  "Daily 3am: clean up replay buffer and old recordings on all nodes.
   Report storage freed."
"""

from __future__ import annotations

import asyncio
import json
import logging
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.maintenance")


@dataclass
class MaintenanceWindow:
    """A scheduled maintenance operation."""
    id: str
    name: str
    schedule: str              # Cron-like: "0 2 * * 0" (Sun 2am)
    node_ids: list[str] = field(default_factory=list)  # Empty = all nodes
    script: str = ""           # Automation script to run
    job_type: str = ""         # Or a fleet_manager job type (burn_in, compliance_scan)
    notify_on_complete: bool = True
    notify_on_failure: bool = True
    create_ticket_on_failure: bool = True
    enabled: bool = True
    last_run: float = 0.0
    last_result: str = ""

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id, "name": self.name, "schedule": self.schedule,
            "nodes": self.node_ids, "enabled": self.enabled,
            "last_run": self.last_run, "last_result": self.last_result,
        }


class MaintenanceManager:
    """
    Orchestrates scheduled maintenance windows.

    Evaluates maintenance schedules, runs operations during windows,
    handles failure recovery, and sends notifications.
    """

    def __init__(self, fleet: Any = None, automation: Any = None,
                 notifier: Any = None, audit: Any = None) -> None:
        self._fleet = fleet
        self._automation = automation
        self._notifier = notifier
        self._audit = audit
        self._windows: dict[str, MaintenanceWindow] = {}
        self._task: asyncio.Task | None = None

    async def start(self) -> None:
        self._task = asyncio.create_task(self._check_loop(), name="maintenance")
        log.info("Maintenance manager started (%d windows)", len(self._windows))

    async def stop(self) -> None:
        if self._task:
            self._task.cancel()

    def add_window(self, window: MaintenanceWindow) -> None:
        self._windows[window.id] = window

    def list_windows(self) -> list[dict]:
        return [w.to_dict() for w in self._windows.values()]

    async def run_now(self, window_id: str) -> dict:
        """Manually trigger a maintenance window."""
        window = self._windows.get(window_id)
        if not window:
            return {"ok": False, "error": "Window not found"}
        return await self._execute_window(window)

    async def _check_loop(self) -> None:
        """Check maintenance schedules every minute."""
        while True:
            try:
                # Simplified schedule check (real impl would parse cron expressions)
                for window in self._windows.values():
                    if not window.enabled:
                        continue
                    # Check if enough time has passed since last run
                    if time.time() - window.last_run < 3600:  # Min 1 hour between runs
                        continue
                    # Would check cron expression here
                await asyncio.sleep(60)
            except asyncio.CancelledError:
                return

    async def _execute_window(self, window: MaintenanceWindow) -> dict:
        """Execute a maintenance window."""
        log.info("Maintenance window starting: %s", window.name)
        window.last_run = time.time()
        results = {"ok": True, "errors": []}

        if self._audit:
            self._audit.log_event("maintenance", "controller", {
                "window": window.id, "action": "start",
            })

        try:
            if window.script and self._automation:
                # Run automation script on each node
                for node_id in (window.node_ids or ["all"]):
                    result = await self._automation.run_script(
                        window.script, node_id=node_id if node_id != "all" else None,
                    )
                    if not result.get("ok"):
                        results["errors"].extend(result.get("errors", []))

            elif window.job_type and self._fleet:
                # Run a fleet management job
                pass  # Would call fleet_manager methods

            window.last_result = "success" if not results["errors"] else "partial_failure"

        except Exception as e:
            window.last_result = "failed"
            results["ok"] = False
            results["errors"].append(str(e))

        # Notifications
        if results["errors"] and window.notify_on_failure and self._notifier:
            await self._notifier.on_event("maintenance.failure", {
                "window": window.name,
                "errors": results["errors"][:5],
            })
        elif window.notify_on_complete and self._notifier:
            await self._notifier.on_event("maintenance.complete", {
                "window": window.name,
                "result": window.last_result,
            })

        return results
