# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Business continuity failover manager.

When a controller goes offline (power, internet, hardware failure), Ozma
Connect detects the outage and automatically prepares a virtual controller
from the last backup. The user gets a single notification:

    "Your controller at Home Office has been offline for 15 minutes.
     We have a backup ready from today at 14:32.
     [Recover virtual system] [Not now]"

One tap restores everything. The same subdomain, same nodes, same config.
The local controller gets a window of free failover time based on their
tier (Business: 3 days, Business Pro: 7 days, Enterprise: 30 days).
When the local hardware comes back, state syncs automatically and the
virtual controller hands off cleanly.

Modes:
  local     — normal operation on physical hardware
  virtual   — running in Connect cloud as a failover controller
  recovery  — local came back; syncing state from virtual before handoff

The virtual controller is launched by the Connect cloud-controller service
(private repo) using the last ZK-encrypted backup. Because we restore the
same mesh CA, all node certificates remain valid and nodes reconnect
automatically when Connect updates the relay routing.

Environment:
  OZMA_FAILOVER_MODE   — "local" (default) | "virtual"
  OZMA_FAILOVER_ORIGIN — controller_id of the local controller this
                          virtual is standing in for (set by cloud-controller)
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
import time
from dataclasses import dataclass, field
from enum import Enum
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.failover")

# How long Connect waits before offering a virtual controller.
# Enough to rule out routine restarts.
_OUTAGE_GRACE_MINUTES = 15

# Free failover days by Connect tier
FREE_DAYS_BY_TIER = {
    "free": 0,
    "business": 3,
    "business_pro": 7,
    "enterprise": 30,
}


class FailoverMode(str, Enum):
    LOCAL = "local"
    VIRTUAL = "virtual"
    RECOVERY = "recovery"      # local came back, syncing from virtual


# ---------------------------------------------------------------------------
# Data models
# ---------------------------------------------------------------------------

@dataclass
class FailoverConfig:
    # How long Connect waits before offering virtual recovery (minutes)
    grace_period_minutes: int = _OUTAGE_GRACE_MINUTES
    # Heartbeat interval in seconds
    heartbeat_interval: int = 60
    # How often to poll Connect for failover status changes
    poll_interval: int = 30
    # Free days for this account's tier (populated by Connect on auth)
    free_days: int = 0
    tier: str = "free"

    def to_dict(self) -> dict:
        return {
            "grace_period_minutes": self.grace_period_minutes,
            "heartbeat_interval": self.heartbeat_interval,
            "poll_interval": self.poll_interval,
            "free_days": self.free_days,
            "tier": self.tier,
        }

    @classmethod
    def from_dict(cls, d: dict) -> FailoverConfig:
        return cls(
            grace_period_minutes=d.get("grace_period_minutes", _OUTAGE_GRACE_MINUTES),
            heartbeat_interval=d.get("heartbeat_interval", 60),
            poll_interval=d.get("poll_interval", 30),
            free_days=d.get("free_days", 0),
            tier=d.get("tier", "free"),
        )


@dataclass
class FailoverStatus:
    mode: FailoverMode = FailoverMode.LOCAL
    # Timestamp when Connect first detected the outage
    outage_detected_at: float | None = None
    # Timestamp when virtual controller became active
    failover_started_at: float | None = None
    # Timestamp local controller came back
    recovery_started_at: float | None = None
    # Timestamp of the backup the virtual was restored from
    backup_timestamp: float | None = None
    # Backup timestamp in human-readable form (set by Connect)
    backup_label: str | None = None
    # How many free days remain (0 when expired or tier=free)
    free_days_remaining: float = 0.0
    # Paid extension expiry (None if no extension purchased)
    paid_until: float | None = None
    # Whether the user has accepted the virtual controller offer
    offer_accepted: bool = False
    # Whether the user declined the offer
    offer_declined: bool = False
    # Virtual controller URL (set by Connect on activation)
    virtual_url: str | None = None
    # The controller_id we're standing in for (virtual mode only)
    origin_controller_id: str | None = None

    def to_dict(self) -> dict:
        return {
            "mode": self.mode.value,
            "outage_detected_at": self.outage_detected_at,
            "failover_started_at": self.failover_started_at,
            "recovery_started_at": self.recovery_started_at,
            "backup_timestamp": self.backup_timestamp,
            "backup_label": self.backup_label,
            "free_days_remaining": self.free_days_remaining,
            "paid_until": self.paid_until,
            "offer_accepted": self.offer_accepted,
            "offer_declined": self.offer_declined,
            "virtual_url": self.virtual_url,
            "origin_controller_id": self.origin_controller_id,
        }


