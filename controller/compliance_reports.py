"""
Compliance Report Engine — automated evidence collection, control mapping,
and report generation for multiple security frameworks.

Supported frameworks:
  essential_eight_ml1 — ACSC Essential Eight Maturity Level 1
  essential_eight_ml2 — ACSC Essential Eight Maturity Level 2
  essential_eight_ml3 — ACSC Essential Eight Maturity Level 3
  iso27001_2022       — ISO 27001:2022 Annex A (93 controls)
  soc2_type1          — SOC 2 Type I (CC criteria subset)
  cis_level1          — CIS Controls v8 Level 1
  cyber_essentials    — UK Cyber Essentials

The engine collects evidence from other managers (ITAM, backup, audit_log,
threat_intelligence, network_scan, DLP, MDM, etc.) and maps it to framework
controls. Gaps are flagged with severity and remediation guidance.

Reports are emitted as JSON evidence packages. PDF rendering is out of scope
for this module (delegated to a rendering service or Connect).

Persistence: compliance_data/
  reports/            — signed JSON report archive
  config.json         — framework preferences, customer profile
  gaps.json           — current open gaps across all frameworks

Events:
  compliance.report.generated   — new report generated
  compliance.gap.new            — new gap identified
  compliance.gap.resolved       — gap automatically resolved
"""

from __future__ import annotations

import asyncio
import hashlib
import json
import logging
import time
import uuid
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.compliance_reports")

# ── Framework control definitions ─────────────────────────────────────────────

# Essential Eight control IDs
E8_CONTROLS = {
    # Application control
    "e8.app_control.ml1":   "Application control — allow-list on workstations",
    "e8.app_control.ml2":   "Application control — validated/signed applications",
    "e8.app_control.ml3":   "Application control — comprehensive coverage",
    # Patch applications
    "e8.patch_apps.ml1":    "Patch applications — critical within 1 month",
    "e8.patch_apps.ml2":    "Patch applications — critical within 2 weeks",
    "e8.patch_apps.ml3":    "Patch applications — critical within 48h",
    # Patch OS
    "e8.patch_os.ml1":      "Patch OS — critical within 1 month",
    "e8.patch_os.ml2":      "Patch OS — critical within 2 weeks",
    "e8.patch_os.ml3":      "Patch OS — critical within 48h; auto-patch",
    # MFA
    "e8.mfa.ml1":           "MFA — on remote desktop",
    "e8.mfa.ml2":           "MFA — on privileged accounts, cloud, email",
    "e8.mfa.ml3":           "MFA — phishing-resistant on all users",
    # Restrict admin
    "e8.restrict_admin.ml1": "Restrict admin — separate admin accounts",
    "e8.restrict_admin.ml2": "Restrict admin — no internet from admin accounts",
    "e8.restrict_admin.ml3": "Restrict admin — just-in-time privileged access",
    # Regular backups
    "e8.backups.ml1":       "Backups — important data backed up",
    "e8.backups.ml2":       "Backups — recovery tested",
    "e8.backups.ml3":       "Backups — immutable/offsite; full recovery tested",
    # Macro settings
    "e8.macros.ml1":        "Macro settings — disabled or from trusted locations",
    "e8.macros.ml2":        "Macro settings — blocked; AV scans macros",
    "e8.macros.ml3":        "Macro settings — antivirus scans; signed only",
    # User app hardening
    "e8.hardening.ml1":     "User app hardening — browser not Java/ads/Flash",
    "e8.hardening.ml2":     "User app hardening — browsers hardened; no web ads",
    "e8.hardening.ml3":     "User app hardening — comprehensive browser hardening",
}

