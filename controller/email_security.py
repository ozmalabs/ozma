# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Email security monitor — SPF, DKIM, DMARC, MTA-STS, BIMI posture checks.

Phase 1: DNS-based monitoring.  No mail gateway access needed — all checks are
pure DNS lookups.  Results are surfaced as compliance alerts in the same health
system used by backup, network, and metrics modules.

Supported checks:
  SPF    — missing, permissive (+all / ?all), too many DNS lookups (>10)
  DKIM   — probes common selectors; reports missing or weak keys
  DMARC  — missing, p=none, pct<100, no rua/ruf reporting address
  MTA-STS — missing (downgrade attacks); present = bonus hardening point
  BIMI   — informational only; presence indicates mature email programme

Scheduling: checks run at startup then every RECHECK_INTERVAL seconds.
Per-domain results are cached; a domain that last checked OK is not re-alerted
until it regresses or RECHECK_INTERVAL elapses.

Wire-up: EmailSecurityMonitor.start() / .stop() + passed to build_app().
API endpoints registered in api.py.
"""

from __future__ import annotations

import asyncio
import logging
import re
import time
from dataclasses import dataclass, field
from typing import Any

log = logging.getLogger("ozma.email_security")

RECHECK_INTERVAL = 3600.0       # re-check all domains every hour
INITIAL_DELAY = 10.0            # seconds after start() before first scan
DNS_TIMEOUT = 5.0               # per-query timeout

# Common DKIM selectors to probe when the user hasn't specified one.
# These cover the major platforms: Google Workspace, M365, Mailchimp, SendGrid,
# Proofpoint, Mimecast, and common self-hosted setups.
_COMMON_DKIM_SELECTORS = [
    "google", "mail", "default", "dkim", "email",
    "selector1", "selector2",           # M365
    "k1", "k2", "k3",                   # Mailchimp / Klaviyo
    "s1", "s2",
    "mimecast", "pm",                   # Proofpoint
    "mandrill", "mailjet", "sendgrid",
    "zoho",
]

# SPF mechanisms that allow the whole internet to send — effectively no SPF.
_SPF_PERMISSIVE = ("+all", "?all")

# DMARC policy values considered insufficiently strict.
_DMARC_WEAK_POLICIES = {"none"}


# ── Remediation guides ────────────────────────────────────────────────────────

@dataclass
class RemediationStep:
    """One concrete step in a provider-specific remediation guide."""
    instruction: str        # human-readable action
    url: str = ""           # admin console URL to open (if applicable)
    command: str = ""       # CLI command (if applicable)

    def to_dict(self) -> dict[str, Any]:
        d: dict[str, Any] = {"instruction": self.instruction}
        if self.url:
            d["url"] = self.url
        if self.command:
            d["command"] = self.command
        return d


@dataclass
class ProviderGuide:
    """Step-by-step fix instructions for one mail provider."""
    provider: str           # "google" | "microsoft" | "generic" etc.
    provider_label: str     # human label, e.g. "Google Workspace"
    steps: list[RemediationStep] = field(default_factory=list)
    auto_fix_available: bool = False   # True when Ozma can apply via API
    auto_fix_scope: str = ""           # OAuth scope required, if auto_fix_available

    def to_dict(self) -> dict[str, Any]:
        return {
            "provider": self.provider,
            "provider_label": self.provider_label,
            "steps": [s.to_dict() for s in self.steps],
            "auto_fix_available": self.auto_fix_available,
            "auto_fix_scope": self.auto_fix_scope,
        }


# ── Per-issue remediation library ─────────────────────────────────────────────
#
# Each entry is keyed by issue code. Value is a callable:
#   build_guides(domain, current_record, provider) -> list[ProviderGuide]
#
# Guides are generated lazily so they can incorporate live values (current SPF
# record text, detected provider, etc.).

def _guides_no_spf(domain: str, _record: str, provider: str) -> list[ProviderGuide]:
    google_include = "include:_spf.google.com"
    ms_include = "include:spf.protection.outlook.com"
    generic_record = f"v=spf1 include:<your-provider> -all"
    provider_record = (
        f"v=spf1 {google_include} -all" if provider == "google"
        else f"v=spf1 {ms_include} -all" if provider == "microsoft"
        else generic_record
    )

    guides: list[ProviderGuide] = []

    if provider == "google":
        guides.append(ProviderGuide(
            provider="google",
            provider_label="Google Workspace",
            auto_fix_available=True,
            auto_fix_scope="https://www.googleapis.com/auth/admin.directory.domain",
            steps=[
                RemediationStep(
                    "Open Google Admin → Apps → Google Workspace → Gmail → "
                    "Authenticate email (SPF).",
                    url="https://admin.google.com/ac/apps/gmail/authenticateemail",
                ),
                RemediationStep(
                    "Click 'Generate new record'. Google will show you the exact "
                    "TXT record to add.",
                ),
                RemediationStep(
                    f"At your DNS provider, add a TXT record on '@' (the domain root):",
                    command=f"@ IN TXT \"{provider_record}\"",
                ),
                RemediationStep(
                    "Back in Google Admin, click 'Start authentication'. Google "
                    "will verify the record within 48 hours.",
                ),
            ],
        ))
    elif provider == "microsoft":
        guides.append(ProviderGuide(
            provider="microsoft",
            provider_label="Microsoft 365",
            auto_fix_available=True,
            auto_fix_scope="https://graph.microsoft.com/Domain.ReadWrite.All",
            steps=[
                RemediationStep(
                    "Open Microsoft 365 Admin Center → Settings → Domains → "
                    f"select {domain}.",
                    url=f"https://admin.microsoft.com/AdminPortal/Home#/Domains",
                ),
                RemediationStep(
                    "Click 'Check health' — Microsoft will show the exact records "
                    "required. Look for the SPF TXT record.",
                ),
                RemediationStep(
                    "At your DNS provider, add a TXT record on '@':",
                    command=f"@ IN TXT \"{provider_record}\"",
                ),
                RemediationStep(
                    "Return to Microsoft Admin and click 'Verify'. Propagation "
                    "typically takes 15 minutes to 1 hour.",
                ),
            ],
        ))

    # Always include generic guide as fallback
    guides.append(ProviderGuide(
        provider="generic",
        provider_label="Any DNS provider",
        steps=[
            RemediationStep(
                "Log in to your DNS provider (where you registered the domain or "
                "manage DNS, e.g. Cloudflare, Route 53, GoDaddy, Namecheap).",
            ),
            RemediationStep(
                f"Add a new TXT record with these values:",
                command=(
                    f"Name/Host: @  (or leave blank — means the domain root)\n"
                    f"Type: TXT\n"
                    f"Value: \"{provider_record}\"\n"
                    f"TTL: 3600"
                ),
            ),
            RemediationStep(
                "Wait for DNS propagation (usually 5–60 minutes). "
                "Ozma will re-check automatically and clear this alert once the "
                "record is live.",
            ),
        ],
    ))
    return guides


def _guides_spf_permissive(domain: str, current: str, provider: str) -> list[ProviderGuide]:
    fixed = re.sub(r"[+?]all", "-all", current)
    return [ProviderGuide(
        provider="generic",
        provider_label="Any DNS provider",
        steps=[
            RemediationStep(
                "Log in to your DNS provider and find the existing SPF TXT record "
                f"on '@' for {domain}.",
            ),
            RemediationStep(
                "Edit the record — change the final mechanism to '-all' (hard fail):",
                command=f"@ IN TXT \"{fixed}\"",
            ),
            RemediationStep(
                "Save the change. Ozma will re-check within the hour.",
            ),
        ],
    )]


def _guides_no_dkim(domain: str, _record: str, provider: str) -> list[ProviderGuide]:
    guides: list[ProviderGuide] = []

    if provider == "google":
        guides.append(ProviderGuide(
            provider="google",
            provider_label="Google Workspace",
            auto_fix_available=True,
            auto_fix_scope="https://www.googleapis.com/auth/admin.directory.domain",
            steps=[
                RemediationStep(
                    "Open Google Admin → Apps → Google Workspace → Gmail → "
                    "Authenticate email (DKIM).",
                    url="https://admin.google.com/ac/apps/gmail/authenticateemail",
                ),
                RemediationStep(
                    "Click 'Generate new record'. Choose key length 2048 bits. "
                    "Leave the default selector prefix ('google').",
                ),
                RemediationStep(
                    "Copy the TXT record value shown and add it at your DNS provider:",
                    command=f"google._domainkey.{domain} IN TXT \"v=DKIM1; k=rsa; p=<key>\"",
                ),
                RemediationStep(
                    "Return to Google Admin and click 'Start authentication'. "
                    "DKIM signing activates once the DNS record is verified.",
                ),
            ],
        ))
    elif provider == "microsoft":
        guides.append(ProviderGuide(
            provider="microsoft",
            provider_label="Microsoft 365",
            auto_fix_available=True,
            auto_fix_scope="https://graph.microsoft.com/Domain.ReadWrite.All",
            steps=[
                RemediationStep(
                    "Open Microsoft Defender portal → Email & collaboration → "
                    "Policies & rules → Threat policies → DKIM.",
                    url="https://security.microsoft.com/dkimv2",
                ),
                RemediationStep(
                    f"Find {domain} in the list. If DKIM is disabled, click on "
                    "the domain row and toggle 'Sign messages for this domain "
                    "with DKIM signatures' to enabled.",
                ),
                RemediationStep(
                    "Microsoft will show two CNAME records to add at your DNS provider "
                    "(selector1 and selector2). Add both:",
                    command=(
                        f"selector1._domainkey.{domain} IN CNAME "
                        f"selector1-{domain.replace('.', '-')}._domainkey.onmicrosoft.com\n"
                        f"selector2._domainkey.{domain} IN CNAME "
                        f"selector2-{domain.replace('.', '-')}._domainkey.onmicrosoft.com"
                    ),
                ),
                RemediationStep(
                    "Once DNS propagates, return to the DKIM page and enable signing. "
                    "Propagation typically takes 15–60 minutes.",
                ),
            ],
        ))

    guides.append(ProviderGuide(
        provider="generic",
        provider_label="Any mail provider",
        steps=[
            RemediationStep(
                "Find the DKIM setup section in your mail provider's admin console. "
                "It is often under 'Email authentication', 'Domain settings', or "
                "'Advanced DNS'. Your provider will generate a key pair for you.",
            ),
            RemediationStep(
                "Copy the public key TXT record value your provider gives you, then "
                "add it at your DNS provider:",
                command=f"<selector>._domainkey.{domain} IN TXT \"v=DKIM1; k=rsa; p=<public-key>\"",
            ),
            RemediationStep(
                "Return to your mail provider and activate DKIM signing. "
                "Ozma will detect the new record automatically.",
            ),
        ],
    ))
    return guides


def _guides_no_dmarc(domain: str, _record: str, provider: str) -> list[ProviderGuide]:
    rua_addr = f"dmarc-reports@{domain}"
    record = f"v=DMARC1; p=quarantine; rua=mailto:{rua_addr}; pct=100"

    guides: list[ProviderGuide] = []

    if provider == "google":
        guides.append(ProviderGuide(
            provider="google",
            provider_label="Google Workspace",
            auto_fix_available=True,
            auto_fix_scope="https://www.googleapis.com/auth/admin.directory.domain",
            steps=[
                RemediationStep(
                    "First, ensure SPF and DKIM are both configured and verified "
                    "(complete those steps first if shown above).",
                ),
                RemediationStep(
                    "Open Google Admin → Apps → Google Workspace → Gmail → "
                    "Authenticate email — confirm DKIM status shows 'Authenticating email'.",
                    url="https://admin.google.com/ac/apps/gmail/authenticateemail",
                ),
                RemediationStep(
                    f"At your DNS provider, add a TXT record at _dmarc.{domain}:",
                    command=f"_dmarc.{domain} IN TXT \"{record}\"",
                ),
                RemediationStep(
                    f"Create a mailbox or alias for {rua_addr} — this is where "
                    "aggregate DMARC reports from other mail systems will be sent. "
                    "Ozma will parse these reports for you.",
                ),
            ],
        ))
    elif provider == "microsoft":
        guides.append(ProviderGuide(
            provider="microsoft",
            provider_label="Microsoft 365",
            auto_fix_available=True,
            auto_fix_scope="https://graph.microsoft.com/Domain.ReadWrite.All",
            steps=[
                RemediationStep(
                    "Ensure SPF and DKIM are both working (complete those steps first).",
                ),
                RemediationStep(
                    "Microsoft 365 Admin Center shows a DMARC setup guide under "
                    "Settings → Domains → select domain → 'Check health'.",
                    url="https://admin.microsoft.com/AdminPortal/Home#/Domains",
                ),
                RemediationStep(
                    f"At your DNS provider, add a TXT record at _dmarc:",
                    command=f"_dmarc IN TXT \"{record}\"",
                ),
                RemediationStep(
                    "Start with p=quarantine and monitor reports for 2–4 weeks "
                    "before advancing to p=reject.",
                ),
            ],
        ))

    guides.append(ProviderGuide(
        provider="generic",
        provider_label="Any DNS provider",
        steps=[
            RemediationStep(
                "Ensure SPF and DKIM are working first — DMARC without them "
                "will cause legitimate email to fail.",
            ),
            RemediationStep(
                f"At your DNS provider, add a TXT record:",
                command=(
                    f"Name/Host: _dmarc  (not the domain root)\n"
                    f"Type: TXT\n"
                    f"Value: \"{record}\"\n"
                    f"TTL: 3600"
                ),
            ),
            RemediationStep(
                "Start with p=quarantine for 2–4 weeks while reviewing reports, "
                "then advance to p=reject for full protection. "
                "Ozma will alert you if any legitimate sending source fails DMARC.",
            ),
        ],
    ))
    return guides


def _guides_dmarc_policy_none(domain: str, current: str, provider: str) -> list[ProviderGuide]:
    fixed = current.replace("p=none", "p=quarantine")
    return [ProviderGuide(
        provider="generic",
        provider_label="Any DNS provider",
        steps=[
            RemediationStep(
                "Review your DMARC aggregate reports first (the rua= address) to "
                "confirm all legitimate mail sources are passing SPF or DKIM. "
                "Advancing to quarantine before this step can interrupt legitimate email.",
            ),
            RemediationStep(
                f"At your DNS provider, edit the TXT record at _dmarc.{domain}. "
                "Change p=none to p=quarantine:",
                command=f"_dmarc IN TXT \"{fixed}\"",
            ),
            RemediationStep(
                "Monitor for 2–4 weeks. If no legitimate mail is affected, "
                "advance to p=reject for full spoofing protection.",
                command=f"_dmarc IN TXT \"{fixed.replace('p=quarantine', 'p=reject')}\"",
            ),
        ],
    )]


def _guides_mta_sts(domain: str, _record: str, provider: str) -> list[ProviderGuide]:
    return [ProviderGuide(
        provider="generic",
        provider_label="Any provider",
        steps=[
            RemediationStep(
                "MTA-STS requires hosting a policy file over HTTPS at "
                f"https://mta-sts.{domain}/.well-known/mta-sts.txt "
                "— this is typically done via your web server or a CDN.",
            ),
            RemediationStep(
                "Create the policy file with these contents:",
                command=(
                    "version: STSv1\n"
                    "mode: enforce\n"
                    f"mx: mail.{domain}\n"
                    "max_age: 604800"
                ),
            ),
            RemediationStep(
                "Add a DNS TXT record to signal the policy is active:",
                command=f"_mta-sts IN TXT \"v=STSv1; id=20240101000000\"",
            ),
            RemediationStep(
                "Update the id= value whenever you change the policy file. "
                "This tells remote MTAs to re-fetch the policy.",
            ),
        ],
    )]


# Registry: issue code → guide builder
_REMEDIATION_BUILDERS: dict[str, Any] = {
    "NO_SPF":              _guides_no_spf,
    "SPF_PERMISSIVE":      _guides_spf_permissive,
    "SPF_SOFTFAIL":        lambda d, r, p: [ProviderGuide(
        provider="generic", provider_label="Any DNS provider", steps=[
            RemediationStep(
                f"Edit the SPF record at your DNS provider — change '~all' to '-all':",
                command=f"@ IN TXT \"{r.replace('~all', '-all')}\"",
            ),
        ],
    )],
    "MULTIPLE_SPF":        lambda d, r, p: [ProviderGuide(
        provider="generic", provider_label="Any DNS provider", steps=[
            RemediationStep(
                f"At your DNS provider, delete all TXT records on '@' that start "
                "with 'v=spf1', then add a single merged record combining all "
                "include: mechanisms.",
            ),
        ],
    )],
    "SPF_TOO_MANY_LOOKUPS": lambda d, r, p: [ProviderGuide(
        provider="generic", provider_label="Any DNS provider", steps=[
            RemediationStep(
                "Use an SPF flattening tool (e.g. dmarcly.com/tools/spf-record-checker) "
                "to reduce the lookup count below 10. Alternatively, remove unused "
                "include: mechanisms for sending services you no longer use.",
            ),
        ],
    )],
    "NO_DKIM":             _guides_no_dkim,
    "DKIM_KEY_REVOKED":    lambda d, r, p: [ProviderGuide(
        provider="generic", provider_label="Any DNS provider", steps=[
            RemediationStep(
                "Generate a new DKIM key pair via your mail provider's admin console, "
                "using a new selector name (e.g. include today's date like '20240601').",
            ),
            RemediationStep(
                "Add the new public key at your DNS provider under the new selector, "
                "then activate signing with the new key in your mail provider.",
            ),
        ],
    )],
    "NO_DMARC":            _guides_no_dmarc,
    "DMARC_POLICY_NONE":   _guides_dmarc_policy_none,
    "DMARC_PCT_LOW":       lambda d, r, p: [ProviderGuide(
        provider="generic", provider_label="Any DNS provider", steps=[
            RemediationStep(
                f"Edit the DMARC record at _dmarc.{d} — change pct=<n> to pct=100:",
                command=f"_dmarc IN TXT \"{re.sub(r'pct=\\d+', 'pct=100', r)}\"",
            ),
        ],
    )],
    "DMARC_NO_RUA":        lambda d, r, p: [ProviderGuide(
        provider="generic", provider_label="Any DNS provider", steps=[
            RemediationStep(
                f"Edit the DMARC record at _dmarc.{d} — add a rua= address to "
                "receive aggregate reports. Use an alias you monitor:",
                command=f"_dmarc IN TXT \"{r.rstrip(';').rstrip()} ; rua=mailto:dmarc@{d}\"",
            ),
        ],
    )],
    "NO_MTA_STS":          _guides_mta_sts,
}


def get_remediation(issue: "EmailIssue", domain: str,
                    current_record: str, provider: str) -> list[ProviderGuide]:
    """Return provider-specific remediation guides for an issue."""
    builder = _REMEDIATION_BUILDERS.get(issue.code)
    if not builder:
        return []
    try:
        return builder(domain, current_record, provider)
    except Exception:
        return []


# ── Data model ───────────────────────────────────────────────────────────────

@dataclass
class EmailIssue:
    code: str           # machine-readable identifier, e.g. "NO_SPF"
    severity: str       # "critical" | "high" | "medium" | "low" | "info"
    title: str
    description: str
    record_name: str = ""    # DNS record name to add/change, if applicable
    record_value: str = ""   # suggested record value (generic)

    def to_dict(self) -> dict[str, Any]:
        d: dict[str, Any] = {
            "code": self.code,
            "severity": self.severity,
            "title": self.title,
            "description": self.description,
        }
        if self.record_name:
            d["record_name"] = self.record_name
        if self.record_value:
            d["record_value"] = self.record_value
        return d


@dataclass
class EmailPosture:
    domain: str
    checked_at: float = 0.0

    # Raw record values (empty string = record not found)
    spf_record: str = ""
    dmarc_record: str = ""
    dkim_selector: str = ""     # which selector was found (empty = none found)
    dkim_record: str = ""
    mta_sts_record: str = ""
    bimi_record: str = ""

    issues: list[EmailIssue] = field(default_factory=list)

    # Derived scores
    score: int = 0              # 0–100
    grade: str = "F"            # A / B / C / D / F
    provider: str = ""          # detected mail provider (google / microsoft / other)

    def to_dict(self, include_remediation: bool = False) -> dict[str, Any]:
        issues_out: list[dict[str, Any]] = []
        for issue in self.issues:
            d = issue.to_dict()
            if include_remediation:
                # Pick the relevant current record for this issue
                rec = (
                    self.spf_record if "SPF" in issue.code
                    else self.dmarc_record if "DMARC" in issue.code
                    else self.dkim_record if "DKIM" in issue.code
                    else ""
                )
                guides = get_remediation(issue, self.domain, rec, self.provider)
                d["remediation"] = [g.to_dict() for g in guides]
                d["auto_fix_available"] = any(g.auto_fix_available for g in guides)
            issues_out.append(d)

        return {
            "domain": self.domain,
            "checked_at": self.checked_at,
            "spf_record": self.spf_record,
            "dmarc_record": self.dmarc_record,
            "dkim_selector": self.dkim_selector,
            "dkim_record": self.dkim_record,
            "mta_sts_record": self.mta_sts_record,
            "bimi_record": self.bimi_record,
            "issues": issues_out,
            "score": self.score,
            "grade": self.grade,
            "provider": self.provider,
        }


# ── DNS helpers ───────────────────────────────────────────────────────────────

async def _query_txt(name: str) -> list[str]:
    """Return all TXT record strings for *name*, or [] on failure/NXDOMAIN."""
    try:
        import dns.asyncresolver      # dnspython
        import dns.exception
        try:
            answers = await asyncio.wait_for(
                dns.asyncresolver.resolve(name, "TXT"),
                timeout=DNS_TIMEOUT,
            )
            return [b.decode() for rdata in answers for b in rdata.strings]
        except (dns.exception.DNSException, Exception):
            return []
    except ImportError:
        # Fallback: use the system resolver via getaddrinfo-style hack.
        # This is synchronous but acceptable for infrequent background checks.
        import subprocess
        try:
            loop = asyncio.get_event_loop()
            result = await loop.run_in_executor(
                None,
                lambda: subprocess.run(
                    ["dig", "+short", "TXT", name],
                    capture_output=True, text=True, timeout=DNS_TIMEOUT,
                )
            )
            lines = [
                ln.strip().strip('"') for ln in result.stdout.splitlines()
                if ln.strip() and not ln.startswith(";")
            ]
            return lines
        except Exception:
            return []


async def _query_mx(domain: str) -> list[str]:
    """Return MX hostnames for *domain*."""
    try:
        import dns.asyncresolver
        import dns.exception
        try:
            answers = await asyncio.wait_for(
                dns.asyncresolver.resolve(domain, "MX"),
                timeout=DNS_TIMEOUT,
            )
            return [str(r.exchange).rstrip(".").lower() for r in answers]
        except Exception:
            return []
    except ImportError:
        return []


# ── Check functions ───────────────────────────────────────────────────────────

def _detect_provider(spf: str, mx_hosts: list[str]) -> str:
    """Heuristic: identify mail provider from SPF include and MX records."""
    combined = spf.lower() + " ".join(mx_hosts).lower()
    if "google" in combined or "googlemail" in combined:
        return "google"
    if "outlook" in combined or "protection.outlook" in combined or "microsoft" in combined:
        return "microsoft"
    if "mailchimp" in combined or "mcsv.net" in combined:
        return "mailchimp"
    if "sendgrid" in combined:
        return "sendgrid"
    if "mimecast" in combined:
        return "mimecast"
    if "proofpoint" in combined:
        return "proofpoint"
    return "other"


def _check_spf(records: list[str]) -> tuple[str, list[EmailIssue]]:
    """Analyse SPF TXT records and return (spf_record, issues)."""
    issues: list[EmailIssue] = []
    spf = next((r for r in records if r.startswith("v=spf1")), "")

    if not spf:
        issues.append(EmailIssue(
            code="NO_SPF",
            severity="high",
            title="No SPF record",
            description=(
                "Anyone can send email appearing to come from this domain. "
                "SPF is required for DMARC alignment and basic anti-spoofing."
            ),
            record_name="@",
            record_value="v=spf1 include:<your-mail-provider> -all",
        ))
        return spf, issues

    # Multiple SPF records is invalid (RFC 7208 §3.2)
    spf_records = [r for r in records if r.startswith("v=spf1")]
    if len(spf_records) > 1:
        issues.append(EmailIssue(
            code="MULTIPLE_SPF",
            severity="high",
            title="Multiple SPF records",
            description=(
                f"Found {len(spf_records)} SPF records. RFC 7208 §3.2 requires exactly "
                "one. Receivers may reject or ignore SPF checks entirely."
            ),
        ))

    # Permissive mechanisms
    for perm in _SPF_PERMISSIVE:
        if perm in spf.lower():
            issues.append(EmailIssue(
                code="SPF_PERMISSIVE",
                severity="high",
                title=f"SPF allows all senders ({perm})",
                description=(
                    f"The mechanism '{perm}' allows any host on the internet to send "
                    "mail as this domain. This provides no anti-spoofing protection."
                ),
                record_value=spf.replace(perm, "-all"),
            ))

    # Soft-fail (~all) is better than permissive but weaker than hard-fail
    if "~all" in spf.lower() and not any(i.code == "SPF_PERMISSIVE" for i in issues):
        issues.append(EmailIssue(
            code="SPF_SOFTFAIL",
            severity="low",
            title="SPF uses soft-fail (~all)",
            description=(
                "Soft-fail marks unauthorised senders as suspicious but does not "
                "instruct receivers to reject them. '-all' (hard fail) is recommended."
            ),
            record_value=spf.replace("~all", "-all"),
        ))

    # DNS lookup count — RFC 7208 §4.6.4 limit is 10
    lookup_mechanisms = re.findall(r"\b(?:include|a|mx|exists|redirect)[:=]", spf.lower())
    if len(lookup_mechanisms) > 10:
        issues.append(EmailIssue(
            code="SPF_TOO_MANY_LOOKUPS",
            severity="medium",
            title=f"SPF exceeds 10 DNS lookup limit ({len(lookup_mechanisms)} found)",
            description=(
                "RFC 7208 limits SPF evaluation to 10 DNS lookups. Receivers may "
                "return PermError and treat the domain as having no SPF."
            ),
        ))

    return spf, issues


def _check_dmarc(records: list[str]) -> tuple[str, list[EmailIssue]]:
    """Analyse DMARC TXT records and return (dmarc_record, issues)."""
    issues: list[EmailIssue] = []
    dmarc = next((r for r in records if r.startswith("v=DMARC1")), "")

    if not dmarc:
        issues.append(EmailIssue(
            code="NO_DMARC",
            severity="high",
            title="No DMARC record",
            description=(
                "DMARC is missing. Without it, SPF and DKIM results are not enforced "
                "and spoofed emails are delivered regardless of SPF/DKIM outcome."
            ),
            record_name="_dmarc",
            record_value="v=DMARC1; p=quarantine; rua=mailto:dmarc@<domain>; pct=100",
        ))
        return dmarc, issues

    # Policy strength
    policy_match = re.search(r"\bp=(\w+)", dmarc.lower())
    policy = policy_match.group(1) if policy_match else ""
    if policy in _DMARC_WEAK_POLICIES:
        issues.append(EmailIssue(
            code="DMARC_POLICY_NONE",
            severity="medium",
            title="DMARC policy is 'none' (monitoring only)",
            description=(
                "p=none causes DMARC to report failures but not block them. "
                "Spoofed emails are still delivered to recipients."
            ),
            record_value=dmarc.replace("p=none", "p=quarantine"),
        ))

    # pct < 100
    pct_match = re.search(r"\bpct=(\d+)", dmarc)
    if pct_match and int(pct_match.group(1)) < 100:
        pct = int(pct_match.group(1))
        issues.append(EmailIssue(
            code="DMARC_PCT_LOW",
            severity="low",
            title=f"DMARC pct={pct} — policy applies to only {pct}% of messages",
            description=(
                f"pct={pct} means DMARC policy is only applied to {pct}% of "
                "failing messages. Increase to 100 once you've confirmed alignment."
            ),
            record_value=re.sub(r"pct=\d+", "pct=100", dmarc),
        ))

    # No aggregate reporting address (rua)
    if "rua=" not in dmarc.lower():
        issues.append(EmailIssue(
            code="DMARC_NO_RUA",
            severity="low",
            title="No DMARC aggregate report address (rua)",
            description=(
                "Without rua=, you receive no reports about who is sending mail "
                "as your domain. Aggregate reports are essential for tuning SPF/DKIM "
                "and detecting shadow IT sending on your behalf."
            ),
        ))

    return dmarc, issues


async def _check_dkim(domain: str, extra_selectors: list[str]) -> tuple[str, str, list[EmailIssue]]:
    """
    Probe common selectors for a DKIM public key.
    Returns (selector_found, dkim_record, issues).
    """
    issues: list[EmailIssue] = []
    selectors = list(dict.fromkeys(extra_selectors + _COMMON_DKIM_SELECTORS))

    for selector in selectors:
        name = f"{selector}._domainkey.{domain}"
        records = await _query_txt(name)
        dkim = next((r for r in records if "v=DKIM1" in r or "p=" in r), "")
        if dkim:
            # Check for empty public key (revoked key)
            if re.search(r"\bp=\s*;", dkim) or dkim.rstrip().endswith("p="):
                issues.append(EmailIssue(
                    code="DKIM_KEY_REVOKED",
                    severity="medium",
                    title=f"DKIM key revoked (selector: {selector})",
                    description=(
                        f"The DKIM key for selector '{selector}' has an empty public key "
                        "(p=), which means it is intentionally revoked. Mail signed with "
                        "this selector will fail DKIM verification."
                    ),
                ))
            return selector, dkim, issues

    # No selector found
    issues.append(EmailIssue(
        code="NO_DKIM",
        severity="high",
        title="No DKIM record found",
        description=(
            "No DKIM public key was found for any common selector. "
            "Without DKIM, email from this domain cannot be cryptographically "
            "authenticated and DMARC alignment will fail for DKIM."
        ),
    ))
    return "", "", issues


async def _check_mta_sts(domain: str) -> tuple[str, list[EmailIssue]]:
    """Check for MTA-STS policy record (RFC 8461)."""
    records = await _query_txt(f"_mta-sts.{domain}")
    record = next((r for r in records if r.startswith("v=STSv1")), "")
    issues: list[EmailIssue] = []
    if not record:
        issues.append(EmailIssue(
            code="NO_MTA_STS",
            severity="low",
            title="No MTA-STS policy",
            description=(
                "MTA-STS is not configured. Without it, SMTP connections to your "
                "mail server can be downgraded to unencrypted by a network attacker."
            ),
            record_name="_mta-sts",
            record_value="v=STSv1; id=<YYYYMMDD>",
        ))
    return record, issues


async def _check_bimi(domain: str) -> str:
    """Check for BIMI record (informational — brand indicator)."""
    records = await _query_txt(f"default._bimi.{domain}")
    return next((r for r in records if r.startswith("v=BIMI1")), "")


def _score(posture: EmailPosture) -> tuple[int, str]:
    """
    Compute a 0–100 score and A–F grade from the issues list.

    Deductions:
      critical  → −30 each
      high      → −20 each
      medium    → −10 each
      low       → −5 each
    Bonus:
      MTA-STS present → +5
      BIMI present    → +5
    """
    score = 100
    weights = {"critical": 30, "high": 20, "medium": 10, "low": 5, "info": 0}
    for issue in posture.issues:
        score -= weights.get(issue.severity, 0)

    if posture.mta_sts_record:
        score = min(score + 5, 100)
    if posture.bimi_record:
        score = min(score + 5, 100)

    score = max(score, 0)
    if score >= 90:
        grade = "A"
    elif score >= 75:
        grade = "B"
    elif score >= 60:
        grade = "C"
    elif score >= 40:
        grade = "D"
    else:
        grade = "F"

    return score, grade


# ── Public check entry point ──────────────────────────────────────────────────

async def check_domain(domain: str,
                       extra_dkim_selectors: list[str] | None = None) -> EmailPosture:
    """
    Run all Phase 1 email security checks for *domain*.

    This is the primary callable for on-demand checks (API) and the scheduled
    scan loop.  All DNS queries run concurrently.
    """
    domain = domain.lower().strip().rstrip(".")
    extra_dkim_selectors = extra_dkim_selectors or []

    # Fire all DNS checks concurrently
    domain_txt_task = asyncio.create_task(_query_txt(domain))
    dmarc_txt_task  = asyncio.create_task(_query_txt(f"_dmarc.{domain}"))
    mta_sts_task    = asyncio.create_task(_check_mta_sts(domain))
    bimi_task       = asyncio.create_task(_check_bimi(domain))
    mx_task         = asyncio.create_task(_query_mx(domain))
    dkim_task       = asyncio.create_task(_check_dkim(domain, extra_dkim_selectors))

    domain_txt    = await domain_txt_task
    dmarc_records = await dmarc_txt_task
    mx_hosts      = await mx_task
    mta_sts_record, mta_sts_issues = await mta_sts_task
    bimi_record   = await bimi_task
    dkim_selector, dkim_record, dkim_issues = await dkim_task

    spf_record, spf_issues = _check_spf(domain_txt)
    dmarc_record, dmarc_issues = _check_dmarc(dmarc_records)
    provider = _detect_provider(spf_record, mx_hosts)

    issues = spf_issues + dkim_issues + dmarc_issues + mta_sts_issues

    posture = EmailPosture(
        domain=domain,
        checked_at=time.time(),
        spf_record=spf_record,
        dmarc_record=dmarc_record,
        dkim_selector=dkim_selector,
        dkim_record=dkim_record,
        mta_sts_record=mta_sts_record,
        bimi_record=bimi_record,
        issues=issues,
        provider=provider,
    )
    posture.score, posture.grade = _score(posture)
    return posture


# ── Manager ───────────────────────────────────────────────────────────────────

class EmailSecurityMonitor:
    """
    Manages periodic email security checks across a set of domains.

    Domains are added via add_domain() / remove_domain().  Results are cached
    and re-checked on RECHECK_INTERVAL.  The alerts callback is fired when a
    domain's status changes (new issues or issues resolved).
    """

    def __init__(self,
                 on_alert: Any | None = None,
                 recheck_interval: float = RECHECK_INTERVAL) -> None:
        """
        on_alert(domain, posture) is called whenever a check completes and
        there are high/critical issues, or when a domain goes from bad to good.
        """
        self._domains: dict[str, list[str]] = {}   # domain → extra DKIM selectors
        self._results: dict[str, EmailPosture] = {}
        self._on_alert = on_alert
        self._recheck_interval = recheck_interval
        self._task: asyncio.Task | None = None

    async def start(self) -> None:
        self._task = asyncio.create_task(self._scan_loop(), name="email-security-scan")

    async def stop(self) -> None:
        if self._task:
            self._task.cancel()
            try:
                await self._task
            except asyncio.CancelledError:
                pass

    def add_domain(self, domain: str,
                   dkim_selectors: list[str] | None = None) -> None:
        """Register a domain for monitoring."""
        self._domains[domain.lower().strip()] = dkim_selectors or []

    def remove_domain(self, domain: str) -> None:
        domain = domain.lower().strip()
        self._domains.pop(domain, None)
        self._results.pop(domain, None)

    def list_domains(self) -> list[str]:
        return list(self._domains)

    def get_result(self, domain: str) -> EmailPosture | None:
        return self._results.get(domain.lower().strip())

    def get_all_results(self) -> list[dict[str, Any]]:
        return [p.to_dict() for p in self._results.values()]

    async def check_now(self, domain: str) -> EmailPosture:
        """Run an immediate check for a domain (bypasses cache)."""
        domain = domain.lower().strip()
        selectors = self._domains.get(domain, [])
        posture = await check_domain(domain, selectors)
        prev = self._results.get(domain)
        self._results[domain] = posture
        await self._maybe_alert(domain, posture, prev)
        return posture

    # ── Internal ─────────────────────────────────────────────────────────────

    async def _scan_loop(self) -> None:
        await asyncio.sleep(INITIAL_DELAY)
        while True:
            for domain in list(self._domains):
                try:
                    prev = self._results.get(domain)
                    # Skip re-check if result is fresh
                    if prev and (time.time() - prev.checked_at) < self._recheck_interval:
                        continue
                    posture = await check_domain(domain, self._domains.get(domain, []))
                    self._results[domain] = posture
                    await self._maybe_alert(domain, posture, prev)
                    log.info(
                        "Email posture %s: score=%d grade=%s issues=%d",
                        domain, posture.score, posture.grade, len(posture.issues),
                    )
                except asyncio.CancelledError:
                    raise
                except Exception as e:
                    log.warning("Email security check failed for %s: %s", domain, e)
                # Small gap between domains to avoid DNS rate limiting
                await asyncio.sleep(2.0)
            await asyncio.sleep(60.0)  # idle wait before next pass

    async def _maybe_alert(self,
                           domain: str,
                           posture: EmailPosture,
                           prev: EmailPosture | None) -> None:
        if not self._on_alert:
            return

        high_issues = [i for i in posture.issues if i.severity in ("critical", "high")]
        prev_high = [i for i in (prev.issues if prev else []) if i.severity in ("critical", "high")]

        # Alert if new high/critical issues appeared, or if status improved to clean
        new_codes = {i.code for i in high_issues} - {i.code for i in prev_high}
        resolved_codes = {i.code for i in prev_high} - {i.code for i in high_issues}

        if new_codes or (resolved_codes and not high_issues):
            try:
                await self._on_alert(domain, posture)
            except Exception as e:
                log.debug("email security alert callback error: %s", e)
