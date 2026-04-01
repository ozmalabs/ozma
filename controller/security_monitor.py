# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Physical security monitoring — tamper detection, classified access control.

Uses expansion sensors and audit logging for physical security:
  - Rack tamper detection (accelerometer, door contact)
  - Classified system access monitoring (hardware-only, no agent)
  - Physical intrusion alerts (vibration, door open)
  - Environmental anomaly detection (sudden temp changes)

All events are logged to the audit trail and can trigger:
  - RGB SYSTEM alerts (red strobe)
  - Webhook/Slack/Discord notifications
  - Session recording (start recording all screens on alert)
  - Syslog forwarding (SIEM integration)
"""

from __future__ import annotations

import asyncio
import logging
import time
from dataclasses import dataclass, field
from typing import Any

log = logging.getLogger("ozma.security")


@dataclass
class SecurityZone:
    """A physical security zone (rack, room, desk)."""
    id: str
    name: str
    node_ids: list[str] = field(default_factory=list)
    armed: bool = True
    # Sensor thresholds
    vibration_threshold: float = 2.0     # g-force for tamper
    door_contact_pin: str = ""           # GPIO pin for door contact
    temp_delta_threshold: float = 10.0   # °C change in 5 min = anomaly

    def to_dict(self) -> dict[str, Any]:
        return {"id": self.id, "name": self.name, "armed": self.armed,
                "nodes": self.node_ids}


class SecurityMonitor:
    """
    Monitors physical security zones using node expansion sensors.

    Watches for:
      - Vibration above threshold (rack tamper)
      - Door contact open (rack door opened)
      - Rapid temperature changes (fire, HVAC failure, door left open)
      - Unexpected power state changes
    """

    def __init__(self, state: Any = None, metrics: Any = None,
                 audit: Any = None, rgb: Any = None, notifier: Any = None) -> None:
        self._state = state
        self._metrics = metrics
        self._audit = audit
        self._rgb = rgb
        self._notifier = notifier
        self._zones: dict[str, SecurityZone] = {}
        self._task: asyncio.Task | None = None
        self._temp_history: dict[str, list[tuple[float, float]]] = {}

    async def start(self) -> None:
        self._task = asyncio.create_task(self._watch_loop(), name="security-monitor")
        log.info("Security monitor started (%d zones)", len(self._zones))

    async def stop(self) -> None:
        if self._task:
            self._task.cancel()

    def add_zone(self, zone: SecurityZone) -> None:
        self._zones[zone.id] = zone

    def arm_zone(self, zone_id: str) -> bool:
        z = self._zones.get(zone_id)
        if z:
            z.armed = True
            return True
        return False

    def disarm_zone(self, zone_id: str) -> bool:
        z = self._zones.get(zone_id)
        if z:
            z.armed = False
            return True
        return False

    def list_zones(self) -> list[dict]:
        return [z.to_dict() for z in self._zones.values()]

    async def _watch_loop(self) -> None:
        while True:
            try:
                if self._metrics:
                    for zone in self._zones.values():
                        if zone.armed:
                            await self._check_zone(zone)
                await asyncio.sleep(5)
            except asyncio.CancelledError:
                return

    async def _check_zone(self, zone: SecurityZone) -> None:
        """Check all sensors in a security zone for anomalies."""
        for node_id in zone.node_ids:
            data = self._metrics.get_device(node_id)
            if not data:
                continue
            metrics = data.get("metrics", {})

            # Vibration check
            for key, info in metrics.items():
                val = info if isinstance(info, (int, float)) else info.get("value", 0)

                if "vibration" in key or "accel" in key:
                    if abs(val) > zone.vibration_threshold:
                        await self._trigger_alert(zone, node_id, "tamper",
                            f"Vibration detected: {val:.2f}g (threshold: {zone.vibration_threshold}g)")

                if "door" in key or "contact" in key:
                    if val > 0:  # Contact open
                        await self._trigger_alert(zone, node_id, "door_open",
                            "Rack door opened")

            # Temperature anomaly (rapid change)
            for key, info in metrics.items():
                if "temperature" in key:
                    val = info if isinstance(info, (int, float)) else info.get("value", 0)
                    hist_key = f"{node_id}:{key}"
                    if hist_key not in self._temp_history:
                        self._temp_history[hist_key] = []
                    self._temp_history[hist_key].append((time.time(), val))
                    # Keep last 5 minutes
                    cutoff = time.time() - 300
                    self._temp_history[hist_key] = [
                        (t, v) for t, v in self._temp_history[hist_key] if t > cutoff
                    ]
                    if len(self._temp_history[hist_key]) >= 2:
                        oldest = self._temp_history[hist_key][0][1]
                        delta = abs(val - oldest)
                        if delta > zone.temp_delta_threshold:
                            await self._trigger_alert(zone, node_id, "temp_anomaly",
                                f"Temperature changed {delta:.1f}°C in 5 min")

    async def _trigger_alert(self, zone: SecurityZone, node_id: str,
                              alert_type: str, message: str) -> None:
        """Fire a security alert."""
        log.warning("SECURITY [%s] %s on %s: %s", zone.name, alert_type, node_id, message)

        if self._audit:
            self._audit.log_event("security", node_id, {
                "zone": zone.id, "alert": alert_type, "message": message,
            }, severity="critical")

        if self._rgb:
            self._rgb.compositor.set_system_alert(
                f"security-{zone.id}", color=(255, 0, 0), effect="strobe"
            )

        if self._notifier:
            await self._notifier.on_event("security.alert", {
                "zone": zone.name, "node": node_id,
                "alert": alert_type, "message": message,
            })
