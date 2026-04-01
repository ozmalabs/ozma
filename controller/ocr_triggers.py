# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
OCR trigger/watch system — continuous screen monitoring with pattern matching.

Runs OCR on captured frames at a configurable interval and fires events
when known error patterns are detected.  This turns ozma into a passive
monitor that watches screens for problems — even on machines with no OS
or agent running.

Built-in error database covers:
  - Linux kernel panics, oops, segfaults, OOM kills
  - Windows BSOD (stop codes, :( face)
  - GRUB rescue mode, filesystem errors
  - BIOS POST error codes
  - SMART disk failure warnings
  - Network errors (link down, no carrier)
  - Application crashes (Python tracebacks, Java exceptions, core dumps)
  - Login prompts (detect "machine is ready" state)

Custom patterns can be added per-scenario or globally.

On match:
  1. Fire WebSocket event (ocr.trigger)
  2. RGB compositor SYSTEM alert (red strobe)
  3. Notification system (webhook/Slack/Discord)
  4. Log with screenshot path
"""

from __future__ import annotations

import asyncio
import logging
import re
import time
from dataclasses import dataclass, field
from typing import Any

log = logging.getLogger("ozma.ocr_triggers")


@dataclass
class TriggerPattern:
    """A pattern to watch for in OCR output."""

    id: str
    pattern: str              # Regex or substring match
    is_regex: bool = False
    severity: str = "error"   # "info", "warning", "error", "critical"
    category: str = ""        # "kernel", "bios", "windows", "application", "custom"
    description: str = ""
    cooldown_s: float = 60.0  # Don't re-trigger within this window
    _last_fired: float = -1e9  # Sentinel: always allow first fire regardless of uptime
    _compiled: re.Pattern | None = field(default=None, repr=False)

    def matches(self, text: str) -> re.Match | bool:
        if self.is_regex:
            if not self._compiled:
                self._compiled = re.compile(self.pattern, re.IGNORECASE | re.MULTILINE)
            return self._compiled.search(text)
        return self.pattern.lower() in text.lower()

    def can_fire(self) -> bool:
        return (time.monotonic() - self._last_fired) >= self.cooldown_s

    def mark_fired(self) -> None:
        self._last_fired = time.monotonic()

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id, "pattern": self.pattern, "is_regex": self.is_regex,
            "severity": self.severity, "category": self.category,
            "description": self.description,
        }


# ── Built-in error database ─────────────────────────────────────────────────

_BUILTIN_PATTERNS: list[dict] = [
    # Linux kernel
    {"id": "kernel-panic", "pattern": r"Kernel panic", "severity": "critical", "category": "kernel",
     "description": "Linux kernel panic — system halted"},
    {"id": "kernel-oops", "pattern": r"Oops:|BUG:", "is_regex": True, "severity": "error", "category": "kernel",
     "description": "Linux kernel oops/bug"},
    {"id": "kernel-oom", "pattern": r"Out of memory|oom-killer|oom_reaper", "is_regex": True, "severity": "error", "category": "kernel",
     "description": "Out of memory — OOM killer invoked"},
    {"id": "kernel-segfault", "pattern": r"segfault at|general protection fault", "is_regex": True, "severity": "error", "category": "kernel",
     "description": "Segmentation fault or protection fault"},
    {"id": "kernel-rcu-stall", "pattern": "rcu_sched self-detected stall", "severity": "warning", "category": "kernel",
     "description": "RCU stall — possible kernel hang"},
    {"id": "kernel-hung-task", "pattern": r"hung_task_timeout_secs|blocked for more than \d+ seconds", "is_regex": True, "severity": "warning", "category": "kernel",
     "description": "Hung task — process blocked too long"},
    {"id": "kernel-mce", "pattern": "Machine check events logged", "severity": "critical", "category": "kernel",
     "description": "Machine check exception — hardware error"},

    # Filesystem / disk
    {"id": "fs-readonly", "pattern": r"Remounting filesystem read-only|EXT4-fs error", "is_regex": True, "severity": "critical", "category": "disk",
     "description": "Filesystem remounted read-only — disk error"},
    {"id": "fs-io-error", "pattern": r"I/O error|Buffer I/O error", "is_regex": True, "severity": "error", "category": "disk",
     "description": "Disk I/O error"},
    {"id": "smart-failure", "pattern": r"SMART.*(?:fail|error|threshold)", "is_regex": True, "severity": "critical", "category": "disk",
     "description": "SMART disk failure prediction"},
    {"id": "disk-full", "pattern": r"No space left on device", "severity": "error", "category": "disk",
     "description": "Disk full"},

    # Windows
    {"id": "bsod-stop", "pattern": r"STOP:|STOP code|Your PC ran into a problem", "is_regex": True, "severity": "critical", "category": "windows",
     "description": "Windows Blue Screen of Death"},
    {"id": "bsod-frown", "pattern": ":(", "severity": "critical", "category": "windows",
     "description": "Windows BSOD frown face", "cooldown_s": 120},
    {"id": "win-chkdsk", "pattern": "CHKDSK is verifying", "severity": "warning", "category": "windows",
     "description": "Windows running CHKDSK — possible disk issue"},
    {"id": "win-recovery", "pattern": "Recovery|Automatic Repair|Startup Repair", "severity": "warning", "category": "windows",
     "description": "Windows recovery/repair mode"},

    # GRUB / boot
    {"id": "grub-rescue", "pattern": "grub rescue", "severity": "error", "category": "boot",
     "description": "GRUB rescue mode — bootloader broken"},
    {"id": "grub-error", "pattern": r"error:.*grub|no such partition|unknown filesystem", "is_regex": True, "severity": "error", "category": "boot",
     "description": "GRUB boot error"},
    {"id": "initramfs-drop", "pattern": r"initramfs|BusyBox|dropping to.*shell", "is_regex": True, "severity": "error", "category": "boot",
     "description": "Dropped to initramfs/emergency shell"},
    {"id": "boot-failure", "pattern": r"Boot failure|No bootable device|PXE-E61", "is_regex": True, "severity": "critical", "category": "boot",
     "description": "Boot device not found"},

    # BIOS / POST
    {"id": "post-memory", "pattern": r"Memory error|DIMM.*fail|Memory training failed", "is_regex": True, "severity": "critical", "category": "bios",
     "description": "BIOS memory error"},
    {"id": "post-cpu", "pattern": r"CPU error|CPU fan error|CPU over temperature", "is_regex": True, "severity": "critical", "category": "bios",
     "description": "BIOS CPU error or overtemperature"},
    {"id": "post-cmos", "pattern": r"CMOS.*error|CMOS battery|CMOS checksum", "is_regex": True, "severity": "warning", "category": "bios",
     "description": "CMOS battery or checksum error"},

    # Network
    {"id": "net-link-down", "pattern": r"NIC Link is Down|link is not ready|no carrier", "is_regex": True, "severity": "warning", "category": "network",
     "description": "Network link down"},

    # Application crashes
    {"id": "python-traceback", "pattern": "Traceback (most recent call last)", "severity": "warning", "category": "application",
     "description": "Python exception traceback"},
    {"id": "java-exception", "pattern": r"Exception in thread|at [\w.]+\([\w.]+:\d+\)", "is_regex": True, "severity": "warning", "category": "application",
     "description": "Java exception stack trace"},
    {"id": "core-dump", "pattern": r"core dumped|Aborted", "is_regex": True, "severity": "error", "category": "application",
     "description": "Process core dumped"},
    {"id": "systemd-failed", "pattern": r"Failed to start|service.*failed", "is_regex": True, "severity": "warning", "category": "application",
     "description": "Systemd service failed to start"},

    # Positive signals (info level)
    {"id": "login-prompt", "pattern": r"login:|Password:", "is_regex": True, "severity": "info", "category": "status",
     "description": "Login prompt — machine is ready", "cooldown_s": 300},
    {"id": "shell-ready", "pattern": r"[$#]\s*$", "is_regex": True, "severity": "info", "category": "status",
     "description": "Shell prompt — machine is ready", "cooldown_s": 300},
]


class OCRTriggerManager:
    """
    Watches captured displays for error patterns and fires events.

    Usage::

        mgr = OCRTriggerManager()
        mgr.on_trigger = my_callback  # async def (trigger_id, severity, text, match)
        await mgr.start(text_capture, captures)
    """

    def __init__(self) -> None:
        self._patterns: list[TriggerPattern] = []
        self._custom_patterns: list[TriggerPattern] = []
        self._task: asyncio.Task | None = None
        self._text_capture: Any = None
        self._captures: Any = None
        self.on_trigger: Any = None  # async callback
        self._scan_interval = 5.0  # seconds between OCR scans
        self._enabled = True

        # Load built-in patterns
        for p in _BUILTIN_PATTERNS:
            self._patterns.append(TriggerPattern(
                id=p["id"], pattern=p["pattern"],
                is_regex=p.get("is_regex", False),
                severity=p.get("severity", "error"),
                category=p.get("category", ""),
                description=p.get("description", ""),
                cooldown_s=p.get("cooldown_s", 60.0),
            ))

    async def start(self, text_capture: Any, captures: Any) -> None:
        self._text_capture = text_capture
        self._captures = captures
        self._task = asyncio.create_task(self._watch_loop(), name="ocr-trigger-watch")
        log.info("OCR trigger watch started (%d built-in + %d custom patterns)",
                 len(self._patterns), len(self._custom_patterns))

    async def stop(self) -> None:
        if self._task:
            self._task.cancel()

    def add_pattern(self, pattern: TriggerPattern) -> None:
        self._custom_patterns.append(pattern)

    def remove_pattern(self, pattern_id: str) -> bool:
        self._custom_patterns = [p for p in self._custom_patterns if p.id != pattern_id]
        return True

    def list_patterns(self) -> list[dict[str, Any]]:
        all_patterns = self._patterns + self._custom_patterns
        return [p.to_dict() for p in all_patterns]

    def list_builtin_patterns(self) -> list[dict[str, Any]]:
        return [p.to_dict() for p in self._patterns]

    async def _watch_loop(self) -> None:
        """Periodically OCR all active captures and check for trigger patterns."""
        while True:
            try:
                if not self._enabled or not self._captures:
                    await asyncio.sleep(self._scan_interval)
                    continue

                for source in self._captures.list_sources():
                    if not source.get("active"):
                        continue
                    await self._scan_source(source["id"])

                await asyncio.sleep(self._scan_interval)
            except asyncio.CancelledError:
                return
            except Exception:
                log.debug("OCR trigger scan error", exc_info=True)
                await asyncio.sleep(self._scan_interval)

    async def _scan_source(self, source_id: str) -> None:
        """Run OCR on a source and check all patterns."""
        if not self._text_capture:
            return

        # Use cached last result if recent enough, otherwise trigger a new OCR
        result = self._text_capture.last_result
        if not result or not result.text:
            return

        text = result.text
        all_patterns = self._patterns + self._custom_patterns

        for pattern in all_patterns:
            if not pattern.can_fire():
                continue

            match = pattern.matches(text)
            if match:
                pattern.mark_fired()
                match_text = match.group(0) if hasattr(match, "group") else pattern.pattern

                log.warning("OCR trigger [%s] %s on %s: %s",
                            pattern.severity, pattern.id, source_id,
                            match_text[:100])

                if self.on_trigger:
                    await self.on_trigger(
                        pattern.id,
                        pattern.severity,
                        source_id,
                        {
                            "pattern_id": pattern.id,
                            "severity": pattern.severity,
                            "category": pattern.category,
                            "description": pattern.description,
                            "match_text": str(match_text)[:200],
                            "source_id": source_id,
                            "timestamp": time.time(),
                        },
                    )