# ---------------------------------------------------------------------------
# Manager
# ---------------------------------------------------------------------------

class FailoverManager:
    STATE_PATH = Path("/var/lib/ozma/failover_state.json")

    def __init__(
        self,
        connect,                        # OzmaConnect instance
        state: Any = None,              # AppState (for events)
        scenarios=None,                 # ScenarioManager (for state export)
        state_path: Path | None = None,
    ) -> None:
        self._connect = connect
        self._app_state = state
        self._scenarios = scenarios
        self._state_path = state_path or self.STATE_PATH

        # Detect if we're running as a virtual controller
        mode_env = os.environ.get("OZMA_FAILOVER_MODE", "local").lower()
        self._mode = FailoverMode(mode_env) if mode_env in FailoverMode._value2member_map_ else FailoverMode.LOCAL
        self._origin_id = os.environ.get("OZMA_FAILOVER_ORIGIN", "")

        self._config = FailoverConfig()
        self._status = FailoverStatus(
            mode=self._mode,
            origin_controller_id=self._origin_id or None,
        )

        self._heartbeat_task: asyncio.Task | None = None
        self._poll_task: asyncio.Task | None = None
        self._sync_task: asyncio.Task | None = None

        self._load()

    # ------------------------------------------------------------------
    # Lifecycle
    # ------------------------------------------------------------------

    async def start(self) -> None:
        if not self._connect.authenticated:
            log.info("failover: Connect not authenticated, monitoring disabled")
            return

        # Update config from tier
        tier = self._connect.tier
        self._config.tier = tier
        self._config.free_days = FREE_DAYS_BY_TIER.get(tier, 0)

        # Always send heartbeats (so Connect can detect outages)
        self._heartbeat_task = asyncio.create_task(
            self._heartbeat_loop(), name="failover.heartbeat"
        )

        # Poll for status changes
        self._poll_task = asyncio.create_task(
            self._poll_loop(), name="failover.poll"
        )

        mode_str = self._mode.value
        log.info("failover: started (mode=%s, tier=%s, free_days=%d)",
                 mode_str, tier, self._config.free_days)

        if self._mode == FailoverMode.VIRTUAL:
            log.info("failover: running as VIRTUAL controller for %s",
                     self._origin_id or "unknown")
            self._emit("failover.virtual_active", {
                "origin_controller_id": self._origin_id,
                "backup_label": self._status.backup_label,
                "free_days_remaining": self._status.free_days_remaining,
            })

    async def stop(self) -> None:
        for task in (self._heartbeat_task, self._poll_task, self._sync_task):
            if task and not task.done():
                task.cancel()
                try:
                    await task
                except asyncio.CancelledError:
                    pass
        self._heartbeat_task = None
        self._poll_task = None
        self._sync_task = None
        log.info("failover: stopped")

    # ------------------------------------------------------------------
    # Status / config
    # ------------------------------------------------------------------

    def get_status(self) -> dict:
        return {
            **self._status.to_dict(),
            "config": self._config.to_dict(),
            "outage_duration_seconds": self._outage_duration(),
            "failover_duration_seconds": self._failover_duration(),
        }

    def get_mode(self) -> FailoverMode:
        return self._mode

    def is_virtual(self) -> bool:
        return self._mode == FailoverMode.VIRTUAL

    def _outage_duration(self) -> float | None:
        if not self._status.outage_detected_at:
            return None
        end = self._status.recovery_started_at or time.time()
        return end - self._status.outage_detected_at

    def _failover_duration(self) -> float | None:
        if not self._status.failover_started_at:
            return None
        end = self._status.recovery_started_at or time.time()
        return end - self._status.failover_started_at

    # ------------------------------------------------------------------
    # User actions (from API)
    # ------------------------------------------------------------------

    async def accept_virtual_controller(self) -> dict:
        """User tapped 'Recover virtual system'. Activate it."""
        if self._mode != FailoverMode.LOCAL:
            return {"ok": False, "error": "Only valid on local controller"}
        result = await self._connect.accept_failover()
        if result:
            self._status.offer_accepted = True
            self._status.virtual_url = result.get("virtual_url")
            self._status.failover_started_at = time.time()
            self._save()
            self._emit("failover.accepted", {
                "virtual_url": self._status.virtual_url,
                "free_days": self._config.free_days,
            })
            log.info("failover: virtual controller accepted → %s", self._status.virtual_url)
        return result or {"ok": False, "error": "Connect request failed"}

    async def decline_virtual_controller(self) -> dict:
        """User tapped 'Not now'."""
        self._status.offer_declined = True
        self._save()
        result = await self._connect.decline_failover()
        self._emit("failover.declined", {})
        return result or {"ok": True}

    async def extend_failover(self, days: int) -> dict:
        """Purchase additional days of virtual failover."""
        result = await self._connect.extend_failover(days)
        if result and result.get("ok"):
            self._status.paid_until = result.get("paid_until")
            self._save()
            self._emit("failover.extended", {
                "days": days,
                "paid_until": self._status.paid_until,
            })
        return result or {"ok": False, "error": "Connect request failed"}

    # ------------------------------------------------------------------
    # State sync (virtual → local handoff)
    # ------------------------------------------------------------------

    async def export_state_delta(self) -> dict:
        """Export changed state for sync to local controller.

        Called by the virtual controller when local comes back.
        """
        delta: dict[str, Any] = {
            "exported_at": time.time(),
            "failover_started_at": self._status.failover_started_at,
        }

        # Scenarios changed during failover
        if self._scenarios:
            try:
                delta["scenarios"] = self._scenarios.list_scenarios()
            except Exception as exc:
                log.warning("failover: could not export scenarios: %s", exc)

        # Node state that changed
        if self._app_state:
            try:
                nodes = {}
                for node_id, node in getattr(self._app_state, "nodes", {}).items():
                    nodes[node_id] = {
                        "last_seen": getattr(node, "last_seen", None),
                        "active": getattr(self._app_state, "active_node_id", None) == node_id,
                    }
                delta["nodes"] = nodes
            except Exception as exc:
                log.warning("failover: could not export node state: %s", exc)

        return delta

    async def apply_state_delta(self, delta: dict) -> bool:
        """Apply a state delta received from the virtual controller.

        Called on the local controller when it comes back online and
        pulls the delta from Connect.
        """
        log.info("failover: applying state delta (exported_at=%s)",
                 delta.get("exported_at"))

        # Merge scenarios
        if "scenarios" in delta and self._scenarios:
            try:
                for s in delta["scenarios"]:
                    self._scenarios.upsert_scenario_from_dict(s)
                log.info("failover: merged %d scenarios from virtual", len(delta["scenarios"]))
            except Exception as exc:
                log.warning("failover: scenario merge failed: %s", exc)

        self._emit("failover.state_applied", {
            "exported_at": delta.get("exported_at"),
            "failover_duration": self._failover_duration(),
        })
        return True

    async def _do_handoff(self) -> None:
        """Full handoff sequence: virtual exports state, signals Connect, shuts down."""
        log.info("failover: initiating handoff — local controller is back")
        self._status.recovery_started_at = time.time()
        self._status.mode = FailoverMode.RECOVERY
        self._mode = FailoverMode.RECOVERY
        self._save()

        self._emit("failover.recovery_started", {
            "origin_controller_id": self._origin_id,
        })

        # Export and push delta to Connect
        delta = await self.export_state_delta()
        try:
            await self._connect.push_sync_delta(delta)
            log.info("failover: state delta pushed to Connect (%d keys)", len(delta))
        except Exception as exc:
            log.error("failover: failed to push delta: %s", exc)

        # Signal Connect that handoff is complete
        try:
            await self._connect.signal_handoff_complete(
                origin_controller_id=self._origin_id
            )
        except Exception as exc:
            log.error("failover: handoff signal failed: %s", exc)

        self._emit("failover.handoff_complete", {
            "failover_duration_seconds": self._failover_duration(),
        })
        log.info("failover: handoff complete — virtual controller shutting down")

        # Give the local controller time to pull the delta and apply it,
        # then exit. The cloud-controller service will stop the container.
        await asyncio.sleep(10)
        os._exit(0)

    # ------------------------------------------------------------------
    # Internal loops
    # ------------------------------------------------------------------

    async def _heartbeat_loop(self) -> None:
        """Send heartbeats to Connect so it can detect outages."""
        while True:
            try:
                await self._connect.send_failover_heartbeat(
                    mode=self._mode.value,
                    origin_controller_id=self._origin_id or None,
                )
            except Exception as exc:
                log.debug("failover: heartbeat failed: %s", exc)
            await asyncio.sleep(self._config.heartbeat_interval)

    async def _poll_loop(self) -> None:
        """Poll Connect for failover status changes."""
        while True:
            await asyncio.sleep(self._config.poll_interval)
            try:
                await self._check_connect_status()
            except Exception as exc:
                log.debug("failover: poll failed: %s", exc)

    async def _check_connect_status(self) -> None:
        status = await self._connect.check_failover_status()
        if not status:
            return

        connect_state = status.get("state")

        # LOCAL mode: track whether Connect has detected an outage and is
        # offering a virtual controller
        if self._mode == FailoverMode.LOCAL:
            if connect_state == "outage_detected":
                if not self._status.outage_detected_at:
                    self._status.outage_detected_at = status.get("outage_detected_at", time.time())
                    self._save()
                    log.warning("failover: Connect reports we were detected as offline at %s",
                                self._status.outage_detected_at)

            elif connect_state == "failover_pending":
                # Connect has prepared a virtual controller — update status
                self._status.backup_label = status.get("backup_label")
                self._status.backup_timestamp = status.get("backup_timestamp")
                self._status.free_days_remaining = status.get("free_days_remaining", 0)
                self._save()
                # Fire event so the dashboard can show the offer banner
                self._emit("failover.offer_available", {
                    "backup_label": self._status.backup_label,
                    "free_days": self._config.free_days,
                    "outage_duration_minutes": round(
                        (time.time() - (self._status.outage_detected_at or time.time())) / 60, 1
                    ),
                    "accept_url": status.get("accept_url"),
                })

            elif connect_state == "recovery_ready":
                # Local came back while virtual is running — pull and apply delta
                if self._mode == FailoverMode.LOCAL:
                    log.info("failover: virtual controller detected our return, pulling delta")
                    self._status.recovery_started_at = time.time()
                    self._status.mode = FailoverMode.RECOVERY
                    self._mode = FailoverMode.RECOVERY
                    self._save()

                    self._sync_task = asyncio.create_task(
                        self._apply_remote_delta(), name="failover.apply_delta"
                    )

        # VIRTUAL mode: watch for the local controller coming back
        elif self._mode == FailoverMode.VIRTUAL:
            if connect_state in ("local_recovered", "local_heartbeat_resumed"):
                log.info("failover: local controller is back — initiating handoff")
                if not self._sync_task or self._sync_task.done():
                    self._sync_task = asyncio.create_task(
                        self._do_handoff(), name="failover.handoff"
                    )

            # Keep free_days_remaining up to date for the dashboard
            if "free_days_remaining" in status:
                self._status.free_days_remaining = status["free_days_remaining"]
                self._save()

    async def _apply_remote_delta(self) -> None:
        """Pull state delta from Connect and apply it."""
        try:
            delta = await self._connect.pull_sync_delta()
            if delta:
                ok = await self.apply_state_delta(delta)
                if ok:
                    await self._connect.signal_handoff_complete(
                        origin_controller_id=None  # we ARE the local
                    )
                    self._status.mode = FailoverMode.LOCAL
                    self._mode = FailoverMode.LOCAL
                    self._save()
                    self._emit("failover.recovery_complete", {
                        "failover_duration_seconds": self._failover_duration(),
                    })
                    log.info("failover: recovery complete — local controller fully restored")
            else:
                log.warning("failover: no delta available from Connect")
        except Exception as exc:
            log.exception("failover: apply_remote_delta failed: %s", exc)

    # ------------------------------------------------------------------
    # Helpers
    # ------------------------------------------------------------------

    def _emit(self, event_type: str, data: dict) -> None:
        if self._app_state and hasattr(self._app_state, "events"):
            try:
                self._app_state.events.put_nowait({"type": event_type, **data})
            except Exception:
                pass

    def _save(self) -> None:
        state = {
            "config": self._config.to_dict(),
            "status": self._status.to_dict(),
        }
        self._state_path.parent.mkdir(parents=True, exist_ok=True)
        tmp = self._state_path.with_suffix(".tmp")
        import os as _os
        tmp.write_text(json.dumps(state, indent=2))
        _os.chmod(tmp, 0o600)
        tmp.rename(self._state_path)

    def _load(self) -> None:
        if not self._state_path.exists():
            return
        try:
            state = json.loads(self._state_path.read_text())
            self._config = FailoverConfig.from_dict(state.get("config", {}))
            s = state.get("status", {})
            self._status = FailoverStatus(
                mode=FailoverMode(s.get("mode", "local")),
                outage_detected_at=s.get("outage_detected_at"),
                failover_started_at=s.get("failover_started_at"),
                recovery_started_at=s.get("recovery_started_at"),
                backup_timestamp=s.get("backup_timestamp"),
                backup_label=s.get("backup_label"),
                free_days_remaining=s.get("free_days_remaining", 0.0),
                paid_until=s.get("paid_until"),
                offer_accepted=s.get("offer_accepted", False),
                offer_declined=s.get("offer_declined", False),
                virtual_url=s.get("virtual_url"),
                origin_controller_id=s.get("origin_controller_id"),
            )
        except Exception as exc:
            log.error("failover: failed to load state: %s", exc)