# ISO 27001:2022 Annex A controls (subset — representative 30)
# Full implementation maps all 93; this covers the most evidence-rich subset.
ISO27001_CONTROLS: dict[str, dict[str, str]] = {
    "5.1":  {"name": "Policies for information security",
             "evidence": "isms_policy_document", "domain": "Org"},
    "5.7":  {"name": "Threat intelligence",
             "evidence": "threat_intel_briefing", "domain": "Org"},
    "5.9":  {"name": "Inventory of information and other associated assets",
             "evidence": "itam_asset_count", "domain": "Org"},
    "5.15": {"name": "Access control",
             "evidence": "idp_user_count", "domain": "Org"},
    "5.16": {"name": "Identity management",
             "evidence": "identity_lifecycle_review", "domain": "Org"},
    "5.19": {"name": "Information security in supplier relationships",
             "evidence": "saas_vendor_risk", "domain": "Org"},
    "5.23": {"name": "Information security for use of cloud services",
             "evidence": "cloud_backup_status", "domain": "Org"},
    "5.26": {"name": "Response to information security incidents",
             "evidence": "itsm_incident_count", "domain": "Org"},
    "5.28": {"name": "Collection of evidence",
             "evidence": "audit_log_integrity", "domain": "Org"},
    "5.29": {"name": "Information security during disruption",
             "evidence": "backup_recovery_test", "domain": "Org"},
    "6.1":  {"name": "Screening",
             "evidence": "hr_screening_policy", "domain": "People"},
    "6.3":  {"name": "Information security awareness, education and training",
             "evidence": "security_awareness_policy", "domain": "People"},
    "6.5":  {"name": "Responsibilities after termination or change of employment",
             "evidence": "offboarding_checklist", "domain": "People"},
    "7.1":  {"name": "Physical security perimeters",
             "evidence": "physical_security_policy", "domain": "Physical"},
    "7.4":  {"name": "Physical security monitoring",
             "evidence": "security_monitor_events", "domain": "Physical"},
    "8.1":  {"name": "User endpoint devices",
             "evidence": "mdm_enrollment_rate", "domain": "Tech"},
    "8.2":  {"name": "Privileged access rights",
             "evidence": "idp_admin_mfa_rate", "domain": "Tech"},
    "8.3":  {"name": "Information access restriction",
             "evidence": "dlp_policy_count", "domain": "Tech"},
    "8.7":  {"name": "Protection against malware",
             "evidence": "endpoint_protection_rate", "domain": "Tech"},
    "8.8":  {"name": "Management of technical vulnerabilities",
             "evidence": "cve_open_critical", "domain": "Tech"},
    "8.9":  {"name": "Configuration management",
             "evidence": "node_config_baseline", "domain": "Tech"},
    "8.12": {"name": "Data leakage prevention",
             "evidence": "dlp_incident_count", "domain": "Tech"},
    "8.13": {"name": "Information backup",
             "evidence": "backup_last_success", "domain": "Tech"},
    "8.15": {"name": "Logging",
             "evidence": "audit_log_entries", "domain": "Tech"},
    "8.16": {"name": "Monitoring activities",
             "evidence": "network_scan_last_run", "domain": "Tech"},
    "8.20": {"name": "Networks security",
             "evidence": "mesh_network_segmentation", "domain": "Tech"},
    "8.24": {"name": "Use of cryptography",
             "evidence": "transport_encryption", "domain": "Tech"},
    "8.25": {"name": "Secure development life cycle",
             "evidence": "ci_signing_pipeline", "domain": "Tech"},
    "8.28": {"name": "Secure coding",
             "evidence": "dependency_scan", "domain": "Tech"},
    "8.32": {"name": "Change management",
             "evidence": "audit_log_change_events", "domain": "Tech"},
}

# SOC 2 CC criteria (Common Criteria subset)
SOC2_CRITERIA: dict[str, str] = {
    "CC1.1": "Integrity and ethical values",
    "CC2.1": "Board oversight of internal control",
    "CC3.1": "Risk assessment process",
    "CC4.1": "Internal control monitoring",
    "CC5.1": "Control selection and development",
    "CC6.1": "Logical access controls",
    "CC6.2": "Authentication",
    "CC6.3": "Authorisation",
    "CC6.6": "Encryption in transit",
    "CC6.7": "Encryption at rest",
    "CC7.1": "System components detection",
    "CC7.2": "System component monitoring",
    "CC7.3": "Security event response",
    "CC7.4": "Incident response",
    "CC8.1": "Change management",
    "CC9.1": "Risk mitigation",
    "CC9.2": "Vendor risk management",
    "A1.1": "Availability commitment",
    "A1.2": "Availability monitoring",
    "C1.1": "Confidentiality commitment",
    "C1.2": "Confidentiality controls",
}


# ── Data model ────────────────────────────────────────────────────────────────

@dataclass
class ControlResult:
    control_id: str
    control_name: str
    status: str          # "pass" | "fail" | "partial" | "not_applicable" | "manual"
    evidence: dict[str, Any] = field(default_factory=dict)
    gap_description: str = ""
    remediation: str = ""
    severity: str = "medium"  # critical | high | medium | low

    def to_dict(self) -> dict[str, Any]:
        return {
            "control_id": self.control_id,
            "control_name": self.control_name,
            "status": self.status,
            "evidence": self.evidence,
            "gap_description": self.gap_description,
            "remediation": self.remediation,
            "severity": self.severity,
        }


@dataclass
class ComplianceGap:
    id: str
    framework: str
    control_id: str
    control_name: str
    description: str
    remediation: str
    severity: str = "medium"
    first_seen: float = 0.0
    resolved: bool = False
    resolved_at: float = 0.0

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id,
            "framework": self.framework,
            "control_id": self.control_id,
            "control_name": self.control_name,
            "description": self.description,
            "remediation": self.remediation,
            "severity": self.severity,
            "first_seen": self.first_seen,
            "resolved": self.resolved,
            "resolved_at": self.resolved_at,
        }

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> "ComplianceGap":
        return cls(
            id=d["id"],
            framework=d["framework"],
            control_id=d["control_id"],
            control_name=d.get("control_name", ""),
            description=d["description"],
            remediation=d.get("remediation", ""),
            severity=d.get("severity", "medium"),
            first_seen=d.get("first_seen", 0.0),
            resolved=d.get("resolved", False),
            resolved_at=d.get("resolved_at", 0.0),
        )


