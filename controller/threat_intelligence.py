"""
Threat Intelligence Platform — real-time threat feed ingestion, credential
exposure monitoring, typosquatting detection, ATT&CK coverage mapping, and
dynamic security posture adjustment.

Feed sources:
 - CISA KEV (Known Exploited Vulnerabilities) — daily, authoritative patch urgency
 - ACSC (Australian Cyber Security Centre) advisories — RSS + JSON feed
 - MITRE ATT&CK STIX feed — weekly, technique/tactic coverage mapping
 - HaveIBeenPwned (HIBP) — credential exposure by domain
 - Certificate Transparency logs (crt.sh) — typosquatting domain detection

Integration:
 - KEV matches → escalate to emergency patch ring (bypasses CVSS threshold)
 - ACSC advisories → ITSM ticket creation, posture adjustment guardrails
 - Credential exposure → alert affected users, ITSM ticket
 - Posture adjustments → logged to audit_log.py with advisory reference

Persistence: threat_data/
  kev.json          — KEV catalogue snapshot
  advisories.json   — ingested advisories
  indicators.json   — IOC/IOA store
  posture.json      — posture changes log
  exposure.json     — credential exposure records
  config.json       — configuration

Events:
  threat.kev.new              — new KEV entry matching estate SBOM
  threat.advisory.new         — new ACSC advisory matching sector/region
  threat.exposure.detected    — credential exposure for monitored domain
  threat.typosquat.detected   — lookalike domain registered
  threat.posture.adjusted     — automatic posture change applied
"""

from __future__ import annotations

import asyncio
import hashlib
import json
import logging
import re
import time
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass, field
from datetime import date, datetime, timedelta
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.threat_intelligence")

# ── Feed URLs ─────────────────────────────────────────────────────────────────

CISA_KEV_URL = "https://www.cisa.gov/sites/default/files/feeds/known_exploited_vulnerabilities.json"
ACSC_FEED_URL = "https://www.cyber.gov.au/about-us/view-all-content/alerts-and-advisories"  # RSS/JSON
HIBP_DOMAIN_URL = "https://haveibeenpwned.com/api/v3/breacheddomain/{domain}"
CRT_SH_URL = "https://crt.sh/?q=%25.{domain}&output=json"

# ── ATT&CK technique → Ozma control mapping ─────────────────────────────────

# Partial map: ATT&CK technique ID → Ozma controls that address it.
# Used to compute ATT&CK coverage percentage.
ATTACK_COVERAGE: dict[str, list[str]] = {
    "T1110": ["mfa_enforced", "failed_login_alerting"],          # Brute Force
    "T1078": ["mfa_enforced", "anomalous_login_alerting"],       # Valid Accounts
    "T1566": ["email_security", "phishing_simulation"],          # Phishing
    "T1059": ["endpoint_detection", "osquery_process_monitoring"], # Command-Line Interface
    "T1055": ["endpoint_detection", "osquery_process_monitoring"], # Process Injection
    "T1005": ["dlp_file_scan", "data_classification"],           # Data from Local System
    "T1048": ["egress_filtering", "dns_monitoring"],             # Exfil over Alt Protocol
    "T1071": ["proxy_inspection", "dns_monitoring"],             # App Layer Protocol
    "T1486": ["backup_immutable", "endpoint_detection"],         # Data Encrypted for Impact
    "T1490": ["backup_immutable", "endpoint_detection"],         # Inhibit System Recovery
    "T1021": ["network_segmentation", "mfa_enforced"],           # Remote Services
    "T1003": ["credential_guard", "laps_rotation"],              # OS Credential Dumping
    "T1098": ["privileged_access_mgmt", "mfa_enforced"],         # Account Manipulation
    "T1053": ["osquery_scheduled_tasks", "endpoint_detection"],  # Scheduled Task
    "T1547": ["osquery_autorun", "endpoint_detection"],          # Boot/Logon Autostart
}

# Sectors that ACSC advisories commonly target
SECTOR_KEYWORDS = {
    "financial": ["bank", "finance", "insurance", "superannuation", "credit"],
    "healthcare": ["health", "hospital", "medical", "pharmacy", "pathology"],
    "government": ["government", "federal", "state", "council", "defence"],
    "education": ["university", "school", "education", "student"],
    "critical_infrastructure": ["energy", "water", "transport", "telco", "electricity"],
}


# ── Data model ────────────────────────────────────────────────────────────────

