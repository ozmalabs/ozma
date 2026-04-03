# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Alert / Alarm / Timer system — presence-aware, time-limited attention requests.

All "something needs the user's attention right now" events share this infrastructure:

  Kind        Title example           Primary action   Typical timeout
  ─────────   ─────────────────────   ──────────────   ───────────────
  doorbell    Doorbell                Answer           30 s
  timer       15 min timer            OK               —
  alarm       Kitchen smoke alarm     Acknowledge      until ack
  motion      Motion at back garden   Dismiss          60 s
  reminder    Meeting in 5 min        OK               300 s

Delivery (alert.created → all channels simultaneously):
  - WebSocket broadcast → OzmaConsole overlay on every connected client
    (covers the desk, the lounge TV, the tablet in the kitchen)
  - KDE Connect ping → paired phone(s)
  - NotificationManager → configured webhooks / Slack / Discord / email

Event flow:
  Source (DoorbellManager, TimerManager, OCRTrigger, …)
    → AlertManager.create()
      → alert.created event on state.events
        → _broadcast() → WebSocket clients (overlay + chime)
      → _notify() → KDE Connect + NotificationManager in parallel
  User action (click, Stream Deck, MIDI, hotkey, …)
    → POST /api/v1/controls/action  { action: "alert.acknowledge", value: <id> }
      → ControlManager._execute_action() → AlertManager.acknowledge()
        → alert.acknowledged event → clients hide overlay