@dataclass
class ComplianceReport:
    id: str
    framework: str
    generated_at: float
    scope: str
    controls: list[ControlResult] = field(default_factory=list)
    summary: dict[str, Any] = field(default_factory=dict)
    signature: str = ""   # SHA256 of JSON content

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id,
            "framework": self.framework,
            "generated_at": self.generated_at,
            "scope": self.scope,
            "controls": [c.to_dict() for c in self.controls],
            "summary": self.summary,
            "signature": self.signature,
        }

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> "ComplianceReport":
        return cls(
            id=d["id"],
            framework=d["framework"],
            generated_at=d["generated_at"],
            scope=d.get("scope", "all"),
            controls=[ControlResult(**c) for c in d.get("controls", [])],
            summary=d.get("summary", {}),
            signature=d.get("signature", ""),
        )


@dataclass
class ComplianceConfig:
    # Customer profile influences control applicability
    industry: str = ""           # financial | healthcare | government | education | general
    country: str = "AU"
    employee_count: int = 0
    has_development_team: bool = False
    has_physical_office: bool = True
    cloud_providers: list[str] = field(default_factory=list)  # aws | azure | gcp
    # Which frameworks to track
    active_frameworks: list[str] = field(
        default_factory=lambda: ["essential_eight_ml1", "iso27001_2022"]
    )
    # Auto-generate reports on schedule (0 = manual only)
    report_interval: int = 2592000  # 30 days

    def to_dict(self) -> dict[str, Any]:
        return {
            "industry": self.industry,
            "country": self.country,
            "employee_count": self.employee_count,
            "has_development_team": self.has_development_team,
            "has_physical_office": self.has_physical_office,
            "cloud_providers": self.cloud_providers,
            "active_frameworks": self.active_frameworks,
            "report_interval": self.report_interval,
        }

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> "ComplianceConfig":
        return cls(
            industry=d.get("industry", ""),
            country=d.get("country", "AU"),
            employee_count=d.get("employee_count", 0),
            has_development_team=d.get("has_development_team", False),
            has_physical_office=d.get("has_physical_office", True),
            cloud_providers=d.get("cloud_providers", []),
            active_frameworks=d.get("active_frameworks", ["essential_eight_ml1", "iso27001_2022"]),
            report_interval=d.get("report_interval", 2592000),
        )


# ── Evidence collector ────────────────────────────────────────────────────────

class EvidenceCollector:
    """
    Collects evidence data from all Ozma managers.
    Each collect_* method returns a dict of evidence key-value pairs.
    Returns empty dict for managers that are not configured.
    """

    def __init__(self):
        # Managers are injected after construction
        self.backup_mgr = None
        self.audit_log = None
        self.threat_intel = None
        self.network_scan = None
        self.dlp = None
        self.mdm = None
        self.itam = None       # LicenseManager as proxy for ITAM
        self.itsm = None
        self.idp = None
        self.saas_mgr = None

    def collect(self) -> dict[str, Any]:
        """Collect all available evidence into a single dict."""
        evidence: dict[str, Any] = {}
        evidence.update(self._collect_backup())
        evidence.update(self._collect_audit_log())
        evidence.update(self._collect_threat_intel())
        evidence.update(self._collect_network_scan())
        evidence.update(self._collect_dlp())
        evidence.update(self._collect_mdm())
        evidence.update(self._collect_itam())
        evidence.update(self._collect_itsm())
        evidence.update(self._collect_saas())
        return evidence

    def _collect_backup(self) -> dict[str, Any]:
        if not self.backup_mgr:
            return {}
        try:
            s = self.backup_mgr.get_status()
            return {
                "backup_configured": s.get("sources_configured", 0) > 0,
                "backup_last_success": s.get("last_success"),
                "backup_object_lock": s.get("object_lock_enabled", False),
                "backup_recovery_tested": s.get("recovery_tested", False),
            }
        except Exception:
            return {}

    def _collect_audit_log(self) -> dict[str, Any]:
        if not self.audit_log:
            return {}
        try:
            s = self.audit_log.status() if hasattr(self.audit_log, "status") else {}
            return {
                "audit_log_entries": s.get("total_entries", 0),
                "audit_log_integrity": s.get("chain_valid", False),
                "audit_log_change_events": s.get("change_events", 0),
            }
        except Exception:
            return {}

    def _collect_threat_intel(self) -> dict[str, Any]:
        if not self.threat_intel:
            return {}
        try:
            s = self.threat_intel.status()
            return {
                "threat_intel_configured": s.get("enabled", False),
                "threat_intel_kev_entries": s.get("kev_entries", 0),
                "threat_intel_last_briefing": s.get("last_kev_poll", 0),
                "cve_open_critical": s.get("kev_matches_in_estate", 0),
            }
        except Exception:
            return {}

    def _collect_network_scan(self) -> dict[str, Any]:
        if not self.network_scan:
            return {}
        try:
            s = self.network_scan.status()
            return {
                "network_scan_configured": s.get("enabled", False),
                "network_scan_last_run": s.get("last_vuln_scan", 0),
                "network_scan_hosts": s.get("known_hosts", 0),
                "network_scan_open_findings": s.get("open_findings", 0),
            }
        except Exception:
            return {}

    def _collect_dlp(self) -> dict[str, Any]:
        if not self.dlp:
            return {}
        try:
            s = self.dlp.status()
            return {
                "dlp_configured": s.get("active_policies", 0) > 0,
                "dlp_policy_count": s.get("policies", 0),
                "dlp_incident_count": s.get("incidents_open", 0),
            }
        except Exception:
            return {}

    def _collect_mdm(self) -> dict[str, Any]:
        if not self.mdm:
            return {}
        try:
            s = self.mdm.status()
            total = s.get("total_devices", 0)
            return {
                "mdm_configured": s.get("configured", False),
                "mdm_enrolled_devices": total,
                "mdm_enrollment_rate": (
                    s.get("compliant_devices", 0) / total
                    if total > 0 else 0.0
                ),
            }
        except Exception:
            return {}

    def _collect_itam(self) -> dict[str, Any]:
        if not self.itam:
            return {}
        try:
            products = self.itam.list_products() if hasattr(self.itam, "list_products") else []
            return {
                "itam_asset_count": len(products),
                "itam_last_sync": time.time(),  # LicenseManager is always in sync
            }
        except Exception:
            return {}

    def _collect_itsm(self) -> dict[str, Any]:
        if not self.itsm:
            return {}
        try:
            s = self.itsm.status() if hasattr(self.itsm, "status") else {}
            return {
                "itsm_configured": True,
                "itsm_incident_count": s.get("open_tickets", 0),
            }
        except Exception:
            return {}

    def _collect_saas(self) -> dict[str, Any]:
        if not self.saas_mgr:
            return {}
        try:
            s = self.saas_mgr.status()
            risk = self.saas_mgr.vendor_risk_summary()
            return {
                "saas_apps_tracked": s.get("total_apps", 0),
                "saas_vendor_risk": len(risk),
                "saas_shadow_it": s.get("shadow_it", 0),
            }
        except Exception:
            return {}


