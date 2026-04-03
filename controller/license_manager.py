# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Software Asset Management (SAM) and license tracking.

Two layers:

1. On-premise / endpoint software
   LicensedProduct tracks seats purchased, seats active (from agent inventory),
   key management, renewal dates, and per-node installation records.

2. SaaS applications
   SaaSApplication tracks every cloud tool in use — discovered via OAuth grants,
   DNS (MX/SPF/CNAME toolmarks), or manually registered.  Tracks seats,
   cost, renewal, vendor risk, shadow IT status, offboarding checklists.

Combined coverage:
  - Installed vs. licensed discrepancy detection (flag over-deployed or wasted)
  - Renewal calendar: alerts 90 / 30 / 7 days before expiry
  - Shadow IT alerts: unapproved tools, no DPA, ex-employee access
  - Cost optimisation: unused seats, duplicate tools by category
  - Offboarding: which apps need manual revocation

All data persisted to license_data/ directory as JSON.
Integrates with NotificationManager for renewal and alert events.
"""

from __future__ import annotations

import asyncio
import json
import logging
import time
import uuid
from dataclasses import dataclass, field
from enum import Enum
from pathlib import Path
from typing import Any, Callable, Awaitable

log = logging.getLogger("ozma.license_manager")

DATA_DIR = Path(__file__).parent / "license_data"


# ── Enums ─────────────────────────────────────────────────────────────────────

class LicenseType(str, Enum):
    PERPETUAL      = "perpetual"
    SUBSCRIPTION   = "subscription"
    VOLUME         = "volume"
    OEM            = "oem"
    OPEN_SOURCE    = "open_source"
    FREEWARE       = "freeware"
    TRIAL          = "trial"


class SaaSCategory(str, Enum):
    PRODUCTIVITY   = "productivity"    # M365, Google Workspace
    COMMUNICATION  = "communication"   # Slack, Teams, Zoom
    SECURITY       = "security"        # 1Password, Okta
    DEVTOOLS       = "devtools"        # GitHub, Jira, Confluence
    CRM            = "crm"
    FINANCE        = "finance"
    HR             = "hr"
    MONITORING     = "monitoring"
    STORAGE        = "storage"
    OTHER          = "other"


class DiscoverySource(str, Enum):
    MANUAL         = "manual"
    OAUTH_GRANTS   = "oauth_grants"
    DNS            = "dns"
    EMAIL_HEADERS  = "email_headers"
    AGENT_BROWSER  = "agent_browser"   # browser extension / history
    INVOICE        = "invoice"


class AlertSeverity(str, Enum):
    INFO    = "info"
    WARNING = "warning"
    URGENT  = "urgent"


# ── On-premise license model ──────────────────────────────────────────────────

@dataclass
class LicensedProduct:
    """An on-premise / desktop software license record."""
    id:               str
    name:             str
    vendor:           str               = ""
    version:          str               = ""
    license_type:     LicenseType       = LicenseType.PERPETUAL
    seats_licensed:   int               = 1
    seats_active:     int               = 0       # from agent reconciliation
    license_key:      str               = ""       # never logged; stored at rest
    purchase_date:    float             = 0.0
    renewal_date:     float             = 0.0      # 0 = perpetual / no renewal
    annual_cost:      float             = 0.0
    notes:            str               = ""
    # node_id → version string for installed nodes
    installed_nodes:  dict[str, str]    = field(default_factory=dict)

    @property
    def days_to_renewal(self) -> int | None:
        if not self.renewal_date:
            return None
        return max(0, int((self.renewal_date - time.time()) / 86400))

    @property
    def utilisation_pct(self) -> float:
        if not self.seats_licensed:
            return 0.0
        return round(self.seats_active / self.seats_licensed * 100, 1)

    @property
    def wasted_seats(self) -> int:
        return max(0, self.seats_licensed - self.seats_active)

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id, "name": self.name, "vendor": self.vendor,
            "version": self.version, "license_type": self.license_type.value,
            "seats_licensed": self.seats_licensed, "seats_active": self.seats_active,
            "purchase_date": self.purchase_date, "renewal_date": self.renewal_date,
            "annual_cost": self.annual_cost, "notes": self.notes,
            "installed_nodes": self.installed_nodes,
            "days_to_renewal": self.days_to_renewal,
            "utilisation_pct": self.utilisation_pct,
            "wasted_seats": self.wasted_seats,
        }

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> LicensedProduct:
        return cls(
            id=d["id"], name=d["name"], vendor=d.get("vendor", ""),
            version=d.get("version", ""),
            license_type=LicenseType(d.get("license_type", "perpetual")),
            seats_licensed=d.get("seats_licensed", 1),
            seats_active=d.get("seats_active", 0),
            license_key=d.get("license_key", ""),
            purchase_date=d.get("purchase_date", 0.0),
            renewal_date=d.get("renewal_date", 0.0),
            annual_cost=d.get("annual_cost", 0.0),
            notes=d.get("notes", ""),
            installed_nodes=d.get("installed_nodes", {}),
        )


# ── SaaS application model ────────────────────────────────────────────────────

@dataclass
class SaaSApplication:
    """A SaaS tool used in the organisation."""
    id:                 str
    name:               str
    vendor:             str                = ""
    category:           SaaSCategory       = SaaSCategory.OTHER
    url:                str                = ""
    discovery_sources:  list[DiscoverySource] = field(default_factory=list)
    # Users
    users:              list[str]          = field(default_factory=list)   # user IDs/emails
    active_users_30d:   int                = 0
    # Commercial
    seats_licensed:     int                = 0     # 0 = unknown/unlimited
    seats_active:       int                = 0
    monthly_cost:       float              = 0.0
    renewal_date:       float              = 0.0
    annual_contract:    bool               = False
    # Vendor risk
    vendor_soc2:        bool | None        = None   # None = unknown
    vendor_gdpr:        bool | None        = None
    dpa_signed:         bool               = False
    data_categories:    list[str]          = field(default_factory=list)
    breach_history:     bool               = False
    # Governance
    approved:           bool               = False   # False = shadow IT
    sso_integrated:     bool               = False
    mfa_enforced:       bool               = False
    owner_user_id:      str                = ""
    notes:              str                = ""
    added_at:           float              = 0.0
    last_seen:          float              = 0.0

    @property
    def days_to_renewal(self) -> int | None:
        if not self.renewal_date:
            return None
        return max(0, int((self.renewal_date - time.time()) / 86400))

    @property
    def annual_cost(self) -> float:
        return self.monthly_cost * 12

    @property
    def wasted_seats(self) -> int:
        if not self.seats_licensed:
            return 0
        return max(0, self.seats_licensed - self.seats_active)

    @property
    def is_shadow_it(self) -> bool:
        return not self.approved

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id, "name": self.name, "vendor": self.vendor,
            "category": self.category.value, "url": self.url,
            "discovery_sources": [s.value for s in self.discovery_sources],
            "users": self.users, "active_users_30d": self.active_users_30d,
            "seats_licensed": self.seats_licensed, "seats_active": self.seats_active,
            "monthly_cost": self.monthly_cost, "renewal_date": self.renewal_date,
            "annual_contract": self.annual_contract,
            "vendor_soc2": self.vendor_soc2, "vendor_gdpr": self.vendor_gdpr,
            "dpa_signed": self.dpa_signed, "data_categories": self.data_categories,
            "breach_history": self.breach_history,
            "approved": self.approved, "sso_integrated": self.sso_integrated,
            "mfa_enforced": self.mfa_enforced, "owner_user_id": self.owner_user_id,
            "notes": self.notes, "added_at": self.added_at, "last_seen": self.last_seen,
            "days_to_renewal": self.days_to_renewal,
            "annual_cost": self.annual_cost,
            "wasted_seats": self.wasted_seats,
            "is_shadow_it": self.is_shadow_it,
        }

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> SaaSApplication:
        return cls(
            id=d["id"], name=d["name"], vendor=d.get("vendor", ""),
            category=SaaSCategory(d.get("category", "other")),
            url=d.get("url", ""),
            discovery_sources=[DiscoverySource(s) for s in d.get("discovery_sources", [])],
            users=d.get("users", []),
            active_users_30d=d.get("active_users_30d", 0),
            seats_licensed=d.get("seats_licensed", 0),
            seats_active=d.get("seats_active", 0),
            monthly_cost=d.get("monthly_cost", 0.0),
            renewal_date=d.get("renewal_date", 0.0),
            annual_contract=d.get("annual_contract", False),
            vendor_soc2=d.get("vendor_soc2"),
            vendor_gdpr=d.get("vendor_gdpr"),
            dpa_signed=d.get("dpa_signed", False),
            data_categories=d.get("data_categories", []),
            breach_history=d.get("breach_history", False),
            approved=d.get("approved", False),
            sso_integrated=d.get("sso_integrated", False),
            mfa_enforced=d.get("mfa_enforced", False),
            owner_user_id=d.get("owner_user_id", ""),
            notes=d.get("notes", ""),
            added_at=d.get("added_at", 0.0),
            last_seen=d.get("last_seen", 0.0),
        )


# ── Renewal alert ─────────────────────────────────────────────────────────────

@dataclass
class RenewalAlert:
    id:           str
    resource_id:  str        # LicensedProduct.id or SaaSApplication.id
    resource_type: str       # "software" | "saas"
    resource_name: str
    days_remaining: int
    severity:     AlertSeverity
    renewal_date: float
    annual_cost:  float
    fired_at:     float

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id, "resource_id": self.resource_id,
            "resource_type": self.resource_type, "resource_name": self.resource_name,
            "days_remaining": self.days_remaining, "severity": self.severity.value,
            "renewal_date": self.renewal_date, "annual_cost": self.annual_cost,
            "fired_at": self.fired_at,
        }


# ── License manager ───────────────────────────────────────────────────────────

class LicenseManager:
    """
    Central manager for software license and SaaS application inventory.

    Responsibilities:
      - CRUD for LicensedProduct and SaaSApplication records
      - Agent reconciliation: cross-reference installed software vs. licensed
      - Renewal calendar: emit alerts at 90 / 30 / 7 days before expiry
      - Cost reporting: total spend, wasted seats, duplicate tools
      - Shadow IT: unapproved app detection and alerts
      - Offboarding: checklist of apps requiring manual revocation
    """

    RENEWAL_THRESHOLDS = [90, 30, 7]   # days before renewal to alert

    def __init__(self, data_dir: Path = DATA_DIR,
                 on_alert: Callable[[dict], Awaitable[None]] | None = None) -> None:
        self._dir = data_dir
        self._dir.mkdir(parents=True, exist_ok=True)
        self._products: dict[str, LicensedProduct] = {}
        self._saas: dict[str, SaaSApplication] = {}
        self._fired_alerts: set[str] = set()   # alert keys already sent this cycle
        self._on_alert = on_alert
        self._renewal_task: asyncio.Task | None = None
        self._load()

    # ── Persistence ──────────────────────────────────────────────────────────

    def _load(self) -> None:
        products_path = self._dir / "products.json"
        saas_path = self._dir / "saas.json"
        fired_path = self._dir / "fired_alerts.json"
        try:
            if products_path.exists():
                for d in json.loads(products_path.read_text()):
                    p = LicensedProduct.from_dict(d)
                    self._products[p.id] = p
            if saas_path.exists():
                for d in json.loads(saas_path.read_text()):
                    a = SaaSApplication.from_dict(d)
                    self._saas[a.id] = a
            if fired_path.exists():
                self._fired_alerts = set(json.loads(fired_path.read_text()))
        except Exception as e:
            log.warning("Failed to load license data: %s", e)

    def _save(self) -> None:
        try:
            (self._dir / "products.json").write_text(
                json.dumps([p.to_dict() for p in self._products.values()], indent=2)
            )
            (self._dir / "saas.json").write_text(
                json.dumps([a.to_dict() for a in self._saas.values()], indent=2)
            )
            (self._dir / "fired_alerts.json").write_text(
                json.dumps(list(self._fired_alerts))
            )
        except Exception as e:
            log.warning("Failed to save license data: %s", e)

    # ── Lifecycle ─────────────────────────────────────────────────────────────

    async def start(self) -> None:
        self._renewal_task = asyncio.create_task(
            self._renewal_loop(), name="license-renewal-check"
        )
        log.info("License manager started (%d products, %d SaaS apps)",
                 len(self._products), len(self._saas))

    async def stop(self) -> None:
        if self._renewal_task:
            self._renewal_task.cancel()

    # ── Software license CRUD ─────────────────────────────────────────────────

    def add_product(self, name: str, vendor: str = "",
                    license_type: LicenseType = LicenseType.PERPETUAL,
                    seats: int = 1, annual_cost: float = 0.0,
                    renewal_date: float = 0.0,
                    **kwargs: Any) -> LicensedProduct:
        p = LicensedProduct(
            id=str(uuid.uuid4()), name=name, vendor=vendor,
            license_type=license_type, seats_licensed=seats,
            annual_cost=annual_cost, renewal_date=renewal_date,
            purchase_date=time.time(), **kwargs,
        )
        self._products[p.id] = p
        self._save()
        log.info("License added: %s (%s) x%d seats", name, vendor, seats)
        return p

    def update_product(self, product_id: str, **kwargs: Any) -> LicensedProduct | None:
        p = self._products.get(product_id)
        if not p:
            return None
        for k, v in kwargs.items():
            if hasattr(p, k):
                setattr(p, k, v)
        self._save()
        return p

    def remove_product(self, product_id: str) -> bool:
        if product_id in self._products:
            del self._products[product_id]
            self._save()
            return True
        return False

    def get_product(self, product_id: str) -> LicensedProduct | None:
        return self._products.get(product_id)

    def list_products(self) -> list[LicensedProduct]:
        return list(self._products.values())

    # ── SaaS CRUD ─────────────────────────────────────────────────────────────

    def add_saas(self, name: str, vendor: str = "",
                 category: SaaSCategory = SaaSCategory.OTHER,
                 url: str = "",
                 discovery_sources: list[DiscoverySource] | None = None,
                 **kwargs: Any) -> SaaSApplication:
        app = SaaSApplication(
            id=str(uuid.uuid4()), name=name, vendor=vendor,
            category=category, url=url,
            discovery_sources=discovery_sources or [],
            added_at=time.time(), last_seen=time.time(),
            **kwargs,
        )
        self._saas[app.id] = app
        self._save()
        log.info("SaaS app added: %s (%s) approved=%s", name, vendor, app.approved)
        return app

    def update_saas(self, app_id: str, **kwargs: Any) -> SaaSApplication | None:
        app = self._saas.get(app_id)
        if not app:
            return None
        for k, v in kwargs.items():
            if hasattr(app, k):
                setattr(app, k, v)
        self._save()
        return app

    def remove_saas(self, app_id: str) -> bool:
        if app_id in self._saas:
            del self._saas[app_id]
            self._save()
            return True
        return False

    def get_saas(self, app_id: str) -> SaaSApplication | None:
        return self._saas.get(app_id)

    def list_saas(self) -> list[SaaSApplication]:
        return list(self._saas.values())

    def approve_saas(self, app_id: str, owner_user_id: str = "") -> SaaSApplication | None:
        return self.update_saas(app_id, approved=True, owner_user_id=owner_user_id)

    # ── Agent reconciliation ──────────────────────────────────────────────────

    def reconcile_node(self, node_id: str,
                       installed: list[dict[str, str]]) -> dict[str, Any]:
        """
        Cross-reference installed software list from an agent against licensed products.

        ``installed`` is a list of dicts: [{name, version, vendor, ...}]

        Returns a reconciliation report with:
          - matched: products found installed on this node
          - unlicensed: installed software with no matching license record
          - absent: licensed products not found on this node (may be intentional)
        """
        matched: list[str] = []
        unlicensed: list[dict] = []

        installed_names = {i["name"].lower(): i for i in installed}

        for prod in self._products.values():
            pname = prod.name.lower()
            if pname in installed_names:
                matched.append(prod.id)
                ver = installed_names[pname].get("version", "")
                prod.installed_nodes[node_id] = ver
                prod.seats_active = len(prod.installed_nodes)
            else:
                # Remove node from installed if it was previously recorded
                if node_id in prod.installed_nodes:
                    del prod.installed_nodes[node_id]
                    prod.seats_active = len(prod.installed_nodes)

        licensed_names = {p.name.lower() for p in self._products.values()}
        for item in installed:
            if item["name"].lower() not in licensed_names:
                unlicensed.append(item)

        absent = [
            p.id for p in self._products.values()
            if node_id not in p.installed_nodes
            and p.license_type not in (LicenseType.OPEN_SOURCE, LicenseType.FREEWARE)
        ]

        self._save()
        return {
            "node_id": node_id,
            "matched_count": len(matched),
            "unlicensed_count": len(unlicensed),
            "absent_count": len(absent),
            "unlicensed": unlicensed,
            "absent_product_ids": absent,
        }

    # ── Analytics ─────────────────────────────────────────────────────────────

    def find_upcoming_renewals(self, days: int = 90) -> list[dict[str, Any]]:
        """Return products/apps renewing within `days` days, sorted by urgency."""
        cutoff = time.time() + days * 86400
        results = []
        for p in self._products.values():
            if p.renewal_date and 0 < p.renewal_date <= cutoff:
                results.append({**p.to_dict(), "_type": "software"})
        for a in self._saas.values():
            if a.renewal_date and 0 < a.renewal_date <= cutoff:
                results.append({**a.to_dict(), "_type": "saas"})
        results.sort(key=lambda x: x["renewal_date"])
        return results

    def find_wasted_seats(self, threshold_pct: float = 50.0) -> list[dict[str, Any]]:
        """Return licenses/apps where utilisation < threshold_pct%."""
        results = []
        for p in self._products.values():
            if p.seats_licensed > 1 and p.utilisation_pct < threshold_pct:
                results.append({**p.to_dict(), "_type": "software"})
        for a in self._saas.values():
            if a.seats_licensed > 0:
                util = a.seats_active / a.seats_licensed * 100 if a.seats_licensed else 0
                if util < threshold_pct:
                    results.append({**a.to_dict(), "_type": "saas", "utilisation_pct": round(util, 1)})
        return results

    def find_shadow_it(self) -> list[SaaSApplication]:
        """Return unapproved SaaS applications."""
        return [a for a in self._saas.values() if not a.approved]

    def find_no_dpa(self) -> list[SaaSApplication]:
        """Return apps handling personal data but no DPA signed."""
        sensitive = {"personal_data", "health", "financial", "hr", "crm"}
        return [
            a for a in self._saas.values()
            if not a.dpa_signed and any(c in sensitive for c in a.data_categories)
        ]

    def find_duplicate_categories(self) -> dict[str, list[SaaSApplication]]:
        """Return SaaS categories with more than one approved app."""
        by_cat: dict[str, list[SaaSApplication]] = {}
        for a in self._saas.values():
            if a.approved:
                by_cat.setdefault(a.category.value, []).append(a)
        return {cat: apps for cat, apps in by_cat.items() if len(apps) > 1}

    def offboarding_checklist(self, user_id: str) -> list[dict[str, Any]]:
        """
        Return apps requiring manual revocation for the given user.
        SSO-integrated apps are auto-revoked on IdP offboarding.
        Non-SSO apps need a manual step.
        """
        checklist = []
        for a in self._saas.values():
            if user_id in a.users:
                checklist.append({
                    "app_id": a.id, "app_name": a.name,
                    "vendor": a.vendor, "url": a.url,
                    "sso_integrated": a.sso_integrated,
                    "action": "auto" if a.sso_integrated else "manual",
                    "note": (
                        "Revoked automatically via IdP" if a.sso_integrated
                        else f"Manually revoke at {a.url or a.vendor}"
                    ),
                })
        checklist.sort(key=lambda x: (x["action"] == "auto", x["app_name"]))
        return checklist

    def cost_summary(self) -> dict[str, Any]:
        """Total annual spend across all tracked software and SaaS."""
        sw_cost = sum(p.annual_cost for p in self._products.values())
        saas_cost = sum(a.annual_cost for a in self._saas.values())
        sw_wasted = sum(
            p.wasted_seats * (p.annual_cost / p.seats_licensed)
            for p in self._products.values()
            if p.seats_licensed > 0 and p.annual_cost > 0
        )
        saas_wasted = sum(
            a.wasted_seats * (a.monthly_cost * 12 / a.seats_licensed)
            for a in self._saas.values()
            if a.seats_licensed > 0 and a.monthly_cost > 0
        )
        return {
            "total_annual": round(sw_cost + saas_cost, 2),
            "software_annual": round(sw_cost, 2),
            "saas_annual": round(saas_cost, 2),
            "saas_monthly": round(saas_cost / 12, 2),
            "wasted_annual": round(sw_wasted + saas_wasted, 2),
            "software_products": len(self._products),
            "saas_apps": len(self._saas),
            "shadow_it_count": len(self.find_shadow_it()),
            "no_dpa_count": len(self.find_no_dpa()),
        }

    # ── Renewal alert loop ────────────────────────────────────────────────────

    async def _renewal_loop(self) -> None:
        """Check for upcoming renewals once per day."""
        while True:
            await asyncio.sleep(86400)
            await self._check_renewals()

    async def _check_renewals(self) -> None:
        now = time.time()
        alerts: list[RenewalAlert] = []

        for p in self._products.values():
            alert = self._maybe_alert(p.id, "software", p.name,
                                       p.renewal_date, p.annual_cost, now)
            if alert:
                alerts.append(alert)

        for a in self._saas.values():
            alert = self._maybe_alert(a.id, "saas", a.name,
                                       a.renewal_date, a.annual_cost, now)
            if alert:
                alerts.append(alert)

        for alert in alerts:
            key = f"{alert.resource_id}:{alert.days_remaining}"
            if key not in self._fired_alerts:
                self._fired_alerts.add(key)
                log.info("Renewal alert: %s in %d days ($%.0f/yr)",
                         alert.resource_name, alert.days_remaining, alert.annual_cost)
                if self._on_alert:
                    await self._on_alert(alert.to_dict())

        if alerts:
            self._save()

    def _maybe_alert(self, resource_id: str, resource_type: str,
                     name: str, renewal_date: float, annual_cost: float,
                     now: float) -> RenewalAlert | None:
        if not renewal_date:
            return None
        days = int((renewal_date - now) / 86400)
        if days < 0:
            return None
        for threshold in self.RENEWAL_THRESHOLDS:
            if days <= threshold:
                severity = (AlertSeverity.URGENT if days <= 7
                            else AlertSeverity.WARNING if days <= 30
                            else AlertSeverity.INFO)
                return RenewalAlert(
                    id=str(uuid.uuid4()),
                    resource_id=resource_id, resource_type=resource_type,
                    resource_name=name, days_remaining=days,
                    severity=severity, renewal_date=renewal_date,
                    annual_cost=annual_cost, fired_at=now,
                )
        return None

    async def run_renewal_check_now(self) -> list[RenewalAlert]:
        """Trigger an immediate renewal check and return fired alerts."""
        fired: list[RenewalAlert] = []
        now = time.time()
        for p in self._products.values():
            a = self._maybe_alert(p.id, "software", p.name,
                                   p.renewal_date, p.annual_cost, now)
            if a:
                fired.append(a)
        for app in self._saas.values():
            a = self._maybe_alert(app.id, "saas", app.name,
                                   app.renewal_date, app.annual_cost, now)
            if a:
                fired.append(a)
        for alert in fired:
            key = f"{alert.resource_id}:{alert.days_remaining}"
            if key not in self._fired_alerts and self._on_alert:
                self._fired_alerts.add(key)
                await self._on_alert(alert.to_dict())
        self._save()
        return fired