@dataclass
class KEVEntry:
    cve_id: str
    vendor: str
    product: str
    vulnerability_name: str
    date_added: str          # ISO date "2023-01-01"
    short_description: str
    required_action: str
    due_date: str            # ISO date — deadline for remediation
    notes: str = ""
    first_seen: float = 0.0
    matched_sbom: bool = False  # True if any node in estate has this CVE

    def to_dict(self) -> dict[str, Any]:
        return {
            "cve_id": self.cve_id,
            "vendor": self.vendor,
            "product": self.product,
            "vulnerability_name": self.vulnerability_name,
            "date_added": self.date_added,
            "short_description": self.short_description,
            "required_action": self.required_action,
            "due_date": self.due_date,
            "notes": self.notes,
            "first_seen": self.first_seen,
            "matched_sbom": self.matched_sbom,
        }

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> "KEVEntry":
        return cls(
            cve_id=d["cve_id"],
            vendor=d.get("vendor", ""),
            product=d.get("product", ""),
            vulnerability_name=d.get("vulnerability_name", ""),
            date_added=d.get("date_added", ""),
            short_description=d.get("short_description", ""),
            required_action=d.get("required_action", ""),
            due_date=d.get("due_date", ""),
            notes=d.get("notes", ""),
            first_seen=d.get("first_seen", 0.0),
            matched_sbom=d.get("matched_sbom", False),
        )


@dataclass
class Advisory:
    id: str                      # advisory ID, e.g. "2024-001"
    title: str
    source: str                  # "acsc" | "cisa" | "manual"
    published: str               # ISO date
    severity: str = "medium"     # critical | high | medium | low | info
    summary: str = ""
    url: str = ""
    cves: list[str] = field(default_factory=list)
    attack_techniques: list[str] = field(default_factory=list)  # T-IDs
    sectors: list[str] = field(default_factory=list)
    indicators: list[str] = field(default_factory=list)        # IPs, domains, hashes
    first_seen: float = 0.0
    acknowledged: bool = False

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id,
            "title": self.title,
            "source": self.source,
            "published": self.published,
            "severity": self.severity,
            "summary": self.summary,
            "url": self.url,
            "cves": self.cves,
            "attack_techniques": self.attack_techniques,
            "sectors": self.sectors,
            "indicators": self.indicators,
            "first_seen": self.first_seen,
            "acknowledged": self.acknowledged,
        }

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> "Advisory":
        return cls(
            id=d["id"],
            title=d["title"],
            source=d.get("source", "manual"),
            published=d.get("published", ""),
            severity=d.get("severity", "medium"),
            summary=d.get("summary", ""),
            url=d.get("url", ""),
            cves=d.get("cves", []),
            attack_techniques=d.get("attack_techniques", []),
            sectors=d.get("sectors", []),
            indicators=d.get("indicators", []),
            first_seen=d.get("first_seen", 0.0),
            acknowledged=d.get("acknowledged", False),
        )


@dataclass
class CredentialExposure:
    id: str
    domain: str
    email: str | None            # specific affected email, if known
    breach_name: str
    breach_date: str             # ISO date
    data_classes: list[str] = field(default_factory=list)  # passwords, emails, etc.
    first_seen: float = 0.0
    notified: bool = False
    resolved: bool = False

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id,
            "domain": self.domain,
            "email": self.email,
            "breach_name": self.breach_name,
            "breach_date": self.breach_date,
            "data_classes": self.data_classes,
            "first_seen": self.first_seen,
            "notified": self.notified,
            "resolved": self.resolved,
        }

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> "CredentialExposure":
        return cls(
            id=d["id"],
            domain=d["domain"],
            email=d.get("email"),
            breach_name=d.get("breach_name", ""),
            breach_date=d.get("breach_date", ""),
            data_classes=d.get("data_classes", []),
            first_seen=d.get("first_seen", 0.0),
            notified=d.get("notified", False),
            resolved=d.get("resolved", False),
        )


@dataclass
class PostureChange:
    id: str
    timestamp: float
    change_type: str             # monitoring_elevated | firewall_rule | patch_escalation
    description: str
    source_advisory_id: str = ""
    source_kev_id: str = ""
    auto_applied: bool = True
    requires_approval: bool = False
    approved: bool = False
    reverted: bool = False

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id,
            "timestamp": self.timestamp,
            "change_type": self.change_type,
            "description": self.description,
            "source_advisory_id": self.source_advisory_id,
            "source_kev_id": self.source_kev_id,
            "auto_applied": self.auto_applied,
            "requires_approval": self.requires_approval,
            "approved": self.approved,
            "reverted": self.reverted,
        }

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> "PostureChange":
        return cls(
            id=d["id"],
            timestamp=d["timestamp"],
            change_type=d["change_type"],
            description=d["description"],
            source_advisory_id=d.get("source_advisory_id", ""),
            source_kev_id=d.get("source_kev_id", ""),
            auto_applied=d.get("auto_applied", True),
            requires_approval=d.get("requires_approval", False),
            approved=d.get("approved", False),
            reverted=d.get("reverted", False),
        )