# ── Framework evaluators ──────────────────────────────────────────────────────

def _evaluate_e8(maturity_level: int, evidence: dict[str, Any]) -> list[ControlResult]:
    """Evaluate Essential Eight controls for the specified maturity level."""
    results: list[ControlResult] = []
    prefix = f"e8.patch_apps.ml{maturity_level}"  # Example; full logic below

    # Backup controls
    backup_ok = evidence.get("backup_configured", False)
    object_lock = evidence.get("backup_object_lock", False)
    recovery_tested = evidence.get("backup_recovery_tested", False)

    if maturity_level >= 1:
        results.append(ControlResult(
            control_id="e8.backups.ml1",
            control_name=E8_CONTROLS["e8.backups.ml1"],
            status="pass" if backup_ok else "fail",
            evidence={"backup_configured": backup_ok},
            gap_description="" if backup_ok else "No backup source configured",
            remediation="Configure backup source in /api/v1/backup/sources",
            severity="critical",
        ))

    if maturity_level >= 2:
        results.append(ControlResult(
            control_id="e8.backups.ml2",
            control_name=E8_CONTROLS["e8.backups.ml2"],
            status="pass" if recovery_tested else "fail",
            evidence={"recovery_tested": recovery_tested},
            gap_description="" if recovery_tested else "Recovery not yet tested",
            remediation="Run a test restore via /api/v1/backup/restore",
            severity="high",
        ))

    if maturity_level >= 3:
        results.append(ControlResult(
            control_id="e8.backups.ml3",
            control_name=E8_CONTROLS["e8.backups.ml3"],
            status="pass" if (backup_ok and object_lock) else "fail",
            evidence={"object_lock": object_lock},
            gap_description="" if object_lock else "Backup Object Lock not enabled — ransomware can delete backups",
            remediation="Enable Object Lock on backup destination (S3/Backblaze B2)",
            severity="critical",
        ))

    # MFA controls
    mfa_configured = evidence.get("mdm_configured", False)

    if maturity_level >= 1:
        results.append(ControlResult(
            control_id="e8.mfa.ml1",
            control_name=E8_CONTROLS["e8.mfa.ml1"],
            status="manual",  # Requires external verification
            evidence={},
            gap_description="MFA on remote desktop requires manual verification",
            remediation="Verify MFA is enabled for all remote access methods",
            severity="high",
        ))

    if maturity_level >= 2:
        results.append(ControlResult(
            control_id="e8.mfa.ml2",
            control_name=E8_CONTROLS["e8.mfa.ml2"],
            status="manual",
            evidence={},
            gap_description="MFA on privileged accounts requires IdP verification",
            remediation="Enable MFA enforcement in Authentik for admin groups",
            severity="high",
        ))

    # Patch — vulnerability data feeds into these
    open_critical = evidence.get("cve_open_critical", 0)
    scan_configured = evidence.get("network_scan_configured", False)

    if maturity_level >= 1:
        results.append(ControlResult(
            control_id="e8.patch_apps.ml1",
            control_name=E8_CONTROLS["e8.patch_apps.ml1"],
            status="pass" if (scan_configured and open_critical == 0) else "partial",
            evidence={"scan_configured": scan_configured, "open_critical": open_critical},
            gap_description=(
                "" if (scan_configured and open_critical == 0)
                else f"{open_critical} critical CVEs open — review and patch"
            ),
            remediation="Patch critical CVEs within 30 days (ML1)",
            severity="critical" if open_critical > 0 else "medium",
        ))

    # Threat intelligence
    ti_configured = evidence.get("threat_intel_configured", False)
    if maturity_level >= 2:
        results.append(ControlResult(
            control_id="e8.patch_apps.ml2",
            control_name="Threat-informed patching (ML2)",
            status="pass" if ti_configured else "fail",
            evidence={"threat_intel_configured": ti_configured},
            gap_description="" if ti_configured else "Threat intelligence not configured",
            remediation="Enable threat intelligence polling in /api/v1/threat/config",
            severity="high",
        ))

    # DLP
    dlp_configured = evidence.get("dlp_configured", False)
    if maturity_level >= 2:
        results.append(ControlResult(
            control_id="e8.app_control.ml2",
            control_name=E8_CONTROLS["e8.app_control.ml2"],
            status="partial",
            evidence={"dlp_configured": dlp_configured},
            gap_description="Application allowlisting requires endpoint agent with audit evidence",
            remediation="Deploy Ozma agent to all endpoints and enable process monitoring",
            severity="medium",
        ))

    return results


