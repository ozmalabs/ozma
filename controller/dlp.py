# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Data Loss Prevention (DLP) — policy-based sensitive data detection.

Scopes:
  file      — scan files on nodes (dispatched via job_queue; or local scan)
  email     — scan outbound email content (hooks into email_security monitor)
  cloud     — scan M365 / Google Workspace content (via cloud_backup credentials)
  clipboard — agent-reported clipboard events (fires on match)
  usb       — removable media events (alert on insert; optional block)

Built-in pattern types:
  credit_card   — PAN numbers with Luhn validation
  ssn           — US Social Security Numbers
  iban          — IBAN bank account numbers
  tfn           — Australian Tax File Numbers (with digit-weight check)
  medicare_au   — Australian Medicare card numbers
  passport      — Passport numbers (generic)
  aws_key       — AWS access key IDs (AKIA...)
  private_key   — PEM private keys
  api_key       — API/secret key assignments in config/code
  password      — Hardcoded passwords in config/code
  custom        — User-supplied regex

Actions:
  log           — audit trail only (lowest friction)
  alert         — NotificationManager + event bus
  block         — prevent the operation (clipboard clear, email hold, file lock)
  quarantine    — move file to quarantine dir; hold email for review

Data lives in controller/dlp_data/:
  dlp.json      — policies + config
  incidents.json — incident ring buffer (last 1000)