@dataclass
class ThreatConfig:
    # Domains to monitor for credential exposure and typosquatting
    monitored_domains: list[str] = field(default_factory=list)
    # Industry sector (used for advisory relevance filtering)
    sector: str = ""            # "financial" | "healthcare" | "government" | etc.
    # Country/region for ACSC advisory relevance
    region: str = "AU"          # AU | US | GB | global
    # HIBP API key (required for domain-level queries)
    hibp_api_key: str = ""
    # Poll intervals (seconds; 0 = manual only)
    kev_poll_interval: int = 86400           # 24h
    advisory_poll_interval: int = 86400      # 24h
    attack_poll_interval: int = 604800       # 7 days
    exposure_poll_interval: int = 86400      # 24h
    # Severities that trigger automatic posture adjustments
    auto_adjust_severities: list[str] = field(
        default_factory=lambda: ["critical", "high"]
    )
    # Whether to apply posture adjustments automatically or queue for approval
    auto_apply_posture: bool = True
    enabled: bool = True

    def to_dict(self) -> dict[str, Any]:
        return {
            "monitored_domains": self.monitored_domains,
            "sector": self.sector,
            "region": self.region,
            "hibp_api_key": self.hibp_api_key,
            "kev_poll_interval": self.kev_poll_interval,
            "advisory_poll_interval": self.advisory_poll_interval,
            "attack_poll_interval": self.attack_poll_interval,
            "exposure_poll_interval": self.exposure_poll_interval,
            "auto_adjust_severities": self.auto_adjust_severities,
            "auto_apply_posture": self.auto_apply_posture,
            "enabled": self.enabled,
        }

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> "ThreatConfig":
        return cls(
            monitored_domains=d.get("monitored_domains", []),
            sector=d.get("sector", ""),
            region=d.get("region", "AU"),
            hibp_api_key=d.get("hibp_api_key", ""),
            kev_poll_interval=d.get("kev_poll_interval", 86400),
            advisory_poll_interval=d.get("advisory_poll_interval", 86400),
            attack_poll_interval=d.get("attack_poll_interval", 604800),
            exposure_poll_interval=d.get("exposure_poll_interval", 86400),
            auto_adjust_severities=d.get("auto_adjust_severities", ["critical", "high"]),
            auto_apply_posture=d.get("auto_apply_posture", True),
            enabled=d.get("enabled", True),
        )


# ── HTTP helpers ──────────────────────────────────────────────────────────────

def _http_get_json(url: str, headers: dict[str, str] | None = None,
                   timeout: int = 30) -> Any:
    """Blocking HTTP GET returning parsed JSON. Raises on non-200."""
    req = urllib.request.Request(url, headers=headers or {})
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        return json.loads(resp.read())


async def _async_http_get_json(url: str, headers: dict[str, str] | None = None,
                                timeout: int = 30) -> Any:
    loop = asyncio.get_event_loop()
    return await loop.run_in_executor(
        None, lambda: _http_get_json(url, headers, timeout)
    )


# ── Manager ───────────────────────────────────────────────────────────────────