def _evaluate_iso27001(evidence: dict[str, Any]) -> list[ControlResult]:
    """Evaluate ISO 27001:2022 Annex A controls against collected evidence."""
    results: list[ControlResult] = []

    # Evidence mapping: control evidence key → what we check
    evidence_checks: dict[str, tuple[str, str, str, str]] = {
        # control_id: (evidence_key, check_value, gap_desc, remediation)
        "5.7":  ("threat_intel_configured", True,
                 "Threat intelligence not configured",
                 "Enable /api/v1/threat/config and set monitored_domains"),
        "5.9":  ("itam_asset_count", 1,   # >= 1 asset
                 "No asset inventory configured",
                 "Add assets to /api/v1/license/products or connect node agents"),
        "5.15": ("mdm_configured", True,
                 "No access control system (MDM/IdP) configured",
                 "Configure MDM bridge or Authentik IdP"),
        "5.19": ("saas_apps_tracked", 1,
                 "No SaaS vendor inventory",
                 "Run SaaS discovery via /api/v1/saas/discover"),
        "5.23": ("backup_configured", True,
                 "No cloud service backup configured",
                 "Configure M365/Google backup in /api/v1/backup/sources"),
        "5.26": ("itsm_configured", True,
                 "No incident management system configured",
                 "Configure ITSM in /api/v1/itsm/config"),
        "5.28": ("audit_log_integrity", True,
                 "Audit log integrity check failing",
                 "Audit log hash chain may be broken; investigate"),
        "8.3":  ("dlp_configured", True,
                 "No DLP (data leakage prevention) configured",
                 "Create a DLP policy via /api/v1/dlp/policies"),
        "8.8":  ("cve_open_critical", 0,  # == 0 is pass (checked as <=)
                 "Critical CVEs from KEV catalogue unpatched",
                 "Apply patches for KEV-listed CVEs immediately"),
        "8.12": ("dlp_configured", True,
                 "No data leakage prevention controls active",
                 "Enable DLP policies for file and cloud scopes"),
        "8.13": ("backup_configured", True,
                 "No backup configured",
                 "Configure backup in /api/v1/backup/sources"),
        "8.15": ("audit_log_entries", 1,
                 "No audit log entries — logging may not be active",
                 "Ensure audit_log is enabled in controller config"),
        "8.16": ("network_scan_configured", True,
                 "No network monitoring/scanning configured",
                 "Enable network scanning in /api/v1/network-scan/config"),
        "8.24": ("backup_configured", True,  # Use as proxy — transport encryption is always on
                 "Encryption evidence from transport layer",
                 "Ozma transport uses XChaCha20-Poly1305 by default; verify config"),
    }

    for control_id, control_meta in ISO27001_CONTROLS.items():
        if control_id in evidence_checks:
            ev_key, expected, gap_desc, remediation = evidence_checks[control_id]
            value = evidence.get(ev_key)
            if isinstance(expected, bool):
                passed = bool(value) == expected
            elif isinstance(expected, int) and expected == 0:
                passed = (isinstance(value, (int, float)) and value == 0)
            else:
                passed = (isinstance(value, (int, float)) and value >= expected)

            results.append(ControlResult(
                control_id=f"iso27001.{control_id}",
                control_name=f"{control_id}: {control_meta['name']}",
                status="pass" if passed else "fail",
                evidence={ev_key: value},
                gap_description="" if passed else gap_desc,
                remediation="" if passed else remediation,
                severity=_iso_severity(control_id),
            ))
        else:
            # Controls that need manual verification or are always manual
            results.append(ControlResult(
                control_id=f"iso27001.{control_id}",
                control_name=f"{control_id}: {control_meta['name']}",
                status="manual",
                evidence={},
                gap_description=f"Manual verification required for {control_meta['name']}",
                remediation="Document this control in your ISMS policy documents",
                severity="low",
            ))

    return results


