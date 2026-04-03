"""
SaaS Application Management — discovery, governance, cost optimisation, and
offboarding for cloud software-as-a-service applications.

Discovery sources:
 - Authentik OAuth application grants
 - M365 Graph API (oauth2PermissionGrants)
 - Google Workspace Admin SDK (tokens.list)
 - osquery chrome_extensions results (via job results from nodes)
 - DNS query log correlation (via mesh_network query)
 - Email invoice parsing (via cloud_backup integration)
 - Manual registration through the API

All sources feed into a unified SaaSApplication inventory. Duplicate entries
from different discovery sources are merged. Shadow IT alerts fire when
previously-unseen tools are detected.

Persistence: saas_data/apps.json
Events emitted: saas.app.discovered, saas.app.updated, saas.shadow_it.alert,
                saas.renewal.alert, saas.offboarding.required
"""

from __future__ import annotations

import asyncio
import json
import logging
import time
import uuid
from dataclasses import dataclass, field
from datetime import date, datetime
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.saas_management")


# ── Data model ────────────────────────────────────────────────────────────────

# SaaS application category tags
CATEGORIES = {
    "CRM": ["salesforce", "hubspot", "pipedrive", "zoho"],
    "Communication": ["slack", "teams", "zoom", "webex", "discord", "meet"],
    "Productivity": ["notion", "confluence", "asana", "monday", "trello", "jira", "linear"],
    "Storage": ["dropbox", "box", "google drive", "onedrive", "sharepoint"],
    "HR": ["workday", "bamboohr", "rippling", "gusto", "lattice"],
    "Finance": ["xero", "quickbooks", "stripe", "expensify", "bill.com"],
    "DevOps": ["github", "gitlab", "bitbucket", "circleci", "datadog", "pagerduty", "sentry"],
    "Security": ["1password", "okta", "duo", "crowdstrike", "knowbe4"],
    "Analytics": ["tableau", "looker", "metabase", "amplitude", "mixpanel"],
    "Email": ["gmail", "outlook", "mailchimp", "sendgrid"],
    "Cloud": ["aws", "azure", "gcp", "cloudflare", "digitalocean"],
}

# Discovery source labels
SOURCE_AUTHENTIK = "authentik_oauth"
SOURCE_M365 = "m365_oauth_grants"
SOURCE_GOOGLE = "google_workspace_tokens"
SOURCE_OSQUERY = "osquery_chrome_extensions"
SOURCE_DNS = "dns_query_logs"
SOURCE_EMAIL = "email_invoice_parsing"
SOURCE_MANUAL = "manual"

# Renewal alert thresholds (days before renewal)
RENEWAL_ALERT_DAYS = [90, 60, 30, 7]


