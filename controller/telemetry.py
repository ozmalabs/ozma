# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Telemetry — anonymous usage and device metrics sent to Connect.

Data collection policy (enforced here, defined in the plan):

  Free / Pro / Team:
    We collect anonymous usage telemetry and device health data to
    improve the product:
      ✓ Device metrics (CPU, RAM, disk, temp, displays)
      ✓ Usage patterns (feature frequency, scenario switch count)
      ✓ Error reports (crash logs, HID errors, pipeline failures)
      ✓ Connection quality (latency, packet loss, jitter)
      ✓ Node inventory (hardware type, OS, capabilities)

    We NEVER collect or inspect your actual data:
      ✗ Screen contents — E2E encrypted, we can't read them
      ✗ Audio streams — E2E encrypted
      ✗ Keyboard input — E2E encrypted
      ✗ Clipboard contents
      ✗ File contents
      ✗ Transcription text

  Business (Enterprise):
    We collect NOTHING. Zero telemetry. Fully isolated.
    The controller never phones home.

HID error logs are always stored locally on the node for debugging.
They contain packet metadata (size, timestamp, error code) — never
the actual keystroke or mouse data.

The telemetry policy is defined in the Connect plan JSON. This module
reads the plan and only sends categories the plan allows. If the plan
says telemetry is off, nothing leaves the machine.
"""

from __future__ import annotations

import asyncio
import json
import logging
import time
from dataclasses import dataclass, field
from typing import Any

log = logging.getLogger("ozma.telemetry")

TELEMETRY_ENDPOINT = "/telemetry/report"
REPORT_INTERVAL = 3600  # 1 hour
LOCAL_ERROR_LOG_MAX = 10000  # max local error log entries


@dataclass
class HIDErrorEntry:
    """Local HID error log entry — metadata only, never keystroke data."""
    timestamp: float
    node_id: str
    error_type: str          # timeout, malformed, decrypt_fail, sequence_gap
    packet_size: int = 0
    sequence_gap: int = 0
    details: str = ""


@dataclass
class TelemetryReport:
    """Aggregated telemetry for one reporting period."""
    # Usage
    scenario_switches: int = 0
    features_used: dict[str, int] = field(default_factory=dict)  # feature → count
    active_hours: float = 0.0

    # Errors
    hid_errors: int = 0
    capture_errors: int = 0
    audio_errors: int = 0
    crash_count: int = 0

    # Connection
    avg_controller_rtt_ms: float = 0.0
    avg_packet_loss: float = 0.0
    relay_uptime_pct: float = 0.0

    # Inventory
    node_count: int = 0
    node_types: dict[str, int] = field(default_factory=dict)  # type → count

    def to_dict(self) -> dict:
        return {
            "scenario_switches": self.scenario_switches,
            "features_used": self.features_used,
            "active_hours": round(self.active_hours, 1),
            "hid_errors": self.hid_errors,
            "capture_errors": self.capture_errors,
            "audio_errors": self.audio_errors,
            "crash_count": self.crash_count,
            "avg_controller_rtt_ms": round(self.avg_controller_rtt_ms, 1),
            "avg_packet_loss": round(self.avg_packet_loss, 4),
            "relay_uptime_pct": round(self.relay_uptime_pct, 1),
            "node_count": self.node_count,
            "node_types": self.node_types,
        }


class TelemetryCollector:
    """
    Collects and reports telemetry according to the plan's data policy.

    If the plan disables telemetry (enterprise), nothing is sent.
    If Connect is not configured, nothing is sent.
    HID errors are always logged locally regardless of plan.
    """

    def __init__(self, connect: Any = None) -> None:
        self._connect = connect
        self._report = TelemetryReport()
        self._hid_errors: list[HIDErrorEntry] = []
        self._task: asyncio.Task | None = None
        self._enabled = False

    async def start(self, plan_telemetry: dict | None = None) -> None:
        """Start telemetry collection if the plan allows it."""
        if not plan_telemetry:
            plan_telemetry = {}

        # Check if any telemetry category is enabled
        self._enabled = any(plan_telemetry.get(k, False) for k in (
            "usage_telemetry", "device_metrics", "error_reports",
            "connection_quality", "node_inventory",
        ))

        if self._enabled and self._connect:
            self._task = asyncio.create_task(
                self._report_loop(plan_telemetry), name="telemetry",
            )
            log.info("Telemetry enabled (anonymous usage + device health)")
        else:
            log.info("Telemetry disabled (enterprise plan or no Connect)")

    async def stop(self) -> None:
        if self._task:
            self._task.cancel()

    # ── Recording ──────────────────────────────────────────────────────

    def record_scenario_switch(self) -> None:
        self._report.scenario_switches += 1

    def record_feature_use(self, feature: str) -> None:
        self._report.features_used[feature] = self._report.features_used.get(feature, 0) + 1

    def record_hid_error(self, node_id: str, error_type: str,
                          packet_size: int = 0, details: str = "") -> None:
        """Record an HID error. Always stored locally. Sent to Connect only if plan allows."""
        entry = HIDErrorEntry(
            timestamp=time.time(),
            node_id=node_id,
            error_type=error_type,
            packet_size=packet_size,
            details=details,
        )
        self._hid_errors.append(entry)
        if len(self._hid_errors) > LOCAL_ERROR_LOG_MAX:
            self._hid_errors = self._hid_errors[-LOCAL_ERROR_LOG_MAX:]
        self._report.hid_errors += 1

    def record_capture_error(self) -> None:
        self._report.capture_errors += 1

    def record_audio_error(self) -> None:
        self._report.audio_errors += 1

    def record_crash(self) -> None:
        self._report.crash_count += 1

    def update_connection_stats(self, rtt_ms: float, packet_loss: float) -> None:
        self._report.avg_controller_rtt_ms = rtt_ms
        self._report.avg_packet_loss = packet_loss

    def update_inventory(self, node_count: int, node_types: dict[str, int]) -> None:
        self._report.node_count = node_count
        self._report.node_types = node_types

    # ── Local error log access ─────────────────────────────────────────

    def get_hid_errors(self, last_n: int = 100) -> list[dict]:
        """Get recent HID errors for local debugging."""
        return [
            {
                "timestamp": e.timestamp,
                "node_id": e.node_id,
                "error_type": e.error_type,
                "packet_size": e.packet_size,
                "details": e.details,
            }
            for e in self._hid_errors[-last_n:]
        ]

    # ── Reporting ──────────────────────────────────────────────────────

    async def _report_loop(self, policy: dict) -> None:
        """Send telemetry to Connect according to the plan policy."""
        while True:
            await asyncio.sleep(REPORT_INTERVAL)
            if not self._connect or not self._connect.authenticated:
                continue

            # Build report based on what the policy allows
            payload: dict[str, Any] = {"timestamp": time.time()}

            if policy.get("usage_telemetry"):
                payload["usage"] = {
                    "scenario_switches": self._report.scenario_switches,
                    "features_used": self._report.features_used,
                    "active_hours": round(self._report.active_hours, 1),
                }

            if policy.get("error_reports"):
                payload["errors"] = {
                    "hid_errors": self._report.hid_errors,
                    "capture_errors": self._report.capture_errors,
                    "audio_errors": self._report.audio_errors,
                    "crash_count": self._report.crash_count,
                }

            if policy.get("connection_quality"):
                payload["connection"] = {
                    "avg_rtt_ms": round(self._report.avg_controller_rtt_ms, 1),
                    "avg_packet_loss": round(self._report.avg_packet_loss, 4),
                    "relay_uptime_pct": round(self._report.relay_uptime_pct, 1),
                }

            if policy.get("node_inventory"):
                payload["inventory"] = {
                    "node_count": self._report.node_count,
                    "node_types": self._report.node_types,
                }

            if payload.keys() - {"timestamp"}:
                await self._connect._api_post(TELEMETRY_ENDPOINT, payload)

            # Reset counters for next period
            self._report = TelemetryReport()

    # ── Status ─────────────────────────────────────────────────────────

    def status(self) -> dict:
        return {
            "enabled": self._enabled,
            "local_hid_errors": len(self._hid_errors),
            "current_period": self._report.to_dict(),
        }