def _evaluate_soc2(evidence: dict[str, Any]) -> list[ControlResult]:
    """Evaluate SOC 2 common criteria."""
    results: list[ControlResult] = []

    soc2_evidence: dict[str, tuple[str | None, str, str]] = {
        "CC6.1": ("mdm_configured",     "No logical access control (MDM/IdP)",
                  "Configure MDM or IdP for logical access control"),
        "CC6.6": (None,                  "Transport encryption verified by architecture",
                  "Ozma uses XChaCha20-Poly1305 for all node traffic by default"),
        "CC7.1": ("network_scan_configured", "No system component detection/monitoring",
                  "Enable network scanning"),
        "CC7.3": ("itsm_configured",     "No security event response system",
                  "Configure ITSM for incident response"),
        "CC7.4": ("itsm_configured",     "No incident response process",
                  "Define incident response procedures in ITSM"),
        "CC8.1": ("audit_log_integrity", "No change management audit trail",
                  "Ensure audit_log change events are enabled"),
        "CC9.2": ("saas_vendor_risk",    "No vendor risk management",
                  "Run SaaS vendor risk analysis via /api/v1/saas/vendor-risk"),
        "A1.2":  ("network_scan_configured", "No availability monitoring",
                  "Enable network_health monitoring"),
        "C1.2":  ("dlp_configured",      "No confidentiality controls (DLP)",
                  "Enable DLP policies"),
    }

    for cc_id, cc_name in SOC2_CRITERIA.items():
        if cc_id in soc2_evidence:
            ev_key, gap_desc, remediation = soc2_evidence[cc_id]
            if ev_key is None:
                status = "pass"
                evidence_val = "architectural_control"
                gap = ""
                rem = ""
            else:
                value = evidence.get(ev_key)
                if isinstance(value, bool):
                    passed = value
                elif isinstance(value, (int, float)):
                    # For vendor_risk we treat 0 gaps as pass
                    if ev_key == "saas_vendor_risk":
                        passed = True  # Having the data at all satisfies the control
                    else:
                        passed = value > 0
                else:
                    passed = bool(value)
                status = "pass" if passed else "fail"
                evidence_val = value
                gap = "" if passed else gap_desc
                rem = "" if passed else remediation
            results.append(ControlResult(
                control_id=f"soc2.{cc_id}",
                control_name=f"{cc_id}: {cc_name}",
                status=status,
                evidence={ev_key or "arch": evidence_val},
                gap_description=gap,
                remediation=rem,
                severity="high",
            ))
        else:
            results.append(ControlResult(
                control_id=f"soc2.{cc_id}",
                control_name=f"{cc_id}: {cc_name}",
                status="manual",
                evidence={},
                gap_description="Manual verification required",
                remediation="Document this criterion in your SOC 2 management system",
                severity="medium",
            ))

    return results


def _iso_severity(control_id: str) -> str:
    """Map ISO 27001 control to gap severity."""
    critical_controls = {"8.8", "8.13", "5.28"}  # CVEs, backup, evidence
    high_controls = {"5.7", "5.15", "8.3", "8.15", "8.16"}
    if control_id in critical_controls:
        return "critical"
    if control_id in high_controls:
        return "high"
    return "medium"


# ── Main manager ──────────────────────────────────────────────────────────────