@dataclass
class ManagedSaaSApp:
    id: str
    name: str                           # "Slack", "Salesforce"
    vendor: str                         # "Salesforce, Inc."
    category: str = "Other"             # CRM | Productivity | etc.
    domain: str = ""                    # "slack.com"
    discovery_sources: list[str] = field(default_factory=list)
    users: list[str] = field(default_factory=list)  # user emails with access
    active_users_30d: int = 0
    seats_licensed: int | None = None
    seats_active: int = 0
    monthly_cost: float | None = None   # USD, from invoice parsing or manual
    renewal_date: str | None = None     # ISO date string "2026-09-01"
    data_categories: list[str] = field(default_factory=list)  # email, files, PII, financial
    vendor_soc2: bool | None = None
    vendor_gdpr: bool | None = None
    sso_integrated: bool = False
    mfa_enforced: bool | None = None
    approved: bool = False              # False = shadow IT
    dpa_signed: bool | None = None      # Data Processing Agreement
    notes: str = ""
    first_seen: float = 0.0
    last_seen: float = 0.0
    last_activity: float = 0.0
    offboarding_tasks: list[dict] = field(default_factory=list)
    # {"type": "api_revoke" | "manual", "user": "...", "done": False, "done_at": None}

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id,
            "name": self.name,
            "vendor": self.vendor,
            "category": self.category,
            "domain": self.domain,
            "discovery_sources": self.discovery_sources,
            "users": self.users,
            "active_users_30d": self.active_users_30d,
            "seats_licensed": self.seats_licensed,
            "seats_active": self.seats_active,
            "monthly_cost": self.monthly_cost,
            "renewal_date": self.renewal_date,
            "data_categories": self.data_categories,
            "vendor_soc2": self.vendor_soc2,
            "vendor_gdpr": self.vendor_gdpr,
            "sso_integrated": self.sso_integrated,
            "mfa_enforced": self.mfa_enforced,
            "approved": self.approved,
            "dpa_signed": self.dpa_signed,
            "notes": self.notes,
            "first_seen": self.first_seen,
            "last_seen": self.last_seen,
            "last_activity": self.last_activity,
            "offboarding_tasks": self.offboarding_tasks,
        }

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> "ManagedSaaSApp":
        return cls(
            id=d["id"],
            name=d["name"],
            vendor=d.get("vendor", d["name"]),
            category=d.get("category", "Other"),
            domain=d.get("domain", ""),
            discovery_sources=d.get("discovery_sources", []),
            users=d.get("users", []),
            active_users_30d=d.get("active_users_30d", 0),
            seats_licensed=d.get("seats_licensed"),
            seats_active=d.get("seats_active", 0),
            monthly_cost=d.get("monthly_cost"),
            renewal_date=d.get("renewal_date"),
            data_categories=d.get("data_categories", []),
            vendor_soc2=d.get("vendor_soc2"),
            vendor_gdpr=d.get("vendor_gdpr"),
            sso_integrated=d.get("sso_integrated", False),
            mfa_enforced=d.get("mfa_enforced"),
            approved=d.get("approved", False),
            dpa_signed=d.get("dpa_signed"),
            notes=d.get("notes", ""),
            first_seen=d.get("first_seen", 0.0),
            last_seen=d.get("last_seen", 0.0),
            last_activity=d.get("last_activity", 0.0),
            offboarding_tasks=d.get("offboarding_tasks", []),
        )

    @property
    def shadow_it(self) -> bool:
        return not self.approved

    @property
    def unused_seats(self) -> int | None:
        if self.seats_licensed is not None and self.seats_active >= 0:
            return max(0, self.seats_licensed - self.seats_active)
        return None

    @property
    def annual_cost(self) -> float | None:
        return self.monthly_cost * 12 if self.monthly_cost is not None else None

    @property
    def days_until_renewal(self) -> int | None:
        if not self.renewal_date:
            return None
        try:
            rd = date.fromisoformat(self.renewal_date)
            return (rd - date.today()).days
        except ValueError:
            return None

    @property
    def renewal_risk(self) -> str:
        """upcoming | ok | overdue | unknown"""
        d = self.days_until_renewal
        if d is None:
            return "unknown"
        if d < 0:
            return "overdue"
        if d <= 30:
            return "upcoming"
        return "ok"


@dataclass
class SaaSConfig:
    # Auto-approve apps from these domains (likely internal tooling)
    trusted_domains: list[str] = field(default_factory=list)
    # Domains to exclude from discovery (noise)
    excluded_domains: list[str] = field(default_factory=list)
    # Fire shadow IT alert as soon as unapproved app is seen
    shadow_it_alerts: bool = True
    # Days of inactivity before marking seats as inactive
    inactive_seat_days: int = 30
    # Poll interval for M365/Google grant discovery (seconds; 0 = manual only)
    discovery_interval: int = 86400  # 24h
    # Renewal alert lookahead (days)
    renewal_alert_days: list[int] = field(default_factory=lambda: list(RENEWAL_ALERT_DAYS))

    def to_dict(self) -> dict[str, Any]:
        return {
            "trusted_domains": self.trusted_domains,
            "excluded_domains": self.excluded_domains,
            "shadow_it_alerts": self.shadow_it_alerts,
            "inactive_seat_days": self.inactive_seat_days,
            "discovery_interval": self.discovery_interval,
            "renewal_alert_days": self.renewal_alert_days,
        }

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> "SaaSConfig":
        return cls(
            trusted_domains=d.get("trusted_domains", []),
            excluded_domains=d.get("excluded_domains", []),
            shadow_it_alerts=d.get("shadow_it_alerts", True),
            inactive_seat_days=d.get("inactive_seat_days", 30),
            discovery_interval=d.get("discovery_interval", 86400),
            renewal_alert_days=d.get("renewal_alert_days", list(RENEWAL_ALERT_DAYS)),
        )