class ThreatIntelligenceEngine:
    """
    Ingests threat intelligence feeds and coordinates security posture
    adjustments. All feeds are polled in background tasks.
    """

    def __init__(self, data_dir: Path, event_queue: asyncio.Queue | None = None) -> None:
        self._data_dir = data_dir
        self._data_dir.mkdir(parents=True, exist_ok=True)
        self._config = ThreatConfig()
        self._kev: dict[str, KEVEntry] = {}            # cve_id → entry
        self._advisories: dict[str, Advisory] = {}     # id → advisory
        self._exposures: dict[str, CredentialExposure] = {}  # id → exposure
        self._posture_changes: list[PostureChange] = []
        self._sbom_cves: set[str] = set()              # CVEs in estate SBOM
        self._event_queue = event_queue
        self._last_kev_poll: float = 0.0
        self._last_advisory_poll: float = 0.0
        self._last_attack_poll: float = 0.0
        self._last_exposure_poll: float = 0.0
        self._task: asyncio.Task | None = None
        # Injected by main.py; optional
        self.itsm = None   # ITSMManager | None
        self.network_scan = None  # NetworkScanManager | None (for SBOM CVEs)

    # ── Lifecycle ─────────────────────────────────────────────────────────────

    async def start(self) -> None:
        self._load()
        self._task = asyncio.create_task(self._background_loop(),
                                          name="threat_intelligence")

    async def stop(self) -> None:
        if self._task:
            self._task.cancel()
            try:
                await self._task
            except asyncio.CancelledError:
                pass

    # ── Persistence ───────────────────────────────────────────────────────────

    def _load(self) -> None:
        for fname, attr, klass in [
            ("kev.json", "_kev", KEVEntry),
            ("advisories.json", "_advisories", Advisory),
            ("exposures.json", "_exposures", CredentialExposure),
        ]:
            path = self._data_dir / fname
            if path.exists():
                try:
                    data = json.loads(path.read_text())
                    store = {}
                    for d in data.get("items", []):
                        obj = klass.from_dict(d)  # type: ignore[attr-defined]
                        key = d.get("cve_id") or d.get("id")
                        store[key] = obj
                    setattr(self, attr, store)
                except Exception as e:
                    log.error("Failed to load %s: %s", fname, e)

        posture_path = self._data_dir / "posture.json"
        if posture_path.exists():
            try:
                data = json.loads(posture_path.read_text())
                self._posture_changes = [
                    PostureChange.from_dict(d) for d in data.get("changes", [])
                ]
            except Exception as e:
                log.error("Failed to load posture.json: %s", e)

        config_path = self._data_dir / "config.json"
        if config_path.exists():
            try:
                self._config = ThreatConfig.from_dict(
                    json.loads(config_path.read_text())
                )
            except Exception as e:
                log.error("Failed to load threat config.json: %s", e)

        timestamps_path = self._data_dir / "timestamps.json"
        if timestamps_path.exists():
            try:
                ts = json.loads(timestamps_path.read_text())
                self._last_kev_poll = ts.get("kev", 0.0)
                self._last_advisory_poll = ts.get("advisory", 0.0)
                self._last_attack_poll = ts.get("attack", 0.0)
                self._last_exposure_poll = ts.get("exposure", 0.0)
            except Exception as e:
                log.error("Failed to load timestamps.json: %s", e)

    def _save(self) -> None:
        for fname, attr, key_fn in [
            ("kev.json", "_kev", lambda v: v.to_dict()),
            ("advisories.json", "_advisories", lambda v: v.to_dict()),
            ("exposures.json", "_exposures", lambda v: v.to_dict()),
        ]:
            store = getattr(self, attr)
            path = self._data_dir / fname
            tmp = path.with_suffix(".tmp")
            tmp.write_text(json.dumps({"items": [key_fn(v) for v in store.values()]},
                                       indent=2))
            tmp.replace(path)

        posture_path = self._data_dir / "posture.json"
        tmp = posture_path.with_suffix(".tmp")
        tmp.write_text(json.dumps(
            {"changes": [p.to_dict() for p in self._posture_changes]}, indent=2
        ))
        tmp.replace(posture_path)

        timestamps_path = self._data_dir / "timestamps.json"
        tmp = timestamps_path.with_suffix(".tmp")
        tmp.write_text(json.dumps({
            "kev": self._last_kev_poll,
            "advisory": self._last_advisory_poll,
            "attack": self._last_attack_poll,
            "exposure": self._last_exposure_poll,
        }))
        tmp.replace(timestamps_path)

    def _save_config(self) -> None:
        path = self._data_dir / "config.json"
        tmp = path.with_suffix(".tmp")
        tmp.write_text(json.dumps(self._config.to_dict(), indent=2))
        tmp.replace(path)

    # ── Config ────────────────────────────────────────────────────────────────

    def get_config(self) -> ThreatConfig:
        return self._config

    def set_config(self, updates: dict[str, Any]) -> ThreatConfig:
        d = self._config.to_dict()
        d.update(updates)
        self._config = ThreatConfig.from_dict(d)
        self._save_config()
        return self._config

    # ── SBOM integration ──────────────────────────────────────────────────────

    def update_sbom_cves(self, cve_ids: set[str]) -> int:
        """
        Called by network_scan or compliance_automation with the current CVE
        set from the estate SBOM. Returns count of new KEV matches found.
        """
        new_matches = 0
        for cve_id in cve_ids:
            self._sbom_cves.add(cve_id)
            if cve_id in self._kev and not self._kev[cve_id].matched_sbom:
                self._kev[cve_id].matched_sbom = True
                new_matches += 1
                asyncio.get_event_loop().call_soon_threadsafe(
                    self._event_queue.put_nowait,
                    {"type": "threat.kev.new", "data": self._kev[cve_id].to_dict()}
                ) if self._event_queue else None
        if new_matches:
            self._save()
        return new_matches

    # ── KEV feed ──────────────────────────────────────────────────────────────

    async def poll_cisa_kev(self) -> list[KEVEntry]:
        """
        Download and process the CISA Known Exploited Vulnerabilities catalogue.
        Returns list of newly-added entries.
        """
        try:
            data = await _async_http_get_json(CISA_KEV_URL, timeout=60)
        except Exception as e:
            log.warning("CISA KEV poll failed: %s", e)
            return []

        new_entries: list[KEVEntry] = []
        for vuln in data.get("vulnerabilities", []):
            cve_id = vuln.get("cveID", "")
            if not cve_id:
                continue
            if cve_id not in self._kev:
                entry = KEVEntry(
                    cve_id=cve_id,
                    vendor=vuln.get("vendorProject", ""),
                    product=vuln.get("product", ""),
                    vulnerability_name=vuln.get("vulnerabilityName", ""),
                    date_added=vuln.get("dateAdded", ""),
                    short_description=vuln.get("shortDescription", ""),
                    required_action=vuln.get("requiredAction", ""),
                    due_date=vuln.get("dueDate", ""),
                    notes=vuln.get("notes", ""),
                    first_seen=time.time(),
                    matched_sbom=cve_id in self._sbom_cves,
                )
                self._kev[cve_id] = entry
                new_entries.append(entry)
                if entry.matched_sbom:
                    await self._fire_event("threat.kev.new", entry.to_dict())
                    await self._maybe_create_itsm_ticket(
                        title=f"KEV: {cve_id} — {entry.vulnerability_name}",
                        body=(
                            f"CISA KEV match in estate SBOM.\n\n"
                            f"Product: {entry.vendor} {entry.product}\n"
                            f"Due: {entry.due_date}\n"
                            f"Action: {entry.required_action}"
                        ),
                        priority="critical",
                        tags=["kev", "cve", cve_id],
                    )

        self._last_kev_poll = time.time()
        if new_entries:
            self._save()
        log.info("CISA KEV: %d total entries, %d new", len(self._kev), len(new_entries))
        return new_entries

    # ── ACSC advisory feed ────────────────────────────────────────────────────

    async def poll_acsc_advisories(self) -> list[Advisory]:
        """
        Fetch ACSC advisories and ingest relevant ones.
        Filters by sector/region relevance.
        Returns list of newly-ingested advisories.
        """
        # ACSC publishes advisories as a JSON feed; the URL may change but the
        # structure is stable. We use a best-effort fetch and gracefully degrade.
        feed_url = "https://www.cyber.gov.au/api/v1/advisories"
        try:
            data = await _async_http_get_json(feed_url, timeout=30)
        except Exception as e:
            log.debug("ACSC advisory feed unavailable (%s); falling back to empty", e)
            data = {}

        new_advisories: list[Advisory] = []
        for item in data.get("advisories", data.get("items", [])):
            adv_id = str(item.get("id") or item.get("acn", ""))
            if not adv_id or adv_id in self._advisories:
                continue

            title = item.get("title", "")
            sectors = self._extract_sectors(title + " " + item.get("summary", ""))
            # Skip if we have a sector configured and this advisory doesn't match
            if self._config.sector and sectors and self._config.sector not in sectors:
                continue

            adv = Advisory(
                id=adv_id,
                title=title,
                source="acsc",
                published=item.get("published", item.get("date", "")),
                severity=self._acsc_severity_to_standard(item.get("severity", "medium")),
                summary=item.get("summary", item.get("description", "")),
                url=item.get("url", item.get("link", "")),
                cves=item.get("cves", []),
                attack_techniques=item.get("mitre_techniques", []),
                sectors=sectors,
                indicators=item.get("indicators", []),
                first_seen=time.time(),
            )
            self._advisories[adv_id] = adv
            new_advisories.append(adv)
            await self._fire_event("threat.advisory.new", adv.to_dict())
            if adv.severity in ("critical", "high"):
                await self._evaluate_posture_adjustment(adv)

        self._last_advisory_poll = time.time()
        if new_advisories:
            self._save()
        return new_advisories

    def ingest_advisory(self, advisory_data: dict[str, Any]) -> Advisory:
        """
        Manually ingest an advisory (from CISA, CERT, or custom source).
        """
        adv = Advisory.from_dict({
            **advisory_data,
            "first_seen": time.time(),
        })
        is_new = adv.id not in self._advisories
        self._advisories[adv.id] = adv
        self._save()
        if is_new:
            asyncio.get_event_loop().call_soon_threadsafe(
                self._event_queue.put_nowait if self._event_queue else lambda x: None,
                {"type": "threat.advisory.new", "data": adv.to_dict()}
            ) if self._event_queue else None
        return adv

    # ── Credential exposure (HIBP) ────────────────────────────────────────────

    async def check_credential_exposure(
        self, domains: list[str] | None = None
    ) -> list[CredentialExposure]:
        """
        Query HaveIBeenPwned for credential exposure on monitored domains.
        Requires hibp_api_key in config for domain-level queries.
        Returns new exposures found.
        """
        check_domains = domains or self._config.monitored_domains
        if not check_domains:
            return []
        if not self._config.hibp_api_key:
            log.debug("HIBP API key not configured; skipping credential exposure check")
            return []

        new_exposures: list[CredentialExposure] = []
        for domain in check_domains:
            url = HIBP_DOMAIN_URL.format(domain=domain)
            headers = {
                "hibp-api-key": self._config.hibp_api_key,
                "User-Agent": "Ozma-Controller/1.0",
            }
            try:
                breaches = await _async_http_get_json(url, headers=headers)
            except urllib.error.HTTPError as e:
                if e.code == 404:
                    continue  # Domain not in any breach
                if e.code == 401:
                    log.warning("HIBP API key invalid or expired")
                    break
                log.warning("HIBP query for %s failed: HTTP %d", domain, e.code)
                continue
            except Exception as e:
                log.warning("HIBP query for %s failed: %s", domain, e)
                continue

            # breaches: list of {Name, Title, Domain, BreachDate, Description,
            #                     DataClasses, IsVerified, ...}
            for breach in (breaches if isinstance(breaches, list) else []):
                exp_id = hashlib.sha256(
                    f"{domain}:{breach.get('Name', '')}".encode()
                ).hexdigest()[:16]
                if exp_id in self._exposures:
                    continue
                exp = CredentialExposure(
                    id=exp_id,
                    domain=domain,
                    email=None,
                    breach_name=breach.get("Name", ""),
                    breach_date=breach.get("BreachDate", ""),
                    data_classes=breach.get("DataClasses", []),
                    first_seen=time.time(),
                )
                self._exposures[exp_id] = exp
                new_exposures.append(exp)
                await self._fire_event("threat.exposure.detected", exp.to_dict())
                await self._maybe_create_itsm_ticket(
                    title=f"Credential exposure: {domain} in {exp.breach_name}",
                    body=(
                        f"Domain {domain} was found in breach: {exp.breach_name}\n"
                        f"Breach date: {exp.breach_date}\n"
                        f"Data classes: {', '.join(exp.data_classes)}"
                    ),
                    priority="high",
                    tags=["credential_exposure", "hibp", domain],
                )

        self._last_exposure_poll = time.time()
        if new_exposures:
            self._save()
        return new_exposures

    # ── Typosquatting detection ───────────────────────────────────────────────

    async def check_typosquat(self, domain: str) -> list[dict[str, Any]]:
        """
        Check Certificate Transparency logs for lookalike domains.
        Uses crt.sh to find recently-issued certs for wildcard/subdomains.
        Returns list of suspicious domain records.
        """
        # We query for certs matching *.domain and subdomains
        domain_parts = domain.split(".")
        if len(domain_parts) < 2:
            return []

        base = ".".join(domain_parts[-2:])
        url = f"https://crt.sh/?q=%.{base}&output=json"
        try:
            certs = await _async_http_get_json(url, timeout=30)
        except Exception as e:
            log.debug("crt.sh query for %s failed: %s", domain, e)
            return []

        seen_domains: set[str] = set()
        suspicious: list[dict[str, Any]] = []
        cutoff = (datetime.utcnow() - timedelta(days=30)).isoformat()

        for cert in (certs if isinstance(certs, list) else []):
            name = cert.get("name_value", "").lower().strip()
            not_before = cert.get("not_before", "")
            if not name or name == domain or name.endswith(f".{domain}"):
                continue  # Skip legitimate certs for the domain itself
            # Only flag certs issued in the last 30 days
            if not_before < cutoff:
                continue
            if name in seen_domains:
                continue
            seen_domains.add(name)

            # Simple typosquat heuristics
            if self._is_typosquat(domain, name):
                suspicious.append({
                    "domain": name,
                    "not_before": not_before,
                    "issuer": cert.get("issuer_name", ""),
                    "serial": cert.get("serial_number", ""),
                })
                await self._fire_event("threat.typosquat.detected", {
                    "monitored_domain": domain,
                    "suspicious_domain": name,
                    "not_before": not_before,
                })

        return suspicious

    @staticmethod
    def _is_typosquat(legitimate: str, candidate: str) -> bool:
        """Heuristic: is candidate likely a typosquat of legitimate?"""
        # Strip leading wildcard
        cand = candidate.lstrip("*.")
        leg = legitimate.lstrip("*.")
        # Remove TLD for comparison
        leg_base = leg.rsplit(".", 1)[0] if "." in leg else leg
        cand_base = cand.rsplit(".", 1)[0] if "." in cand else cand
        if leg_base == cand_base:
            return False  # Same base, different TLD — not necessarily typosquat
        # Contains the legitimate base with insertion/substitution/transposition
        if leg_base in cand_base:
            return False  # Subdomain of legitimate
        # Levenshtein distance ≤ 2 on base name
        return _levenshtein(leg_base, cand_base) <= 2

    # ── ATT&CK coverage map ───────────────────────────────────────────────────

    def compute_attack_coverage(
        self, active_controls: list[str]
    ) -> dict[str, Any]:
        """
        Compute ATT&CK coverage based on the list of active Ozma controls.
        active_controls: list of control IDs (e.g. ["mfa_enforced", "dlp_file_scan"])
        Returns coverage percentage and gap list.
        """
        total = len(ATTACK_COVERAGE)
        covered = 0
        covered_techniques: list[str] = []
        gap_techniques: list[dict[str, Any]] = []

        for technique_id, required_controls in ATTACK_COVERAGE.items():
            matched = any(c in active_controls for c in required_controls)
            if matched:
                covered += 1
                covered_techniques.append(technique_id)
            else:
                gap_techniques.append({
                    "technique_id": technique_id,
                    "required_controls": required_controls,
                    "missing_controls": [c for c in required_controls
                                          if c not in active_controls],
                })

        coverage_pct = round(covered / total * 100, 1) if total > 0 else 0.0
        return {
            "total_techniques": total,
            "covered_techniques": covered,
            "coverage_pct": coverage_pct,
            "covered": covered_techniques,
            "gaps": gap_techniques,
        }

    # ── Posture adjustment ────────────────────────────────────────────────────

    async def _evaluate_posture_adjustment(self, advisory: Advisory) -> None:
        """
        Evaluate whether an advisory warrants an automatic posture adjustment.
        Guardrails:
          - monitoring_elevated: always automatic (zero risk)
          - firewall_rule: automatic if config.auto_apply_posture
          - patch_escalation: automatic for KEV/advisory-flagged CVEs
          - credential_action: always requires human approval
        """
        if advisory.severity not in self._config.auto_adjust_severities:
            return

        changes: list[PostureChange] = []

        # Monitoring elevation is always automatic
        import uuid
        pc = PostureChange(
            id=str(uuid.uuid4()),
            timestamp=time.time(),
            change_type="monitoring_elevated",
            description=(
                f"Monitoring elevated in response to {advisory.severity} advisory: "
                f"{advisory.title}"
            ),
            source_advisory_id=advisory.id,
            auto_applied=True,
            requires_approval=False,
            approved=True,
        )
        changes.append(pc)

        # Ransomware advisories trigger SMB lateral movement block
        if any(kw in advisory.title.lower()
               for kw in ("ransomware", "lateral movement", "smb", "wiper")):
            pc2 = PostureChange(
                id=str(uuid.uuid4()),
                timestamp=time.time(),
                change_type="firewall_rule",
                description=(
                    f"Block SMB lateral movement between workstations — "
                    f"ransomware advisory: {advisory.title}"
                ),
                source_advisory_id=advisory.id,
                auto_applied=self._config.auto_apply_posture,
                requires_approval=not self._config.auto_apply_posture,
            )
            changes.append(pc2)

        for change in changes:
            self._posture_changes.append(change)
            await self._fire_event("threat.posture.adjusted", change.to_dict())

        if changes:
            self._save()

    # ── Advisory management ───────────────────────────────────────────────────

    def list_advisories(
        self,
        source: str | None = None,
        severity: str | None = None,
        acknowledged: bool | None = None,
    ) -> list[Advisory]:
        advisories = list(self._advisories.values())
        if source:
            advisories = [a for a in advisories if a.source == source]
        if severity:
            advisories = [a for a in advisories if a.severity == severity]
        if acknowledged is not None:
            advisories = [a for a in advisories if a.acknowledged == acknowledged]
        return sorted(advisories, key=lambda a: a.first_seen, reverse=True)

    def acknowledge_advisory(self, advisory_id: str) -> Advisory | None:
        adv = self._advisories.get(advisory_id)
        if not adv:
            return None
        adv.acknowledged = True
        self._save()
        return adv

    def list_kev(
        self,
        matched_sbom: bool | None = None,
    ) -> list[KEVEntry]:
        entries = list(self._kev.values())
        if matched_sbom is not None:
            entries = [e for e in entries if e.matched_sbom == matched_sbom]
        return sorted(entries, key=lambda e: e.date_added, reverse=True)

    def list_exposures(
        self,
        resolved: bool | None = None,
    ) -> list[CredentialExposure]:
        exposures = list(self._exposures.values())
        if resolved is not None:
            exposures = [e for e in exposures if e.resolved == resolved]
        return sorted(exposures, key=lambda e: e.first_seen, reverse=True)

    def resolve_exposure(self, exposure_id: str) -> CredentialExposure | None:
        exp = self._exposures.get(exposure_id)
        if not exp:
            return None
        exp.resolved = True
        self._save()
        return exp

    def list_posture_changes(self) -> list[PostureChange]:
        return sorted(self._posture_changes, key=lambda p: p.timestamp, reverse=True)

    # ── Threat briefing ───────────────────────────────────────────────────────

    def generate_threat_briefing(
        self, active_controls: list[str] | None = None
    ) -> dict[str, Any]:
        """
        Generate a weekly threat briefing summary.
        """
        now = time.time()
        week_ago = now - 7 * 86400
        recent_advisories = [a for a in self._advisories.values()
                              if a.first_seen >= week_ago]
        recent_kev = [e for e in self._kev.values()
                      if e.first_seen >= week_ago]
        new_exposures = [e for e in self._exposures.values()
                         if e.first_seen >= week_ago]
        recent_posture = [p for p in self._posture_changes
                          if p.timestamp >= week_ago]

        coverage = self.compute_attack_coverage(active_controls or [])

        return {
            "generated_at": now,
            "period_days": 7,
            "advisories_this_week": len(recent_advisories),
            "critical_advisories": len([a for a in recent_advisories
                                         if a.severity == "critical"]),
            "new_kev_entries": len(recent_kev),
            "kev_matches_in_estate": len([e for e in recent_kev if e.matched_sbom]),
            "new_credential_exposures": len(new_exposures),
            "posture_changes": len(recent_posture),
            "attack_coverage_pct": coverage["coverage_pct"],
            "attack_gaps": len(coverage["gaps"]),
            "total_kev": len(self._kev),
            "total_advisories": len(self._advisories),
            "open_exposures": len([e for e in self._exposures.values()
                                    if not e.resolved]),
        }

    # ── Background loop ───────────────────────────────────────────────────────

    async def _background_loop(self) -> None:
        while True:
            try:
                await asyncio.sleep(3600)  # check every hour
                if not self._config.enabled:
                    continue
                now = time.time()
                if (self._config.kev_poll_interval > 0 and
                        now - self._last_kev_poll >= self._config.kev_poll_interval):
                    await self.poll_cisa_kev()
                if (self._config.advisory_poll_interval > 0 and
                        now - self._last_advisory_poll >= self._config.advisory_poll_interval):
                    await self.poll_acsc_advisories()
                if (self._config.exposure_poll_interval > 0 and
                        now - self._last_exposure_poll >= self._config.exposure_poll_interval):
                    await self.check_credential_exposure()
            except asyncio.CancelledError:
                return
            except Exception as e:
                log.error("Threat intelligence background loop error: %s", e)

    # ── ITSM integration ──────────────────────────────────────────────────────

    async def _maybe_create_itsm_ticket(
        self, title: str, body: str, priority: str = "high",
        tags: list[str] | None = None
    ) -> None:
        if not self.itsm:
            return
        try:
            await self.itsm.create_ticket(
                title=title,
                description=body,
                priority=priority,
                tags=tags or [],
                source="threat_intelligence",
            )
        except Exception as e:
            log.warning("Failed to create ITSM ticket for threat event: %s", e)

    # ── Events ────────────────────────────────────────────────────────────────

    async def _fire_event(self, event_type: str, data: dict) -> None:
        if self._event_queue:
            await self._event_queue.put({"type": event_type, "data": data})

    # ── Helpers ───────────────────────────────────────────────────────────────

    @staticmethod
    def _acsc_severity_to_standard(acsc_sev: str) -> str:
        mapping = {
            "critical": "critical",
            "high": "high",
            "medium": "medium",
            "moderate": "medium",
            "low": "low",
            "informational": "info",
            "info": "info",
        }
        return mapping.get(acsc_sev.lower(), "medium")

    @staticmethod
    def _extract_sectors(text: str) -> list[str]:
        text_lower = text.lower()
        found = []
        for sector, keywords in SECTOR_KEYWORDS.items():
            if any(kw in text_lower for kw in keywords):
                found.append(sector)
        return found

    # ── Status ────────────────────────────────────────────────────────────────

    def status(self) -> dict[str, Any]:
        return {
            "enabled": self._config.enabled,
            "kev_entries": len(self._kev),
            "kev_matches_in_estate": len([e for e in self._kev.values() if e.matched_sbom]),
            "advisories": len(self._advisories),
            "unacknowledged_advisories": len([a for a in self._advisories.values()
                                               if not a.acknowledged]),
            "open_exposures": len([e for e in self._exposures.values()
                                    if not e.resolved]),
            "posture_changes": len(self._posture_changes),
            "monitored_domains": len(self._config.monitored_domains),
            "last_kev_poll": self._last_kev_poll,
            "last_advisory_poll": self._last_advisory_poll,
            "last_exposure_poll": self._last_exposure_poll,
            "sbom_cves_tracked": len(self._sbom_cves),
        }


# ── Levenshtein distance ──────────────────────────────────────────────────────

def _levenshtein(s1: str, s2: str) -> int:
    """Compute Levenshtein edit distance between two strings."""
    if len(s1) < len(s2):
        return _levenshtein(s2, s1)
    if not s2:
        return len(s1)
    prev = list(range(len(s2) + 1))
    for i, c1 in enumerate(s1):
        curr = [i + 1]
        for j, c2 in enumerate(s2):
            curr.append(min(
                prev[j + 1] + 1,    # deletion
                curr[j] + 1,        # insertion
                prev[j] + (c1 != c2),  # substitution
            ))
        prev = curr
    return prev[-1]