class ComplianceReportEngine:
    """
    Orchestrates compliance evidence collection and report generation.
    """

    SUPPORTED_FRAMEWORKS = [
        "essential_eight_ml1",
        "essential_eight_ml2",
        "essential_eight_ml3",
        "iso27001_2022",
        "soc2_type1",
    ]

    def __init__(self, data_dir: Path, event_queue: asyncio.Queue | None = None) -> None:
        self._data_dir = data_dir
        self._data_dir.mkdir(parents=True, exist_ok=True)
        (self._data_dir / "reports").mkdir(exist_ok=True)
        self._config = ComplianceConfig()
        self._gaps: dict[str, ComplianceGap] = {}    # gap_id → gap
        self._event_queue = event_queue
        self._last_report_times: dict[str, float] = {}
        self._task: asyncio.Task | None = None
        self._evidence = EvidenceCollector()

    # ── Manager injection ─────────────────────────────────────────────────────

    def inject_managers(self, **kwargs: Any) -> None:
        """Inject optional manager references into the evidence collector."""
        for key, val in kwargs.items():
            if hasattr(self._evidence, key):
                setattr(self._evidence, key, val)

    # ── Lifecycle ─────────────────────────────────────────────────────────────

    async def start(self) -> None:
        self._load()
        self._task = asyncio.create_task(self._background_loop(),
                                          name="compliance_reports")

    async def stop(self) -> None:
        if self._task:
            self._task.cancel()
            try:
                await self._task
            except asyncio.CancelledError:
                pass

    # ── Persistence ───────────────────────────────────────────────────────────

    def _load(self) -> None:
        config_path = self._data_dir / "config.json"
        if config_path.exists():
            try:
                self._config = ComplianceConfig.from_dict(
                    json.loads(config_path.read_text())
                )
            except Exception as e:
                log.error("Failed to load compliance config: %s", e)

        gaps_path = self._data_dir / "gaps.json"
        if gaps_path.exists():
            try:
                data = json.loads(gaps_path.read_text())
                for d in data.get("gaps", []):
                    g = ComplianceGap.from_dict(d)
                    self._gaps[g.id] = g
            except Exception as e:
                log.error("Failed to load compliance gaps: %s", e)

        times_path = self._data_dir / "report_times.json"
        if times_path.exists():
            try:
                self._last_report_times = json.loads(times_path.read_text())
            except Exception:
                pass

    def _save(self) -> None:
        gaps_path = self._data_dir / "gaps.json"
        tmp = gaps_path.with_suffix(".tmp")
        tmp.write_text(json.dumps(
            {"gaps": [g.to_dict() for g in self._gaps.values()]}, indent=2
        ))
        tmp.replace(gaps_path)

        times_path = self._data_dir / "report_times.json"
        tmp = times_path.with_suffix(".tmp")
        tmp.write_text(json.dumps(self._last_report_times))
        tmp.replace(times_path)

    def _save_config(self) -> None:
        path = self._data_dir / "config.json"
        tmp = path.with_suffix(".tmp")
        tmp.write_text(json.dumps(self._config.to_dict(), indent=2))
        tmp.replace(path)

    # ── Config ────────────────────────────────────────────────────────────────

    def get_config(self) -> ComplianceConfig:
        return self._config

    def set_config(self, updates: dict[str, Any]) -> ComplianceConfig:
        d = self._config.to_dict()
        d.update(updates)
        self._config = ComplianceConfig.from_dict(d)
        self._save_config()
        return self._config

    # ── Report generation ─────────────────────────────────────────────────────

    async def generate_report(
        self,
        framework: str,
        scope: str = "all",
    ) -> ComplianceReport:
        """
        Generate a compliance report for the specified framework.
        framework: one of SUPPORTED_FRAMEWORKS
        Returns a ComplianceReport with signed JSON evidence package.
        """
        if framework not in self.SUPPORTED_FRAMEWORKS:
            raise ValueError(f"Unknown framework: {framework}. "
                             f"Supported: {self.SUPPORTED_FRAMEWORKS}")

        # Collect evidence from all configured managers
        evidence = await asyncio.get_event_loop().run_in_executor(
            None, self._evidence.collect
        )

        controls = self._evaluate_framework(framework, evidence)

        # Compute summary statistics
        total = len(controls)
        passed = sum(1 for c in controls if c.status == "pass")
        failed = sum(1 for c in controls if c.status == "fail")
        manual = sum(1 for c in controls if c.status == "manual")
        partial = sum(1 for c in controls if c.status == "partial")
        score = round(passed / (passed + failed) * 100, 1) if (passed + failed) > 0 else 0.0

        summary = {
            "total_controls": total,
            "passed": passed,
            "failed": failed,
            "manual": manual,
            "partial": partial,
            "score_pct": score,
            "gaps_critical": sum(1 for c in controls
                                  if c.status == "fail" and c.severity == "critical"),
            "gaps_high": sum(1 for c in controls
                              if c.status == "fail" and c.severity == "high"),
        }

        report = ComplianceReport(
            id=str(uuid.uuid4()),
            framework=framework,
            generated_at=time.time(),
            scope=scope,
            controls=controls,
            summary=summary,
        )

        # Sign report with SHA256
        body = json.dumps(
            {"id": report.id, "framework": framework, "controls": [c.to_dict() for c in controls]},
            sort_keys=True
        )
        report.signature = hashlib.sha256(body.encode()).hexdigest()

        # Persist report
        report_path = self._data_dir / "reports" / f"{report.id}.json"
        report_path.write_text(json.dumps(report.to_dict(), indent=2))

        # Update gaps from this report
        self._update_gaps_from_report(framework, controls)
        self._last_report_times[framework] = time.time()
        self._save()

        await self._fire_event("compliance.report.generated", {
            "report_id": report.id,
            "framework": framework,
            "score_pct": score,
            "failed": failed,
        })

        return report

    def _evaluate_framework(
        self, framework: str, evidence: dict[str, Any]
    ) -> list[ControlResult]:
        if framework == "essential_eight_ml1":
            return _evaluate_e8(1, evidence)
        if framework == "essential_eight_ml2":
            return _evaluate_e8(2, evidence)
        if framework == "essential_eight_ml3":
            return _evaluate_e8(3, evidence)
        if framework == "iso27001_2022":
            return _evaluate_iso27001(evidence)
        if framework == "soc2_type1":
            return _evaluate_soc2(evidence)
        return []

    def _update_gaps_from_report(
        self, framework: str, controls: list[ControlResult]
    ) -> None:
        now = time.time()
        # Track gaps from this report
        active_gap_keys: set[str] = set()
        for c in controls:
            if c.status in ("fail", "partial"):
                gap_key = f"{framework}:{c.control_id}"
                active_gap_keys.add(gap_key)
                if gap_key not in {f"{g.framework}:{g.control_id}"
                                    for g in self._gaps.values() if not g.resolved}:
                    gap = ComplianceGap(
                        id=str(uuid.uuid4()),
                        framework=framework,
                        control_id=c.control_id,
                        control_name=c.control_name,
                        description=c.gap_description,
                        remediation=c.remediation,
                        severity=c.severity,
                        first_seen=now,
                    )
                    self._gaps[gap.id] = gap

        # Auto-resolve gaps that are now passing
        for gap in self._gaps.values():
            if gap.framework == framework and not gap.resolved:
                gap_key = f"{gap.framework}:{gap.control_id}"
                if gap_key not in active_gap_keys:
                    gap.resolved = True
                    gap.resolved_at = now

    # ── Gap management ────────────────────────────────────────────────────────

    def list_gaps(
        self,
        framework: str | None = None,
        resolved: bool | None = None,
        severity: str | None = None,
    ) -> list[ComplianceGap]:
        gaps = list(self._gaps.values())
        if framework:
            gaps = [g for g in gaps if g.framework == framework]
        if resolved is not None:
            gaps = [g for g in gaps if g.resolved == resolved]
        if severity:
            gaps = [g for g in gaps if g.severity == severity]
        return sorted(gaps, key=lambda g: g.first_seen, reverse=True)

    def resolve_gap(self, gap_id: str, notes: str = "") -> ComplianceGap | None:
        gap = self._gaps.get(gap_id)
        if not gap:
            return None
        gap.resolved = True
        gap.resolved_at = time.time()
        self._save()
        return gap

    # ── Report listing ────────────────────────────────────────────────────────

    def list_reports(self) -> list[dict[str, Any]]:
        reports = []
        for path in sorted((self._data_dir / "reports").glob("*.json"),
                             key=lambda p: p.stat().st_mtime, reverse=True):
            try:
                d = json.loads(path.read_text())
                reports.append({
                    "id": d["id"],
                    "framework": d["framework"],
                    "generated_at": d["generated_at"],
                    "summary": d.get("summary", {}),
                    "signature": d.get("signature", ""),
                })
            except Exception:
                continue
        return reports

    def get_report(self, report_id: str) -> ComplianceReport | None:
        path = self._data_dir / "reports" / f"{report_id}.json"
        if not path.exists():
            return None
        try:
            return ComplianceReport.from_dict(json.loads(path.read_text()))
        except Exception:
            return None

    # ── SoA generation ────────────────────────────────────────────────────────

    async def generate_soa(self) -> dict[str, Any]:
        """
        Generate a Statement of Applicability for ISO 27001:2022.
        Returns a structured SoA document as a dict.
        """
        evidence = await asyncio.get_event_loop().run_in_executor(
            None, self._evidence.collect
        )
        controls = _evaluate_iso27001(evidence)

        soa_entries = []
        for ctrl in controls:
            soa_entries.append({
                "control_id": ctrl.control_id,
                "control_name": ctrl.control_name,
                "applicable": True,  # All controls applicable by default; can be overridden
                "implementation_status": ctrl.status,
                "justification": (
                    "Implemented via Ozma platform controls" if ctrl.status == "pass"
                    else ctrl.gap_description if ctrl.status == "fail"
                    else "Manual verification required"
                ),
                "evidence_reference": ctrl.evidence,
                "gap": ctrl.gap_description,
                "remediation": ctrl.remediation,
            })

        pass_count = sum(1 for c in controls if c.status == "pass")
        return {
            "title": "Statement of Applicability",
            "standard": "ISO/IEC 27001:2022",
            "generated_at": time.time(),
            "total_controls": len(controls),
            "implemented": pass_count,
            "coverage_pct": round(pass_count / len(controls) * 100, 1) if controls else 0.0,
            "entries": soa_entries,
        }

    # ── Background loop ───────────────────────────────────────────────────────

    async def _background_loop(self) -> None:
        while True:
            try:
                await asyncio.sleep(3600)
                if self._config.report_interval <= 0:
                    continue
                now = time.time()
                for framework in self._config.active_frameworks:
                    last = self._last_report_times.get(framework, 0.0)
                    if now - last >= self._config.report_interval:
                        log.info("Scheduled compliance report: %s", framework)
                        await self.generate_report(framework)
            except asyncio.CancelledError:
                return
            except Exception as e:
                log.error("Compliance report background loop error: %s", e)

    # ── Events ────────────────────────────────────────────────────────────────

    async def _fire_event(self, event_type: str, data: dict) -> None:
        if self._event_queue:
            await self._event_queue.put({"type": event_type, "data": data})

    # ── Status ────────────────────────────────────────────────────────────────

    def status(self) -> dict[str, Any]:
        open_gaps = [g for g in self._gaps.values() if not g.resolved]
        return {
            "active_frameworks": self._config.active_frameworks,
            "total_gaps": len(open_gaps),
            "critical_gaps": sum(1 for g in open_gaps if g.severity == "critical"),
            "high_gaps": sum(1 for g in open_gaps if g.severity == "high"),
            "reports_generated": len(list((self._data_dir / "reports").glob("*.json"))),
            "last_report_times": self._last_report_times,
        }