"""

from __future__ import annotations

import asyncio
import logging
import time
import uuid
from dataclasses import dataclass, field
from typing import Any

log = logging.getLogger("ozma.alerts")

ALERT_TTL_S = 300   # clean up resolved alerts after 5 minutes

# Backwards-compat / kind-specific event aliases.
# When an alert of the given kind transitions, we also fire the aliased event
# so that pre-existing dashboard and agent code that listens for
# "doorbell.ringing" / "doorbell.answered" etc. continues to work.
_KIND_EVENT_ALIAS: dict[tuple[str, str], str] = {
    ("doorbell", "alert.created"):      "doorbell.ringing",
    ("doorbell", "alert.acknowledged"): "doorbell.answered",
    ("doorbell", "alert.dismissed"):    "doorbell.dismissed",
    ("doorbell", "alert.expired"):      "doorbell.expired",
    ("doorbell", "alert.updated"):      "doorbell.updated",
}


@dataclass
class AlertSession:
    """A single attention request with a lifecycle.

    Kinds and their conventions:
      doorbell  — camera snapshot available; primary="Answer", secondary="Dismiss"
      timer     — countdown already done; primary="OK", no secondary needed
      alarm     — urgent; no auto-expire (timeout_s=0); primary="Acknowledge"
      motion    — camera snapshot optional; primary="Dismiss"
      reminder  — soft; primary="OK"
    """

    id: str
    kind: str               # "doorbell" | "timer" | "alarm" | "motion" | "reminder"
    title: str              # Short header shown in the overlay
    body: str               # One-line description
    started_at: float
    timeout_s: int = 30     # Auto-expire after this many seconds; 0 = never
    state: str = "active"   # active | acknowledged | dismissed | expired
    node_id: str | None = None   # machine this alert is relevant to (None = global)

    # Optional media / metadata
    snapshot_url: str = ""  # Camera snapshot proxied via /api/v1/alerts/{id}/snapshot
    camera: str = ""
    person: str = ""        # Recognised person name (if any)

    # Control button labels — override per kind
    primary_label: str = "OK"
    secondary_label: str = "Dismiss"

    def to_dict(self) -> dict[str, Any]:
        d: dict[str, Any] = {
            "id": self.id,
            "kind": self.kind,
            "title": self.title,
            "body": self.body,
            "started_at": self.started_at,
            "timeout_s": self.timeout_s,
            "state": self.state,
            "age_s": round(time.time() - self.started_at, 1),
            "primary_label": self.primary_label,
            "secondary_label": self.secondary_label,
        }
        if self.node_id:
            d["node_id"] = self.node_id
        if self.snapshot_url:
            d["snapshot_url"] = f"/api/v1/alerts/{self.id}/snapshot"
        if self.camera:
            d["camera"] = self.camera
        if self.person:
            d["person"] = self.person
        return d


class AlertManager:
    """Central registry for all attention requests.

    Sources (DoorbellManager, future TimerManager, OCRTrigger, …) call
    ``create()`` to raise an alert.  The manager handles delivery, expiry,
    and state transitions.  Control surfaces call ``acknowledge()`` /
    ``dismiss()`` via ControlManager._execute_action().
    """

    def __init__(
        self,
        state: Any,
        kdeconnect: Any = None,
        notifier: Any = None,
    ) -> None:
        self._state = state
        self._kdeconnect = kdeconnect
        self._notifier = notifier
        self._alerts: dict[str, AlertSession] = {}
        self._expire_task: asyncio.Task | None = None
        # Optional callbacks fired (as Tasks) when an alert is acknowledged.
        # Key: alert_id, Value: async callable(alert_id: str)
        self._ack_callbacks: dict[str, Any] = {}

    async def start(self) -> None:
        self._expire_task = asyncio.create_task(
            self._expire_loop(), name="alert-expire"
        )
        log.info("AlertManager started")

    async def stop(self) -> None:
        if self._expire_task:
            self._expire_task.cancel()

    # ── Public API ────────────────────────────────────────────────────────────

    async def create(
        self,
        kind: str,
        title: str,
        body: str,
        *,
        timeout_s: int = 30,
        node_id: str | None = None,
        snapshot_url: str = "",
        camera: str = "",
        person: str = "",
        primary_label: str = "OK",
        secondary_label: str = "Dismiss",
        debounce_key: str = "",  # if set, suppress if matching active alert < debounce_s old
        debounce_s: int = 5,
    ) -> AlertSession | None:
        """Create a new alert and begin delivery.

        Returns the new AlertSession, or None if suppressed by debounce.
        """
        if debounce_key:
            for a in self._alerts.values():
                # Debounce on kind + camera so doorbell and motion on the same
                # camera don't suppress each other — they're independent events.
                if (a.state == "active"
                        and a.kind == kind
                        and a.camera == camera
                        and time.time() - a.started_at < debounce_s):
                    log.debug("Alert debounced: kind=%s camera=%s", kind, camera)
                    return None

        alert = AlertSession(
            id=uuid.uuid4().hex[:8],
            kind=kind,
            title=title,
            body=body,
            started_at=time.time(),
            timeout_s=timeout_s,
            node_id=node_id,
            snapshot_url=snapshot_url,
            camera=camera,
            person=person,
            primary_label=primary_label,
            secondary_label=secondary_label,
        )
        self._alerts[alert.id] = alert
        log.info("Alert created: id=%s kind=%s title=%r", alert.id, kind, title)

        await self._push("alert.created", alert)
        await self._notify(alert)
        return alert

    def register_acknowledge_callback(self, alert_id: str, coro_fn: Any) -> None:
        """Register a coroutine function to call when alert_id is acknowledged.

        Called with the alert_id as the sole argument.  Used by DoorbellManager
        to start audio bridges on answer without polling the event queue.
        """
        self._ack_callbacks[alert_id] = coro_fn

    async def acknowledge(self, alert_id: str) -> bool:
        """Primary action (Answer / OK). Returns True if state changed."""
        alert = self._resolve(alert_id)
        if not alert or alert.state != "active":
            return False
        alert.state = "acknowledged"
        log.info("Alert acknowledged: id=%s kind=%s", alert.id, alert.kind)
        await self._push("alert.acknowledged", alert)
        cb = self._ack_callbacks.pop(alert.id, None)
        if cb:
            asyncio.create_task(cb(alert.id), name=f"alert-ack-cb-{alert.id}")
        return True

    async def dismiss(self, alert_id: str) -> bool:
        """Secondary action (Dismiss). Returns True if state changed."""
        alert = self._resolve(alert_id)
        if not alert or alert.state != "active":
            return False
        alert.state = "dismissed"
        log.info("Alert dismissed: id=%s kind=%s", alert.id, alert.kind)
        await self._push("alert.dismissed", alert)
        return True

    async def update(self, alert_id: str, **kwargs: Any) -> bool:
        """Enrich an active alert with additional metadata (e.g. person name).

        Pushes alert.updated so clients can refresh the overlay without hiding it.
        """
        alert = self._alerts.get(alert_id)
        if not alert or alert.state != "active":
            return False
        for k, v in kwargs.items():
            if hasattr(alert, k):
                setattr(alert, k, v)
        await self._push("alert.updated", alert)
        return True

    def get_alert(self, alert_id: str) -> AlertSession | None:
        return self._alerts.get(alert_id)

    def list_alerts(
        self,
        kind: str | None = None,
        state: str | None = None,
    ) -> list[dict[str, Any]]:
        alerts = self._alerts.values()
        if kind:
            alerts = (a for a in alerts if a.kind == kind)
        if state:
            alerts = (a for a in alerts if a.state == state)
        return [a.to_dict() for a in alerts]

    def get_most_recent_active(self, kind: str | None = None) -> AlertSession | None:
        """Return the most recently created active alert, optionally filtered by kind."""
        candidates = [
            a for a in self._alerts.values()
            if a.state == "active" and (kind is None or a.kind == kind)
        ]
        return max(candidates, key=lambda a: a.started_at, default=None)

    def get_snapshot_url(self, alert_id: str) -> str | None:
        a = self._alerts.get(alert_id)
        return a.snapshot_url if a else None

    # ── Internal ──────────────────────────────────────────────────────────────

    def _resolve(self, alert_id: str) -> AlertSession | None:
        """Resolve an alert_id; empty string → most recent active alert."""
        if not alert_id:
            return self.get_most_recent_active()
        return self._alerts.get(alert_id)

    async def _push(self, event_type: str, alert: AlertSession) -> None:
        payload = {"type": event_type, **alert.to_dict()}
        await self._state.events.put(payload)
        alias = _KIND_EVENT_ALIAS.get((alert.kind, event_type))
        if alias:
            await self._state.events.put({**payload, "type": alias})

    async def _notify(self, alert: AlertSession) -> None:
        """Push to phone and configured notification channels."""
        text = alert.body

        if self._kdeconnect:
            for device in self._kdeconnect._devices.values():
                if device.connected:
                    try:
                        await self._kdeconnect.ping(device.id, message=f"{alert.title}: {text}")
                    except Exception as exc:
                        log.debug("KDE Connect ping failed for %s: %s", device.id, exc)

        if self._notifier:
            try:
                await self._notifier.on_event(f"alert.{alert.kind}", {
                    "title": alert.title,
                    "message": text,
                    "kind": alert.kind,
                    "alert_id": alert.id,
                    "camera": alert.camera,
                    "person": alert.person,
                    "snapshot_url": alert.to_dict().get("snapshot_url", ""),
                })
            except Exception as exc:
                log.debug("Notifier failed for alert %s: %s", alert.id, exc)

    async def _expire_loop(self) -> None:
        while True:
            await asyncio.sleep(5)
            await self._expire_sweep()

    async def _expire_sweep(self) -> None:
        now = time.time()
        stale: list[str] = []
        for alert in list(self._alerts.values()):
            if (alert.state == "active"
                    and alert.timeout_s > 0
                    and now - alert.started_at > alert.timeout_s):
                alert.state = "expired"
                log.debug("Alert expired: id=%s kind=%s", alert.id, alert.kind)
                await self._push("alert.expired", alert)
            if alert.state != "active" and now - alert.started_at > ALERT_TTL_S:
                stale.append(alert.id)
        for aid in stale:
            self._alerts.pop(aid, None)
