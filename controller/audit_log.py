# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Compliance audit logging — unified access trail for regulated environments.

Combines all observation channels into a tamper-evident audit log:
  - HID events (what was typed/clicked, timestamped)
  - Display state (OCR snapshots at configurable intervals)
  - Serial console output (kernel messages, login events)
  - Session recordings (video files with timestamps)
  - Scenario switches (who switched, when, from/to)
  - Power events (on/off/reset/cycle)
  - Sensor data (environmental conditions during access)
  - Metric snapshots (system state at each event)

Compliance targets:
  HIPAA   — access to medical systems must be logged
  SOX     — financial system access audit trail
  PCI-DSS — cardholder data environment access logging
  NIST    — federal system access monitoring
  ISO27001 — information security event logging

The audit log is:
  - Append-only (no modification of past entries)
  - Timestamped (monotonic + wall clock)
  - Hashchained (each entry includes hash of previous — tamper detection)
  - Exportable (JSON lines, syslog, or direct to SIEM)

Storage: local JSONL files + optional real-time export to syslog/SIEM.
Retention: configurable (default 90 days).
"""

from __future__ import annotations

import asyncio
import hashlib
import asyncio
import hashlib
import json
import logging
import os
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.audit")

AUDIT_DIR = Path(__file__).parent / "audit_logs"


@dataclass
class AuditEntry:
    """A single audit log entry."""
    timestamp: float
    event_type: str          # hid, ocr, serial, scenario, power, login, sensor, recording
    source: str              # node_id or "controller"
    severity: str = "info"   # info, warning, error, critical
    data: dict = field(default_factory=dict)
    prev_hash: str = ""      # Hash of previous entry (chain)
    entry_hash: str = ""     # Hash of this entry

    def compute_hash(self) -> str:
        content = f"{self.timestamp}:{self.event_type}:{self.source}:{json.dumps(self.data, sort_keys=True)}:{self.prev_hash}"
        self.entry_hash = hashlib.sha256(content.encode()).hexdigest()[:32]
        return self.entry_hash

    def to_dict(self) -> dict[str, Any]:
        return {
            "ts": self.timestamp, "type": self.event_type,
            "source": self.source, "severity": self.severity,
            "data": self.data, "hash": self.entry_hash,
        }

    def to_jsonl(self) -> str:
        return json.dumps(self.to_dict())


class AuditLogger:
    """
    Append-only, hashchained audit logger for compliance.

    All ozma events funnel through here when audit mode is enabled.
    """

    def __init__(self, enabled: bool = True, retention_days: int = 90) -> None:
        self._enabled = enabled
        self._retention_days = retention_days
        self._prev_hash = "genesis"
        self._file = None
        self._entry_count = 0
        self._syslog_exporter = None  # Set externally for SIEM forwarding

    async def start(self) -> None:
        if not self._enabled:
            return
        AUDIT_DIR.mkdir(parents=True, exist_ok=True)
        date_str = time.strftime("%Y%m%d")
        self._file = open(AUDIT_DIR / f"audit-{date_str}.jsonl", "a")
        log.info("Audit logging enabled (retention: %d days)", self._retention_days)
        # Prune old logs
        self._prune()

    async def stop(self) -> None:
        if self._file:
            self._file.close()

    @property
    def enabled(self) -> bool:
        return self._enabled

    def log_event(self, event_type: str, source: str, data: dict,
                  severity: str = "info") -> None:
        """Log an audit event."""
        if not self._enabled:
            return

        entry = AuditEntry(
            timestamp=time.time(),
            event_type=event_type,
            source=source,
            severity=severity,
            data=data,
            prev_hash=self._prev_hash,
        )
        entry.compute_hash()
        self._prev_hash = entry.entry_hash

        if self._file:
            self._file.write(entry.to_jsonl() + "\n")
            self._file.flush()

        self._entry_count += 1

        # Forward to syslog if configured
        if self._syslog_exporter:
            asyncio.create_task(
                self._syslog_exporter.export_alert({
                    "severity": severity,
                    "description": f"audit:{event_type}",
                    "match_text": json.dumps(data)[:500],
                }),
            )

    def log_messaging_event(self, direction: str, channel: str, sender: str, 
                           message_hash: str, data: dict = None) -> None:
        """Log messaging events when MESSAGING_AUDIT is enabled."""
        if not self._enabled or os.getenv("MESSAGING_AUDIT") != "1":
            return
            
        event_data = {
            "channel": channel,
            "sender": sender,
            "message_hash": message_hash,
            "direction": direction
        }
        if data:
            event_data["data"] = data
            
        self.log_event("messaging.message", "messaging_bridge", event_data, "info")

    def log_webhook_event(self, channel: str, sender: str, body_hash: str) -> None:
        """Log webhook events when MESSAGING_AUDIT is enabled."""
        if not self._enabled or os.getenv("MESSAGING_AUDIT") != "1":
            return
            
        event_data = {
            "channel": channel,
            "sender": sender,
            "body_hash": body_hash
        }
        self.log_event("messaging.webhook", "messaging_bridge", event_data, "info")

    def log_messaging_event(self, direction: str, channel: str, sender: str, 
                           message_hash: str, data: dict = None) -> None:
        """Log messaging events when MESSAGING_AUDIT is enabled."""
        if not self._enabled or os.getenv("MESSAGING_AUDIT") != "1":
            return
            
        event_data = {
            "channel": channel,
            "sender": sender,
            "message_hash": message_hash,
            "direction": direction
        }
        if data:
            event_data["data"] = data
            
        self.log_event("messaging.message", "messaging_bridge", event_data, "info")

    # Convenience methods for common event types
    def log_scenario_switch(self, from_id: str, to_id: str, source: str = "controller") -> None:
        self.log_event("scenario", source, {"from": from_id, "to": to_id})

    def log_power_action(self, node_id: str, action: str) -> None:
        self.log_event("power", node_id, {"action": action}, severity="warning")

    def log_hid_session(self, node_id: str, duration_s: float, keystrokes: int) -> None:
        self.log_event("hid_session", node_id, {
            "duration_s": round(duration_s, 1), "keystrokes": keystrokes,
        })

    def log_serial_alert(self, console_id: str, severity: str, text: str) -> None:
        self.log_event("serial", console_id, {"text": text[:500]}, severity=severity)

    def log_ocr_trigger(self, source_id: str, pattern_id: str, text: str) -> None:
        self.log_event("ocr_trigger", source_id, {
            "pattern": pattern_id, "text": text[:500],
        }, severity="warning")

    def log_login_detected(self, node_id: str, method: str = "ocr") -> None:
        self.log_event("login", node_id, {"method": method})

    def log_recording_start(self, source_id: str, filename: str) -> None:
        self.log_event("recording", source_id, {"action": "start", "file": filename})

    def log_recording_stop(self, source_id: str, filename: str, duration_s: float) -> None:
        self.log_event("recording", source_id, {
            "action": "stop", "file": filename, "duration_s": round(duration_s, 1),
        })

    # Query
    def get_recent(self, lines: int = 100) -> list[dict]:
        """Read recent entries from today's log."""
        date_str = time.strftime("%Y%m%d")
        path = AUDIT_DIR / f"audit-{date_str}.jsonl"
        if not path.exists():
            return []
        all_lines = path.read_text().strip().splitlines()
        recent = all_lines[-lines:]
        return [json.loads(l) for l in recent]

    def verify_chain(self) -> dict[str, Any]:
        """Verify the hash chain integrity of today's log."""
        entries = self.get_recent(10000)
        if not entries:
            return {"ok": True, "entries": 0}
        broken_at = None
        for i in range(1, len(entries)):
            # Recompute hash of entry i and check it matches
            content = f"{entries[i]['ts']}:{entries[i]['type']}:{entries[i]['source']}:{json.dumps(entries[i]['data'], sort_keys=True)}:{entries[i-1].get('hash', '')}"
            expected = hashlib.sha256(content.encode()).hexdigest()[:32]
            if expected != entries[i].get("hash"):
                broken_at = i
                break
        return {"ok": broken_at is None, "entries": len(entries), "broken_at": broken_at}

    def _prune(self) -> None:
        """Remove audit logs older than retention period."""
        if not AUDIT_DIR.exists():
            return
        cutoff = time.time() - self._retention_days * 86400
        for f in AUDIT_DIR.glob("audit-*.jsonl"):
            if f.stat().st_mtime < cutoff:
                f.unlink()