# ── Category inference ────────────────────────────────────────────────────────

def _infer_category(name: str, domain: str) -> str:
    text = (name + " " + domain).lower()
    for cat, keywords in CATEGORIES.items():
        for kw in keywords:
            if kw in text:
                return cat
    return "Other"


def _normalise_name(name: str) -> str:
    """Return a lowercase key for deduplication."""
    return name.lower().strip()


# ── Manager ───────────────────────────────────────────────────────────────────

class SaaSManager:
    """
    Manages the SaaS application inventory.

    All public methods are synchronous unless they need to interact with
    external APIs (discovery methods are async). The inventory itself is
    always in-memory; persistence happens on every mutation.
    """

    def __init__(self, data_dir: Path, event_queue: asyncio.Queue | None = None) -> None:
        self._data_dir = data_dir
        self._data_dir.mkdir(parents=True, exist_ok=True)
        self._apps: dict[str, ManagedSaaSApp] = {}   # id → app
        self._name_index: dict[str, str] = {}          # normalised name → id
        self._config = SaaSConfig()
        self._event_queue = event_queue
        self._last_discovery: float = 0.0
        self._task: asyncio.Task | None = None
        # Injected by main.py after construction; avoid circular imports
        self.cloud_backup = None   # CloudBackupManager | None

    # ── Lifecycle ─────────────────────────────────────────────────────────────

    async def start(self) -> None:
        self._load()
        self._task = asyncio.create_task(self._background_loop(), name="saas_manager")

    async def stop(self) -> None:
        if self._task:
            self._task.cancel()
            try:
                await self._task
            except asyncio.CancelledError:
                pass

    # ── Persistence ───────────────────────────────────────────────────────────

    def _apps_path(self) -> Path:
        return self._data_dir / "apps.json"

    def _config_path(self) -> Path:
        return self._data_dir / "config.json"

    def _load(self) -> None:
        if self._apps_path().exists():
            try:
                data = json.loads(self._apps_path().read_text())
                for d in data.get("apps", []):
                    app = ManagedSaaSApp.from_dict(d)
                    self._apps[app.id] = app
                    self._name_index[_normalise_name(app.name)] = app.id
                self._last_discovery = data.get("last_discovery", 0.0)
            except Exception as e:
                log.error("Failed to load saas apps.json: %s", e)
        if self._config_path().exists():
            try:
                self._config = SaaSConfig.from_dict(
                    json.loads(self._config_path().read_text())
                )
            except Exception as e:
                log.error("Failed to load saas config.json: %s", e)

    def _save(self) -> None:
        apps_data = {
            "apps": [a.to_dict() for a in self._apps.values()],
            "last_discovery": self._last_discovery,
        }
        tmp = self._apps_path().with_suffix(".tmp")
        tmp.write_text(json.dumps(apps_data, indent=2))
        tmp.replace(self._apps_path())

    def _save_config(self) -> None:
        tmp = self._config_path().with_suffix(".tmp")
        tmp.write_text(json.dumps(self._config.to_dict(), indent=2))
        tmp.replace(self._config_path())

    # ── Config ────────────────────────────────────────────────────────────────

    def get_config(self) -> SaaSConfig:
        return self._config

    def set_config(self, updates: dict[str, Any]) -> SaaSConfig:
        d = self._config.to_dict()
        d.update(updates)
        self._config = SaaSConfig.from_dict(d)
        self._save_config()
        return self._config

    # ── CRUD ──────────────────────────────────────────────────────────────────

    def register_app(
        self,
        name: str,
        vendor: str = "",
        domain: str = "",
        source: str = SOURCE_MANUAL,
        **kwargs: Any,
    ) -> ManagedSaaSApp:
        """
        Register a new app or merge into an existing one (by normalised name).
        Returns the app record.
        """
        norm = _normalise_name(name)
        now = time.time()
        if norm in self._name_index:
            app = self._apps[self._name_index[norm]]
            # Merge discovery source
            if source and source not in app.discovery_sources:
                app.discovery_sources.append(source)
            app.last_seen = now
            for k, v in kwargs.items():
                if hasattr(app, k) and v is not None:
                    setattr(app, k, v)
            if domain and not app.domain:
                app.domain = domain
            if vendor and not app.vendor:
                app.vendor = vendor
            self._save()
            return app

        # New app
        app = ManagedSaaSApp(
            id=str(uuid.uuid4()),
            name=name,
            vendor=vendor or name,
            domain=domain,
            category=_infer_category(name, domain),
            discovery_sources=[source] if source else [],
            first_seen=now,
            last_seen=now,
        )
        for k, v in kwargs.items():
            if hasattr(app, k) and v is not None:
                setattr(app, k, v)
        self._apps[app.id] = app
        self._name_index[norm] = app.id
        self._save()
        self._schedule_event("saas.app.discovered", app.to_dict())
        if self._config.shadow_it_alerts and not app.approved:
            self._schedule_event("saas.shadow_it.alert", {
                "app_id": app.id, "name": app.name, "source": source,
            })
        return app

    def get_app(self, app_id: str) -> ManagedSaaSApp | None:
        return self._apps.get(app_id)

    def list_apps(
        self,
        shadow_it: bool | None = None,
        category: str | None = None,
        renewal_risk: str | None = None,
        source: str | None = None,
    ) -> list[ManagedSaaSApp]:
        apps = list(self._apps.values())
        if shadow_it is not None:
            apps = [a for a in apps if a.shadow_it == shadow_it]
        if category:
            apps = [a for a in apps if a.category == category]
        if renewal_risk:
            apps = [a for a in apps if a.renewal_risk == renewal_risk]
        if source:
            apps = [a for a in apps if source in a.discovery_sources]
        return sorted(apps, key=lambda a: a.name.lower())

    def update_app(self, app_id: str, updates: dict[str, Any]) -> ManagedSaaSApp | None:
        app = self._apps.get(app_id)
        if not app:
            return None
        was_shadow = app.shadow_it
        for k, v in updates.items():
            if hasattr(app, k):
                setattr(app, k, v)
        app.last_seen = time.time()
        self._save()
        # Rebuild name index if name changed
        if "name" in updates:
            self._name_index = {_normalise_name(a.name): a.id for a in self._apps.values()}
        if was_shadow and not app.shadow_it:
            self._schedule_event("saas.app.updated", {
                "app_id": app.id, "change": "approved",
            })
        return app

    def delete_app(self, app_id: str) -> bool:
        app = self._apps.pop(app_id, None)
        if not app:
            return False
        self._name_index.pop(_normalise_name(app.name), None)
        self._save()
        return True

    # ── User tracking ─────────────────────────────────────────────────────────

    def add_user_access(self, app_id: str, user_email: str) -> bool:
        app = self._apps.get(app_id)
        if not app:
            return False
        if user_email not in app.users:
            app.users.append(user_email)
        app.seats_active = len(app.users)
        app.last_seen = time.time()
        self._save()
        return True

    def remove_user_access(self, app_id: str, user_email: str) -> bool:
        app = self._apps.get(app_id)
        if not app:
            return False
        if user_email in app.users:
            app.users.remove(user_email)
        app.seats_active = len(app.users)
        self._save()
        return True

    def user_apps(self, user_email: str) -> list[ManagedSaaSApp]:
        """All apps a user has access to — used for offboarding checklist."""
        return [a for a in self._apps.values() if user_email in a.users]

    # ── Offboarding ───────────────────────────────────────────────────────────

    def create_offboarding_checklist(self, user_email: str) -> dict[str, Any]:
        """
        Generate an offboarding task list for a departing user.
        Returns tasks grouped by revocation method.
        """
        apps = self.user_apps(user_email)
        api_revocable: list[dict] = []
        manual_required: list[dict] = []

        for app in apps:
            task = {"app_id": app.id, "app_name": app.name, "user": user_email}
            if app.sso_integrated:
                # SSO apps are revoked by disabling the IdP account — no per-app action needed
                task["method"] = "sso_disable_account"
                api_revocable.append(task)
            else:
                task["method"] = "manual"
                task["instructions"] = (
                    f"Log into {app.name} admin panel and remove {user_email}"
                )
                manual_required.append(task)

        return {
            "user": user_email,
            "total_apps": len(apps),
            "api_revocable": api_revocable,
            "manual_required": manual_required,
            "generated_at": time.time(),
        }

    # ── Analytics ─────────────────────────────────────────────────────────────

    def shadow_it_summary(self) -> dict[str, Any]:
        shadow = [a for a in self._apps.values() if a.shadow_it]
        return {
            "count": len(shadow),
            "apps": [{"id": a.id, "name": a.name, "first_seen": a.first_seen,
                      "users": len(a.users), "source": a.discovery_sources}
                     for a in shadow],
        }

    def cost_summary(self) -> dict[str, Any]:
        apps_with_cost = [a for a in self._apps.values() if a.monthly_cost is not None]
        total_monthly = sum(a.monthly_cost for a in apps_with_cost if a.monthly_cost)
        wasted_monthly = sum(
            (a.monthly_cost / a.seats_licensed * (a.unused_seats or 0))
            for a in apps_with_cost
            if a.seats_licensed and a.unused_seats and a.monthly_cost
        )
        return {
            "total_monthly_cost": round(total_monthly, 2),
            "total_annual_cost": round(total_monthly * 12, 2),
            "estimated_wasted_monthly": round(wasted_monthly, 2),
            "apps_with_cost_data": len(apps_with_cost),
            "apps_without_cost_data": len(self._apps) - len(apps_with_cost),
        }

    def upcoming_renewals(self, days: int = 90) -> list[dict[str, Any]]:
        results = []
        for app in self._apps.values():
            d = app.days_until_renewal
            if d is not None and 0 <= d <= days:
                results.append({
                    "app_id": app.id,
                    "name": app.name,
                    "renewal_date": app.renewal_date,
                    "days_until_renewal": d,
                    "monthly_cost": app.monthly_cost,
                    "annual_cost": app.annual_cost,
                    "seats_licensed": app.seats_licensed,
                    "seats_active": app.seats_active,
                    "unused_seats": app.unused_seats,
                })
        return sorted(results, key=lambda x: x["days_until_renewal"])

    def duplicate_categories(self) -> list[dict[str, Any]]:
        """Return categories with more than one approved app — potential duplicates."""
        from collections import defaultdict
        by_cat: dict[str, list[ManagedSaaSApp]] = defaultdict(list)
        for app in self._apps.values():
            if app.approved and app.category != "Other":
                by_cat[app.category].append(app)
        return [
            {
                "category": cat,
                "count": len(apps),
                "apps": [{"id": a.id, "name": a.name, "monthly_cost": a.monthly_cost}
                         for a in apps],
            }
            for cat, apps in by_cat.items()
            if len(apps) > 1
        ]

    def vendor_risk_summary(self) -> list[dict[str, Any]]:
        """Apps missing SOC 2, GDPR adequacy, or DPA."""
        risks = []
        for app in self._apps.values():
            gaps = []
            if app.vendor_soc2 is False:
                gaps.append("no_soc2")
            if app.vendor_gdpr is False:
                gaps.append("no_gdpr_adequacy")
            if app.dpa_signed is False:
                gaps.append("dpa_not_signed")
            if "PII" in app.data_categories and not app.dpa_signed:
                gaps.append("pii_without_dpa")
            if gaps:
                risks.append({
                    "app_id": app.id,
                    "name": app.name,
                    "gaps": gaps,
                    "data_categories": app.data_categories,
                })
        return risks

    # ── Discovery: M365 OAuth grants ─────────────────────────────────────────

    async def discover_m365_oauth_grants(
        self, access_token: str
    ) -> list[ManagedSaaSApp]:
        """
        Query Microsoft Graph API for all OAuth permission grants in the tenant.
        access_token: a Graph API token with Policy.Read.All or Directory.Read.All.
        Returns newly-registered apps.
        """
        try:
            import urllib.request
            import urllib.error
        except ImportError:
            log.warning("urllib not available")
            return []

        discovered: list[ManagedSaaSApp] = []
        url = "https://graph.microsoft.com/v1.0/oauth2PermissionGrants?$top=100"
        headers = {"Authorization": f"Bearer {access_token}", "Accept": "application/json"}

        while url:
            try:
                req = urllib.request.Request(url, headers=headers)
                with urllib.request.urlopen(req, timeout=30) as resp:
                    data = json.loads(resp.read())
            except Exception as e:
                log.error("M365 OAuth grants query failed: %s", e)
                break

            for grant in data.get("value", []):
                # clientId is the service principal id of the app
                # We use the resourceId display name as the app name
                resource_id = grant.get("resourceId", "")
                client_id = grant.get("clientId", "")
                principal_id = grant.get("principalId", "")  # user if delegated
                scopes = grant.get("scope", "")
                consent_type = grant.get("consentType", "")

                # Try to look up the service principal name
                app_name = grant.get("_resource_display_name", resource_id[:8])
                app = self.register_app(
                    name=app_name,
                    source=SOURCE_M365,
                    approved=False,  # starts as shadow IT; admin can approve
                )
                if principal_id and consent_type == "Principal":
                    self.add_user_access(app.id, principal_id)
                if scopes:
                    # Infer data categories from scopes
                    cats = _scopes_to_data_categories(scopes)
                    if cats:
                        merged = list(set(app.data_categories + cats))
                        self.update_app(app.id, {"data_categories": merged})
                discovered.append(app)

            url = data.get("@odata.nextLink")

        self._last_discovery = time.time()
        self._save()
        log.info("M365 OAuth grant discovery: %d apps", len(discovered))
        return discovered

    # ── Discovery: Google Workspace tokens ───────────────────────────────────

    async def discover_google_workspace_tokens(
        self, admin_credentials: dict[str, Any]
    ) -> list[ManagedSaaSApp]:
        """
        Query Google Workspace Admin SDK tokens.list for all OAuth app grants.
        admin_credentials: service account key dict with domain-wide delegation.
        Returns newly-registered apps.
        """
        # This requires google-auth + googleapiclient. We import lazily so that
        # controllers without these packages don't crash on startup.
        try:
            from google.oauth2 import service_account
            from googleapiclient.discovery import build
        except ImportError:
            log.warning("google-auth / google-api-python-client not installed; "
                        "Google Workspace token discovery unavailable")
            return []

        discovered: list[ManagedSaaSApp] = []
        try:
            scopes = ["https://www.googleapis.com/auth/admin.directory.user.security"]
            creds = service_account.Credentials.from_service_account_info(
                admin_credentials, scopes=scopes
            ).with_subject(admin_credentials.get("admin_email", ""))
            service = build("admin", "directory_v1", credentials=creds, cache_discovery=False)

            # List all users first
            users_result = service.users().list(customer="my_customer", maxResults=200).execute()
            for user in users_result.get("users", []):
                user_email = user.get("primaryEmail", "")
                try:
                    tokens_result = service.tokens().list(userKey=user_email).execute()
                    for token in tokens_result.get("items", []):
                        display_name = token.get("displayText", token.get("clientId", "unknown"))
                        scopes = " ".join(token.get("scopes", []))
                        app = self.register_app(
                            name=display_name,
                            source=SOURCE_GOOGLE,
                            approved=False,
                        )
                        self.add_user_access(app.id, user_email)
                        cats = _scopes_to_data_categories(scopes)
                        if cats:
                            merged = list(set(app.data_categories + cats))
                            self.update_app(app.id, {"data_categories": merged})
                        discovered.append(app)
                except Exception as e:
                    log.debug("Failed to list tokens for %s: %s", user_email, e)
        except Exception as e:
            log.error("Google Workspace token discovery failed: %s", e)

        self._last_discovery = time.time()
        self._save()
        log.info("Google Workspace token discovery: %d app grants", len(discovered))
        return discovered

    # ── Discovery: osquery chrome_extensions ─────────────────────────────────

    def ingest_chrome_extensions(
        self, node_id: str, extensions: list[dict[str, Any]]
    ) -> int:
        """
        Process osquery chrome_extensions results from a node agent job.
        extensions: list of osquery chrome_extensions rows.
        Returns count of new apps discovered.
        """
        new_count = 0
        for ext in extensions:
            name = ext.get("name") or ext.get("identifier", "")
            if not name:
                continue
            # Filter out obvious non-SaaS extensions (ad blockers, themes, etc.)
            if any(skip in name.lower() for skip in [
                "dark mode", "theme", "ad block", "ublock", "password",
                "grammar", "translator", "capture",
            ]):
                continue
            existing_norm = _normalise_name(name)
            if existing_norm not in self._name_index:
                new_count += 1
            self.register_app(name=name, source=SOURCE_OSQUERY)
        return new_count

    # ── Discovery: DNS query log correlation ─────────────────────────────────

    def ingest_dns_domains(self, domains: list[str]) -> int:
        """
        Process external domains from DNS query logs.
        Registers SaaS apps for known domains not already in inventory.
        Returns count of new apps discovered.
        """
        new_count = 0
        for domain in domains:
            # Strip common prefixes
            root = domain.lower()
            for prefix in ("www.", "app.", "api.", "mail.", "auth.", "login."):
                if root.startswith(prefix):
                    root = root[len(prefix):]
                    break

            if root in self._config.excluded_domains:
                continue
            # Check if any existing app already has this domain
            existing = next(
                (a for a in self._apps.values() if a.domain == root), None
            )
            if not existing:
                # Infer app name from domain (e.g. "slack.com" → "Slack")
                parts = root.split(".")
                name = parts[0].title() if parts else root
                new_count += 1
                self.register_app(name=name, domain=root, source=SOURCE_DNS)
        return new_count

    # ── Discovery: email invoice parsing ─────────────────────────────────────

    def ingest_invoice_data(self, invoices: list[dict[str, Any]]) -> int:
        """
        Process parsed invoice records from cloud_backup email parsing.
        Each invoice dict: {vendor, amount_usd, period_start, renewal_date, currency}
        Returns count of apps updated with cost data.
        """
        updated = 0
        for inv in invoices:
            vendor = inv.get("vendor", "")
            if not vendor:
                continue
            norm = _normalise_name(vendor)
            app_id = self._name_index.get(norm)
            if not app_id:
                app = self.register_app(
                    name=vendor, source=SOURCE_EMAIL,
                    monthly_cost=inv.get("amount_usd"),
                    renewal_date=inv.get("renewal_date"),
                )
                updated += 1
            else:
                updates: dict[str, Any] = {}
                if inv.get("amount_usd") is not None:
                    updates["monthly_cost"] = inv["amount_usd"]
                if inv.get("renewal_date"):
                    updates["renewal_date"] = inv["renewal_date"]
                if updates:
                    self.update_app(app_id, updates)
                    updated += 1
        return updated

    # ── Background tasks ─────────────────────────────────────────────────────

    async def _background_loop(self) -> None:
        while True:
            try:
                await asyncio.sleep(3600)  # check every hour
                now = time.time()
                if (self._config.discovery_interval > 0 and
                        now - self._last_discovery >= self._config.discovery_interval):
                    log.debug("SaaS scheduled discovery cycle")
                    # Scheduled external discoveries require credentials to be
                    # provided by the caller; the background loop only handles
                    # lightweight internal checks.
                    await self._check_renewals()
            except asyncio.CancelledError:
                return
            except Exception as e:
                log.error("SaaS background loop error: %s", e)

    async def _check_renewals(self) -> None:
        for app in self._apps.values():
            d = app.days_until_renewal
            if d is None:
                continue
            for threshold in self._config.renewal_alert_days:
                # Fire alert when we cross the threshold (within ±1 hour window)
                if 0 <= d <= threshold:
                    await self._fire_event("saas.renewal.alert", {
                        "app_id": app.id,
                        "name": app.name,
                        "days_until_renewal": d,
                        "renewal_date": app.renewal_date,
                        "annual_cost": app.annual_cost,
                        "unused_seats": app.unused_seats,
                    })
                    break  # Only fire for the most urgent threshold

    # ── Events ────────────────────────────────────────────────────────────────

    def _schedule_event(self, event_type: str, data: dict) -> None:
        """Fire an event synchronously (non-blocking). Safe in any context."""
        if self._event_queue:
            try:
                self._event_queue.put_nowait({"type": event_type, "data": data})
            except asyncio.QueueFull:
                pass

    async def _fire_event(self, event_type: str, data: dict) -> None:
        if self._event_queue:
            await self._event_queue.put({"type": event_type, "data": data})

    # ── Status ────────────────────────────────────────────────────────────────

    def status(self) -> dict[str, Any]:
        all_apps = list(self._apps.values())
        approved = [a for a in all_apps if a.approved]
        shadow = [a for a in all_apps if a.shadow_it]
        with_cost = [a for a in all_apps if a.monthly_cost is not None]
        total_monthly = sum(a.monthly_cost for a in with_cost if a.monthly_cost)
        upcoming = self.upcoming_renewals(days=30)
        return {
            "total_apps": len(all_apps),
            "approved": len(approved),
            "shadow_it": len(shadow),
            "apps_with_cost_data": len(with_cost),
            "total_monthly_cost": round(total_monthly, 2),
            "renewals_next_30d": len(upcoming),
            "last_discovery": self._last_discovery,
            "vendor_risk_count": len(self.vendor_risk_summary()),
        }


# ── Scope → data category inference ──────────────────────────────────────────

def _scopes_to_data_categories(scopes: str) -> list[str]:
    """Infer data categories from OAuth scope strings."""
    cats: list[str] = []
    s = scopes.lower()
    if any(x in s for x in ["mail", "email", "message"]):
        cats.append("email")
    if any(x in s for x in ["files", "drive", "document", "sheet"]):
        cats.append("files")
    if any(x in s for x in ["user.read", "profile", "openid", "people"]):
        cats.append("PII")
    if any(x in s for x in ["calendar", "events"]):
        cats.append("calendar")
    if any(x in s for x in ["contacts", "directory"]):
        cats.append("contacts")
    if any(x in s for x in ["financial", "payment", "billing"]):
        cats.append("financial")
    return list(set(cats))