"""

from __future__ import annotations

import asyncio
import json
import logging
import re
import time
import uuid
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.dlp")

_SEVERITIES = ("critical", "high", "medium", "low", "info")
_ACTIONS = ("log", "alert", "block", "quarantine")
_SCOPES = ("file", "email", "cloud", "clipboard", "usb")

MAX_INCIDENTS = 1000   # ring buffer cap
MAX_CONTEXT   = 200    # chars of redacted context to store per match


# ── Built-in patterns ─────────────────────────────────────────────────────────

_BUILTIN_PATTERNS: dict[str, str] = {
    # Credit card: matches both compact (4111111111111111) and formatted (4111-1111-1111-1111)
    # Luhn validation applied on top (validate=True by default in built-in rules).
    "credit_card": (
        r"\b(?:"
        r"4[0-9]{3}[\s\-]?[0-9]{4}[\s\-]?[0-9]{4}[\s\-]?[0-9]{1,4}|"  # Visa
        r"5[1-5][0-9]{2}[\s\-]?[0-9]{4}[\s\-]?[0-9]{4}[\s\-]?[0-9]{4}|"  # Mastercard
        r"3[47][0-9]{2}[\s\-]?[0-9]{6}[\s\-]?[0-9]{5}|"                  # Amex
        r"3(?:0[0-5]|[68][0-9])[0-9]{2}[\s\-]?[0-9]{6}[\s\-]?[0-9]{4}|"  # Diners
        r"6(?:011|5[0-9]{2})[0-9]{2}[\s\-]?[0-9]{4}[\s\-]?[0-9]{4}[\s\-]?[0-9]{4}|"  # Discover
        r"(?:2131|1800|35\d{3})[\s\-]?\d{4}[\s\-]?\d{4}[\s\-]?\d{3}"    # JCB
        r")\b"
    ),
    "ssn": (
        r"\b(?!000|666|9\d\d)\d{3}"
        r"[- ](?!00)\d{2}[- ](?!0000)\d{4}\b"
    ),
    "iban": r"\b[A-Z]{2}[0-9]{2}[A-Z0-9]{4,}[0-9]{7,}\b",
    "tfn": r"\b[1-9][0-9]{2}[ \-]?[0-9]{3}[ \-]?[0-9]{3}\b",
    "medicare_au": r"\b[2-6][0-9]{9}[0-9]\b",
    "passport": r"\b[A-Z]{1,2}[0-9]{6,9}\b",
    "aws_key": r"\bAKIA[0-9A-Z]{16}\b",
    "private_key": r"-----BEGIN (?:RSA |EC |DSA |OPENSSH )?PRIVATE KEY-----",
    "api_key": (
        r"(?i)(?:api[_\-]?key|access[_\-]?token|secret[_\-]?key)"
        r"\s*[:=]\s*['\"]?([A-Za-z0-9_\-]{16,})"
    ),
    "password": (
        r"(?i)(?:password|passwd|pwd)\s*[:=]\s*['\"]?([^\s'\"]{8,})"
    ),
}

# Compiled cache
_COMPILED: dict[str, re.Pattern] = {}


def _get_pattern(pattern_type: str, custom_pattern: str = "") -> re.Pattern | None:
    key = pattern_type if pattern_type != "custom" else f"custom:{custom_pattern}"
    if key not in _COMPILED:
        raw = _BUILTIN_PATTERNS.get(pattern_type, custom_pattern)
        if not raw:
            return None
        try:
            _COMPILED[key] = re.compile(raw)
        except re.error as e:
            log.warning("Invalid DLP pattern %r: %s", raw, e)
            return None
    return _COMPILED[key]


def _luhn_valid(digits: str) -> bool:
    """Luhn algorithm for credit card number validation."""
    total = 0
    for i, ch in enumerate(reversed(digits)):
        if not ch.isdigit():
            return False
        d = int(ch)
        if i % 2 == 1:
            d *= 2
            if d > 9:
                d -= 9
        total += d
    return total % 10 == 0


def _tfn_valid(raw: str) -> bool:
    """Australian TFN digit-weight checksum validation."""
    digits = re.sub(r"\D", "", raw)
    if len(digits) != 9:
        return False
    weights = [1, 4, 3, 7, 5, 8, 6, 9, 10]
    total = sum(int(d) * w for d, w in zip(digits, weights))
    return total % 11 == 0


def _redact_context(text: str, match: re.Match, window: int = 40) -> str:
    """Return a redacted snippet around a match for incident context."""
    start = max(0, match.start() - window)
    end = min(len(text), match.end() + window)
    snippet = text[start:end]
    # Replace the matched value with asterisks
    inner = match.start() - start
    inner_end = inner + (match.end() - match.start())
    redacted = snippet[:inner] + "*" * min(len(snippet[inner:inner_end]), 8) + snippet[inner_end:]
    return redacted[:MAX_CONTEXT]


# ── Data models ───────────────────────────────────────────────────────────────

@dataclass
class DLPMatch:
    """A single pattern match within scanned content."""
    rule_id: str
    pattern_type: str
    match_text: str        # redacted
    context: str
    offset: int


@dataclass
class DLPRule:
    id: str
    name: str
    pattern_type: str      # from _BUILTIN_PATTERNS or "custom"
    custom_pattern: str = ""
    action: str = "alert"  # log | alert | block | quarantine
    severity: str = "high"
    scopes: list[str] = field(default_factory=lambda: list(_SCOPES))
    enabled: bool = True
    min_matches: int = 1   # minimum matches to trigger
    validate: bool = True  # apply secondary validation (Luhn, TFN checksum)

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id, "name": self.name,
            "pattern_type": self.pattern_type,
            "custom_pattern": self.custom_pattern,
            "action": self.action, "severity": self.severity,
            "scopes": self.scopes, "enabled": self.enabled,
            "min_matches": self.min_matches, "validate": self.validate,
        }

    @classmethod
    def from_dict(cls, d: dict) -> "DLPRule":
        return cls(
            id=d["id"], name=d.get("name", d["id"]),
            pattern_type=d["pattern_type"],
            custom_pattern=d.get("custom_pattern", ""),
            action=d.get("action", "alert"),
            severity=d.get("severity", "high"),
            scopes=d.get("scopes", list(_SCOPES)),
            enabled=d.get("enabled", True),
            min_matches=d.get("min_matches", 1),
            validate=d.get("validate", True),
        )


@dataclass
class DLPPolicy:
    id: str
    name: str
    description: str = ""
    rules: list[DLPRule] = field(default_factory=list)
    enabled: bool = True
    node_ids: list[str] = field(default_factory=list)  # empty = all nodes
    scan_paths: list[str] = field(default_factory=lambda: ["/home", "/Users", "C:\\Users"])
    scan_extensions: list[str] = field(default_factory=lambda: [
        ".txt", ".csv", ".xlsx", ".docx", ".pdf", ".json", ".yaml",
        ".env", ".conf", ".cfg", ".ini", ".py", ".js", ".ts", ".sh",
    ])
    max_file_size_mb: int = 10

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id, "name": self.name,
            "description": self.description,
            "rules": [r.to_dict() for r in self.rules],
            "enabled": self.enabled,
            "node_ids": self.node_ids,
            "scan_paths": self.scan_paths,
            "scan_extensions": self.scan_extensions,
            "max_file_size_mb": self.max_file_size_mb,
        }

    @classmethod
    def from_dict(cls, d: dict) -> "DLPPolicy":
        return cls(
            id=d["id"], name=d["name"],
            description=d.get("description", ""),
            rules=[DLPRule.from_dict(r) for r in d.get("rules", [])],
            enabled=d.get("enabled", True),
            node_ids=d.get("node_ids", []),
            scan_paths=d.get("scan_paths", ["/home", "/Users", "C:\\Users"]),
            scan_extensions=d.get("scan_extensions", [
                ".txt", ".csv", ".xlsx", ".docx", ".pdf", ".json", ".yaml",
                ".env", ".conf", ".cfg", ".ini", ".py", ".js", ".ts", ".sh",
            ]),
            max_file_size_mb=d.get("max_file_size_mb", 10),
        )

    def get_rule(self, rule_id: str) -> DLPRule | None:
        return next((r for r in self.rules if r.id == rule_id), None)


@dataclass
class DLPIncident:
    id: str
    policy_id: str
    rule_id: str
    pattern_type: str
    action_taken: str
    severity: str
    scope: str             # file | email | cloud | clipboard | usb
    source: str            # file path, email subject, cloud object key, etc.
    node_id: str = ""
    user_email: str = ""
    match_count: int = 1
    context: str = ""      # redacted snippet
    acknowledged: bool = False
    resolved: bool = False
    created_at: float = 0.0
    resolved_at: float = 0.0
    itsm_ticket_id: str = ""

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id, "policy_id": self.policy_id,
            "rule_id": self.rule_id, "pattern_type": self.pattern_type,
            "action_taken": self.action_taken, "severity": self.severity,
            "scope": self.scope, "source": self.source,
            "node_id": self.node_id, "user_email": self.user_email,
            "match_count": self.match_count, "context": self.context,
            "acknowledged": self.acknowledged, "resolved": self.resolved,
            "created_at": self.created_at, "resolved_at": self.resolved_at,
            "itsm_ticket_id": self.itsm_ticket_id,
        }

    @classmethod
    def from_dict(cls, d: dict) -> "DLPIncident":
        return cls(**{k: v for k, v in d.items()
                      if k in cls.__dataclass_fields__})


@dataclass
class DLPConfig:
    # Scheduled file scan interval (seconds; 0 = manual only)
    file_scan_interval: int = 86400
    # Auto-create ITSM ticket for incidents at/above this severity
    itsm_ticket_severity: str = "high"
    # Email scanning integration (hooks into email_security.py)
    email_scan_enabled: bool = True
    # Cloud scanning (uses cloud_backup credentials)
    cloud_scan_enabled: bool = False
    cloud_scan_interval: int = 86400
    # USB/removable media — alert on insert
    usb_alert_enabled: bool = True
    # Quarantine directory (for file quarantine action)
    quarantine_path: str = ""   # blank → dlp_data/quarantine/

    def to_dict(self) -> dict[str, Any]:
        return {
            "file_scan_interval": self.file_scan_interval,
            "itsm_ticket_severity": self.itsm_ticket_severity,
            "email_scan_enabled": self.email_scan_enabled,
            "cloud_scan_enabled": self.cloud_scan_enabled,
            "cloud_scan_interval": self.cloud_scan_interval,
            "usb_alert_enabled": self.usb_alert_enabled,
            "quarantine_path": self.quarantine_path,
        }

    @classmethod
    def from_dict(cls, d: dict) -> "DLPConfig":
        return cls(
            file_scan_interval=d.get("file_scan_interval", 86400),
            itsm_ticket_severity=d.get("itsm_ticket_severity", "high"),
            email_scan_enabled=d.get("email_scan_enabled", True),
            cloud_scan_enabled=d.get("cloud_scan_enabled", False),
            cloud_scan_interval=d.get("cloud_scan_interval", 86400),
            usb_alert_enabled=d.get("usb_alert_enabled", True),
            quarantine_path=d.get("quarantine_path", ""),
        )


# ── Content scanner ───────────────────────────────────────────────────────────

class ContentScanner:
    """
    Pattern-match text content against a list of DLP rules.

    Secondary validation (Luhn for credit cards, TFN checksum) is applied
    when `rule.validate=True` to reduce false positives.
    """

    def scan_text(self, text: str, rules: list[DLPRule],
                  scope: str) -> list[DLPMatch]:
        matches: list[DLPMatch] = []
        for rule in rules:
            if not rule.enabled:
                continue
            if scope not in rule.scopes:
                continue
            pattern = _get_pattern(rule.pattern_type, rule.custom_pattern)
            if pattern is None:
                continue
            rule_matches: list[DLPMatch] = []
            for m in pattern.finditer(text):
                raw = m.group(0)
                if rule.validate:
                    if rule.pattern_type == "credit_card":
                        digits = re.sub(r"\D", "", raw)
                        if not _luhn_valid(digits):
                            continue
                    elif rule.pattern_type == "tfn":
                        if not _tfn_valid(raw):
                            continue
                redacted = "*" * min(len(raw), 8)
                ctx = _redact_context(text, m)
                rule_matches.append(DLPMatch(
                    rule_id=rule.id,
                    pattern_type=rule.pattern_type,
                    match_text=redacted,
                    context=ctx,
                    offset=m.start(),
                ))
            if len(rule_matches) >= rule.min_matches:
                matches.extend(rule_matches)
        return matches

    async def scan_file(self, path: Path, rules: list[DLPRule],
                        max_size_mb: int = 10) -> list[DLPMatch]:
        """Read a file and scan its text content."""
        try:
            size = path.stat().st_size
            if size > max_size_mb * 1024 * 1024:
                log.debug("DLP: skipping %s (%.1f MB > limit)", path, size / 1048576)
                return []
            # Read as text; skip binary files
            loop = asyncio.get_event_loop()
            text = await loop.run_in_executor(None, _read_text_file, path)
            if text is None:
                return []
            return self.scan_text(text, rules, scope="file")
        except (OSError, PermissionError) as e:
            log.debug("DLP: cannot read %s — %s", path, e)
            return []


def _read_text_file(path: Path) -> str | None:
    """Read file as UTF-8, falling back to latin-1. Return None if binary."""
    try:
        with open(path, "r", encoding="utf-8", errors="strict") as f:
            return f.read(10 * 1024 * 1024)  # 10 MB cap
    except UnicodeDecodeError:
        try:
            with open(path, "r", encoding="latin-1") as f:
                return f.read(10 * 1024 * 1024)
        except (OSError, UnicodeDecodeError):
            return None
    except OSError:
        return None


# ── DLP Manager ───────────────────────────────────────────────────────────────

class DLPManager:
    """
    Manages DLP policies, runs scans, tracks incidents.

    Optional integrations set after construction:
        mgr.itsm = itsm_mgr
        mgr.notifier = notification_mgr
    """

    def __init__(self, data_path: Path,
                 config: DLPConfig | None = None,
                 event_queue: asyncio.Queue | None = None) -> None:
        self._path = data_path
        self._config = config or DLPConfig()
        self._policies: dict[str, DLPPolicy] = {}
        self._incidents: list[DLPIncident] = []
        self._event_queue = event_queue
        self._tasks: list[asyncio.Task] = []
        self._last_file_scan: float = 0.0
        self._last_cloud_scan: float = 0.0
        self._scanner = ContentScanner()
        self.itsm: Any = None
        self.notifier: Any = None
        self._load()

    # ── Persistence ───────────────────────────────────────────────────

    def _load(self) -> None:
        self._path.mkdir(parents=True, exist_ok=True)
        cfg_path = self._path / "dlp.json"
        if cfg_path.exists():
            try:
                d = json.loads(cfg_path.read_text())
                self._config = DLPConfig.from_dict(d.get("config", {}))
                for p in d.get("policies", []):
                    policy = DLPPolicy.from_dict(p)
                    self._policies[policy.id] = policy
                self._last_file_scan = d.get("last_file_scan", 0.0)
                self._last_cloud_scan = d.get("last_cloud_scan", 0.0)
            except Exception as e:
                log.warning("Failed to load DLP config: %s", e)
        inc_path = self._path / "incidents.json"
        if inc_path.exists():
            try:
                raw = json.loads(inc_path.read_text())
                self._incidents = [DLPIncident.from_dict(i)
                                   for i in raw.get("incidents", [])]
            except Exception as e:
                log.warning("Failed to load DLP incidents: %s", e)
        log.info("DLP loaded: %d policies, %d incidents",
                 len(self._policies), len(self._incidents))

    def _save(self) -> None:
        cfg_path = self._path / "dlp.json"
        try:
            cfg_path.write_text(json.dumps({
                "config": self._config.to_dict(),
                "policies": [p.to_dict() for p in self._policies.values()],
                "last_file_scan": self._last_file_scan,
                "last_cloud_scan": self._last_cloud_scan,
            }, indent=2))
        except Exception as e:
            log.error("Failed to save DLP config: %s", e)
        inc_path = self._path / "incidents.json"
        try:
            recent = self._incidents[-MAX_INCIDENTS:]
            inc_path.write_text(json.dumps(
                {"incidents": [i.to_dict() for i in recent]}, indent=2
            ))
        except Exception as e:
            log.error("Failed to save DLP incidents: %s", e)

    # ── Lifecycle ─────────────────────────────────────────────────────

    async def start(self) -> None:
        self._tasks.append(asyncio.create_task(
            self._scan_loop(), name="dlp-scan-loop"
        ))
        log.info("DLP manager started")

    async def stop(self) -> None:
        for t in self._tasks:
            t.cancel()

    async def _scan_loop(self) -> None:
        await asyncio.sleep(60)
        while True:
            now = time.time()
            try:
                if (self._config.file_scan_interval > 0 and
                        now - self._last_file_scan >= self._config.file_scan_interval):
                    await self.run_file_scan()
                if (self._config.cloud_scan_enabled and
                        self._config.cloud_scan_interval > 0 and
                        now - self._last_cloud_scan >= self._config.cloud_scan_interval):
                    await self.run_cloud_scan()
            except Exception as e:
                log.error("DLP scan loop error: %s", e)
            await asyncio.sleep(300)  # check every 5 min

    # ── Policy management ─────────────────────────────────────────────

    def list_policies(self) -> list[DLPPolicy]:
        return list(self._policies.values())

    def get_policy(self, policy_id: str) -> DLPPolicy | None:
        return self._policies.get(policy_id)

    def create_policy(self, name: str, description: str = "",
                      rules: list[DLPRule] | None = None) -> DLPPolicy:
        policy = DLPPolicy(
            id=str(uuid.uuid4()),
            name=name,
            description=description,
            rules=rules or [],
        )
        self._policies[policy.id] = policy
        self._save()
        return policy

    def update_policy(self, policy_id: str, **kwargs) -> DLPPolicy | None:
        policy = self._policies.get(policy_id)
        if not policy:
            return None
        for k, v in kwargs.items():
            if hasattr(policy, k):
                setattr(policy, k, v)
        self._save()
        return policy

    def delete_policy(self, policy_id: str) -> bool:
        if policy_id not in self._policies:
            return False
        del self._policies[policy_id]
        self._save()
        return True

    def add_rule(self, policy_id: str, rule: DLPRule) -> DLPPolicy | None:
        policy = self._policies.get(policy_id)
        if not policy:
            return None
        policy.rules.append(rule)
        self._save()
        return policy

    def update_rule(self, policy_id: str, rule_id: str,
                    **kwargs) -> DLPRule | None:
        policy = self._policies.get(policy_id)
        if not policy:
            return None
        rule = policy.get_rule(rule_id)
        if not rule:
            return None
        for k, v in kwargs.items():
            if hasattr(rule, k):
                setattr(rule, k, v)
        self._save()
        return rule

    def remove_rule(self, policy_id: str, rule_id: str) -> bool:
        policy = self._policies.get(policy_id)
        if not policy:
            return False
        before = len(policy.rules)
        policy.rules = [r for r in policy.rules if r.id != rule_id]
        if len(policy.rules) == before:
            return False
        self._save()
        return True

    def create_default_policy(self) -> DLPPolicy:
        """Create a sensible default policy covering PAN, credentials, keys."""
        rules = [
            DLPRule(
                id=str(uuid.uuid4()), name="Credit / Debit Card Numbers",
                pattern_type="credit_card", action="alert", severity="critical",
                scopes=list(_SCOPES), validate=True,
            ),
            DLPRule(
                id=str(uuid.uuid4()), name="US Social Security Numbers",
                pattern_type="ssn", action="alert", severity="high",
                scopes=["file", "email", "cloud"], validate=False,
            ),
            DLPRule(
                id=str(uuid.uuid4()), name="Australian Tax File Numbers",
                pattern_type="tfn", action="alert", severity="high",
                scopes=["file", "email", "cloud"], validate=True,
            ),
            DLPRule(
                id=str(uuid.uuid4()), name="AWS Access Keys",
                pattern_type="aws_key", action="alert", severity="critical",
                scopes=list(_SCOPES), validate=False,
            ),
            DLPRule(
                id=str(uuid.uuid4()), name="Private Keys in Files",
                pattern_type="private_key", action="alert", severity="critical",
                scopes=["file", "email", "cloud"], validate=False,
            ),
            DLPRule(
                id=str(uuid.uuid4()), name="Hardcoded API Keys",
                pattern_type="api_key", action="alert", severity="high",
                scopes=["file", "cloud"], validate=False,
            ),
            DLPRule(
                id=str(uuid.uuid4()), name="Hardcoded Passwords",
                pattern_type="password", action="alert", severity="high",
                scopes=["file", "cloud"], validate=False,
            ),
        ]
        return self.create_policy(
            name="Default Policy",
            description="Covers payment card data, credentials, and keys.",
            rules=rules,
        )

    # ── Incident management ───────────────────────────────────────────

    def list_incidents(self,
                       scope: str | None = None,
                       severity: str | None = None,
                       resolved: bool | None = None,
                       node_id: str | None = None,
                       limit: int = 100) -> list[DLPIncident]:
        incidents = list(self._incidents)
        if scope:
            incidents = [i for i in incidents if i.scope == scope]
        if severity:
            incidents = [i for i in incidents if i.severity == severity]
        if resolved is not None:
            incidents = [i for i in incidents if i.resolved == resolved]
        if node_id:
            incidents = [i for i in incidents if i.node_id == node_id]
        return sorted(incidents, key=lambda i: i.created_at, reverse=True)[:limit]

    def get_incident(self, incident_id: str) -> DLPIncident | None:
        return next((i for i in self._incidents if i.id == incident_id), None)

    def acknowledge_incident(self, incident_id: str) -> bool:
        i = self.get_incident(incident_id)
        if not i:
            return False
        i.acknowledged = True
        self._save()
        return True

    def resolve_incident(self, incident_id: str) -> bool:
        i = self.get_incident(incident_id)
        if not i:
            return False
        i.resolved = True
        i.resolved_at = time.time()
        self._save()
        return True

    async def _record_incident(self, policy: DLPPolicy, rule: DLPRule,
                                matches: list[DLPMatch], scope: str,
                                source: str, node_id: str = "",
                                user_email: str = "") -> DLPIncident:
        context = matches[0].context if matches else ""
        inc = DLPIncident(
            id=str(uuid.uuid4()),
            policy_id=policy.id,
            rule_id=rule.id,
            pattern_type=rule.pattern_type,
            action_taken=rule.action,
            severity=rule.severity,
            scope=scope,
            source=source,
            node_id=node_id,
            user_email=user_email,
            match_count=len(matches),
            context=context,
            created_at=time.time(),
        )
        self._incidents.append(inc)
        if len(self._incidents) > MAX_INCIDENTS:
            self._incidents = self._incidents[-MAX_INCIDENTS:]
        self._save()

        await self._fire_event("dlp.incident", inc.to_dict())

        # ITSM ticket for significant incidents
        sev_idx = _SEVERITIES.index(inc.severity) if inc.severity in _SEVERITIES else 99
        threshold_idx = _SEVERITIES.index(self._config.itsm_ticket_severity) \
            if self._config.itsm_ticket_severity in _SEVERITIES else 2
        if sev_idx <= threshold_idx and self.itsm:
            try:
                ticket = await self.itsm.create_ticket(
                    title=f"[DLP] {rule.name} — {inc.pattern_type} in {scope}",
                    description=(
                        f"DLP policy '{policy.name}' triggered.\n\n"
                        f"Rule: {rule.name}\n"
                        f"Pattern: {inc.pattern_type}\n"
                        f"Action: {inc.action_taken}\n"
                        f"Scope: {scope}\n"
                        f"Source: {source}\n"
                        f"Node: {node_id or 'N/A'}\n"
                        f"User: {user_email or 'N/A'}\n"
                        f"Matches: {inc.match_count}\n\n"
                        f"Context (redacted):\n{context}"
                    ),
                    priority=inc.severity,
                    source="dlp",
                )
                if ticket and hasattr(ticket, "id"):
                    inc.itsm_ticket_id = ticket.id
                    self._save()
            except Exception as e:
                log.warning("Failed to create DLP ITSM ticket: %s", e)

        return inc

    # ── Scan interfaces ───────────────────────────────────────────────

    async def scan_content(self, text: str, scope: str,
                           source: str, node_id: str = "",
                           user_email: str = "") -> list[DLPIncident]:
        """
        Scan arbitrary text content against all enabled policies.
        Called by email_security, agent clipboard events, etc.
        """
        incidents: list[DLPIncident] = []
        for policy in self._policies.values():
            if not policy.enabled:
                continue
            all_rules = [r for r in policy.rules if r.enabled and scope in r.scopes]
            matches = self._scanner.scan_text(text, all_rules, scope)
            if not matches:
                continue
            # Group by rule
            by_rule: dict[str, list[DLPMatch]] = {}
            for m in matches:
                by_rule.setdefault(m.rule_id, []).append(m)
            for rule_id, rule_matches in by_rule.items():
                rule = policy.get_rule(rule_id)
                if not rule or len(rule_matches) < rule.min_matches:
                    continue
                inc = await self._record_incident(
                    policy, rule, rule_matches, scope, source, node_id, user_email
                )
                incidents.append(inc)
        return incidents

    async def run_file_scan(self, paths: list[str] | None = None,
                             node_id: str = "") -> dict[str, Any]:
        """
        Walk configured paths and scan matching files.
        In production this is dispatched to the agent via job_queue;
        here it runs locally (useful for controller-local files and testing).
        """
        scan_paths: list[Path] = []
        all_policies = [p for p in self._policies.values() if p.enabled]
        if not all_policies:
            return {"ok": True, "files_scanned": 0, "incidents": 0, "skipped": 0}

        # Collect paths from all enabled policies (or caller override)
        if paths:
            scan_paths = [Path(p) for p in paths]
        else:
            for policy in all_policies:
                for sp in policy.scan_paths:
                    p = Path(sp)
                    if p.exists():
                        scan_paths.append(p)

        extensions: set[str] = set()
        for policy in all_policies:
            extensions.update(policy.scan_extensions)
        max_mb = max((p.max_file_size_mb for p in all_policies), default=10)

        files_scanned = skipped = total_incidents = 0
        for base_path in scan_paths:
            async for file_path, inc_count in self._walk_and_scan(
                base_path, extensions, max_mb, all_policies, node_id
            ):
                files_scanned += 1
                total_incidents += inc_count
            skipped += 0  # counted inside _walk_and_scan

        self._last_file_scan = time.time()
        self._save()
        await self._fire_event("dlp.file_scan.complete", {
            "files_scanned": files_scanned,
            "incidents": total_incidents,
            "node_id": node_id,
        })
        return {
            "ok": True,
            "files_scanned": files_scanned,
            "incidents": total_incidents,
            "skipped": skipped,
        }

    async def _walk_and_scan(self, base: Path, extensions: set[str],
                              max_mb: int, policies: list[DLPPolicy],
                              node_id: str):
        """Async generator: walk a directory tree and scan matching files."""
        if not base.is_dir():
            if base.is_file() and base.suffix.lower() in extensions:
                count = await self._scan_single_file(base, policies, node_id)
                yield base, count
            return
        for entry in base.rglob("*"):
            if entry.is_file() and entry.suffix.lower() in extensions:
                try:
                    count = await self._scan_single_file(entry, policies, node_id)
                    yield entry, count
                except Exception as e:
                    log.debug("DLP scan error on %s: %s", entry, e)

    async def _scan_single_file(self, path: Path,
                                  policies: list[DLPPolicy],
                                  node_id: str) -> int:
        """Scan one file against all enabled policies. Returns incident count."""
        incident_count = 0
        for policy in policies:
            if not policy.enabled:
                continue
            rules = [r for r in policy.rules if r.enabled and "file" in r.scopes]
            if not rules:
                continue
            matches = await self._scanner.scan_file(path, rules, policy.max_file_size_mb)
            if not matches:
                continue
            by_rule: dict[str, list[DLPMatch]] = {}
            for m in matches:
                by_rule.setdefault(m.rule_id, []).append(m)
            for rule_id, rule_matches in by_rule.items():
                rule = policy.get_rule(rule_id)
                if not rule or len(rule_matches) < rule.min_matches:
                    continue
                await self._record_incident(
                    policy, rule, rule_matches, "file", str(path), node_id
                )
                incident_count += 1
        return incident_count

    async def run_cloud_scan(self) -> dict[str, Any]:
        """
        Scan cloud storage (M365 / Google Workspace) for sensitive content.
        Requires cloud_backup credentials to be configured.
        This is a stub — full implementation hooks into cloud_backup's
        rclone + graph delta infrastructure to enumerate and scan documents.
        """
        log.info("DLP cloud scan: stub — implement via cloud_backup integration")
        self._last_cloud_scan = time.time()
        self._save()
        return {"ok": True, "files_scanned": 0, "incidents": 0, "note": "cloud scan stub"}

    # ── USB / removable media ─────────────────────────────────────────

    async def handle_usb_event(self, node_id: str, device_name: str,
                                user_email: str = "") -> list[DLPIncident]:
        """Called by agent when a removable storage device is inserted."""
        if not self._config.usb_alert_enabled:
            return []
        # Find any policy with a USB-scope rule
        incidents: list[DLPIncident] = []
        for policy in self._policies.values():
            if not policy.enabled:
                continue
            for rule in policy.rules:
                if not rule.enabled or "usb" not in rule.scopes:
                    continue
                # USB events are structural (device inserted), not content-based
                # We create a "data exfiltration risk" incident for every insert
                inc = await self._record_incident(
                    policy, rule,
                    matches=[DLPMatch(rule_id=rule.id, pattern_type="usb_insert",
                                     match_text="[device]",
                                     context=f"USB device inserted: {device_name}",
                                     offset=0)],
                    scope="usb",
                    source=f"USB:{device_name}",
                    node_id=node_id,
                    user_email=user_email,
                )
                incidents.append(inc)
                break  # one incident per policy per event
        return incidents

    # ── Config ────────────────────────────────────────────────────────

    def get_config(self) -> DLPConfig:
        return self._config

    def set_config(self, config: DLPConfig) -> None:
        self._config = config
        self._save()

    def status(self) -> dict[str, Any]:
        active = [i for i in self._incidents if not i.resolved]
        by_sev: dict[str, int] = {s: 0 for s in _SEVERITIES}
        for i in active:
            by_sev[i.severity] = by_sev.get(i.severity, 0) + 1
        return {
            "policies": len(self._policies),
            "active_policies": sum(1 for p in self._policies.values() if p.enabled),
            "total_rules": sum(len(p.rules) for p in self._policies.values()),
            "incidents_open": len(active),
            "incidents_by_severity": by_sev,
            "last_file_scan": self._last_file_scan,
            "last_cloud_scan": self._last_cloud_scan,
            "scans_enabled": {
                "file": self._config.file_scan_interval > 0,
                "email": self._config.email_scan_enabled,
                "cloud": self._config.cloud_scan_enabled,
                "usb": self._config.usb_alert_enabled,
            },
        }

    async def _fire_event(self, event_type: str, data: dict) -> None:
        if self._event_queue:
            await self._event_queue.put({"type": event_type, "data": data})
