#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Unit tests for SaaSManager — SaaS application discovery and governance.
"""

import asyncio
import json
import sys
import tempfile
import time
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent.parent.parent.parent / "controller"))

from saas_management import (
    SaaSManager, ManagedSaaSApp, SaaSConfig,
    _infer_category, _normalise_name, _scopes_to_data_categories,
    SOURCE_MANUAL, SOURCE_M365, SOURCE_GOOGLE, SOURCE_OSQUERY,
    SOURCE_DNS, SOURCE_EMAIL,
)


def _mgr(tmp: Path) -> SaaSManager:
    return SaaSManager(tmp)


def _run(coro):
    return asyncio.run(coro)


# ── Helper utilities ──────────────────────────────────────────────────────────

class TestHelpers(unittest.TestCase):
    def test_infer_category_slack(self):
        self.assertEqual(_infer_category("Slack", "slack.com"), "Communication")

    def test_infer_category_salesforce(self):
        self.assertEqual(_infer_category("Salesforce", "salesforce.com"), "CRM")

    def test_infer_category_github(self):
        self.assertEqual(_infer_category("GitHub", "github.com"), "DevOps")

    def test_infer_category_unknown(self):
        self.assertEqual(_infer_category("RandomApp", "randomapp.io"), "Other")

    def test_normalise_name(self):
        self.assertEqual(_normalise_name("  Slack  "), "slack")
        self.assertEqual(_normalise_name("SALESFORCE"), "salesforce")

    def test_scopes_email(self):
        cats = _scopes_to_data_categories("mail.read calendar.readwrite")
        self.assertIn("email", cats)
        self.assertIn("calendar", cats)

    def test_scopes_files(self):
        cats = _scopes_to_data_categories("files.readwrite.all")
        self.assertIn("files", cats)

    def test_scopes_pii(self):
        cats = _scopes_to_data_categories("user.read profile openid")
        self.assertIn("PII", cats)

    def test_scopes_empty(self):
        self.assertEqual(_scopes_to_data_categories(""), [])


# ── ManagedSaaSApp model ──────────────────────────────────────────────────────

class TestManagedSaaSApp(unittest.TestCase):
    def _make(self, **kwargs) -> ManagedSaaSApp:
        import uuid
        defaults = dict(
            id=str(uuid.uuid4()), name="TestApp", vendor="TestCo",
            first_seen=1000.0, last_seen=2000.0,
        )
        defaults.update(kwargs)
        return ManagedSaaSApp(**defaults)

    def test_shadow_it_flag(self):
        app = self._make(approved=False)
        self.assertTrue(app.shadow_it)
        app2 = self._make(approved=True)
        self.assertFalse(app2.shadow_it)

    def test_unused_seats(self):
        app = self._make(seats_licensed=10, seats_active=6)
        self.assertEqual(app.unused_seats, 4)

    def test_unused_seats_none_when_unlicensed(self):
        app = self._make(seats_licensed=None, seats_active=3)
        self.assertIsNone(app.unused_seats)

    def test_annual_cost(self):
        app = self._make(monthly_cost=100.0)
        self.assertEqual(app.annual_cost, 1200.0)

    def test_annual_cost_none(self):
        app = self._make(monthly_cost=None)
        self.assertIsNone(app.annual_cost)

    def test_days_until_renewal(self):
        from datetime import date, timedelta
        future = (date.today() + timedelta(days=45)).isoformat()
        app = self._make(renewal_date=future)
        d = app.days_until_renewal
        self.assertIsNotNone(d)
        self.assertGreater(d, 40)
        self.assertLess(d, 50)

    def test_renewal_risk_upcoming(self):
        from datetime import date, timedelta
        soon = (date.today() + timedelta(days=20)).isoformat()
        app = self._make(renewal_date=soon)
        self.assertEqual(app.renewal_risk, "upcoming")

    def test_renewal_risk_ok(self):
        from datetime import date, timedelta
        later = (date.today() + timedelta(days=120)).isoformat()
        app = self._make(renewal_date=later)
        self.assertEqual(app.renewal_risk, "ok")

    def test_renewal_risk_unknown(self):
        app = self._make(renewal_date=None)
        self.assertEqual(app.renewal_risk, "unknown")

    def test_to_dict_roundtrip(self):
        app = self._make(
            name="Slack", vendor="Slack Technologies",
            category="Communication", domain="slack.com",
            users=["alice@co.com"], seats_licensed=50, seats_active=40,
            monthly_cost=500.0, approved=True, sso_integrated=True,
        )
        d = app.to_dict()
        restored = ManagedSaaSApp.from_dict(d)
        self.assertEqual(restored.name, "Slack")
        self.assertEqual(restored.users, ["alice@co.com"])
        self.assertEqual(restored.monthly_cost, 500.0)
        self.assertTrue(restored.sso_integrated)


# ── SaaSConfig ────────────────────────────────────────────────────────────────

class TestSaaSConfig(unittest.TestCase):
    def test_defaults(self):
        c = SaaSConfig()
        self.assertTrue(c.shadow_it_alerts)
        self.assertEqual(c.discovery_interval, 86400)
        self.assertEqual(c.inactive_seat_days, 30)

    def test_roundtrip(self):
        c = SaaSConfig(
            trusted_domains=["internal.co"],
            shadow_it_alerts=False,
            discovery_interval=3600,
        )
        c2 = SaaSConfig.from_dict(c.to_dict())
        self.assertEqual(c2.trusted_domains, ["internal.co"])
        self.assertFalse(c2.shadow_it_alerts)
        self.assertEqual(c2.discovery_interval, 3600)


# ── App registration and deduplication ───────────────────────────────────────

class TestAppRegistration(unittest.TestCase):
    def setUp(self):
        self._tmp = Path(tempfile.mkdtemp())
        self._mgr = _mgr(self._tmp)

    def test_register_new_app(self):
        app = self._mgr.register_app("Slack", vendor="Slack Technologies",
                                      domain="slack.com")
        self.assertEqual(app.name, "Slack")
        self.assertEqual(app.domain, "slack.com")
        self.assertFalse(app.approved)  # shadow IT by default

    def test_register_deduplicates_by_name(self):
        app1 = self._mgr.register_app("Slack")
        app2 = self._mgr.register_app("Slack", domain="slack.com")
        self.assertEqual(app1.id, app2.id)
        # Domain should be merged in
        self.assertEqual(app2.domain, "slack.com")

    def test_case_insensitive_dedup(self):
        app1 = self._mgr.register_app("slack")
        app2 = self._mgr.register_app("SLACK")
        self.assertEqual(app1.id, app2.id)

    def test_multiple_sources_merged(self):
        self._mgr.register_app("Notion", source=SOURCE_DNS)
        app = self._mgr.register_app("Notion", source=SOURCE_M365)
        self.assertIn(SOURCE_DNS, app.discovery_sources)
        self.assertIn(SOURCE_M365, app.discovery_sources)

    def test_category_inferred(self):
        app = self._mgr.register_app("GitHub", domain="github.com")
        self.assertEqual(app.category, "DevOps")

    def test_register_fires_discovered_event(self):
        q: asyncio.Queue = asyncio.Queue()
        mgr = SaaSManager(self._tmp / "ev", event_queue=q)
        _run(mgr.start())
        mgr.register_app("NewApp")
        # Events are fired via create_task; run event loop briefly
        _run(asyncio.sleep(0.01))
        events = []
        while not q.empty():
            events.append(q.get_nowait())
        _run(mgr.stop())
        types = [e["type"] for e in events]
        self.assertIn("saas.app.discovered", types)


# ── CRUD operations ───────────────────────────────────────────────────────────

class TestCRUD(unittest.TestCase):
    def setUp(self):
        self._tmp = Path(tempfile.mkdtemp())
        self._mgr = _mgr(self._tmp)

    def test_get_app(self):
        app = self._mgr.register_app("Jira")
        fetched = self._mgr.get_app(app.id)
        self.assertIsNotNone(fetched)
        self.assertEqual(fetched.name, "Jira")

    def test_get_nonexistent(self):
        self.assertIsNone(self._mgr.get_app("nonexistent-id"))

    def test_update_app(self):
        app = self._mgr.register_app("Asana")
        self._mgr.update_app(app.id, {"approved": True, "monthly_cost": 99.0})
        updated = self._mgr.get_app(app.id)
        self.assertTrue(updated.approved)
        self.assertEqual(updated.monthly_cost, 99.0)

    def test_update_nonexistent(self):
        result = self._mgr.update_app("nonexistent", {"approved": True})
        self.assertIsNone(result)

    def test_delete_app(self):
        app = self._mgr.register_app("OldApp")
        self.assertTrue(self._mgr.delete_app(app.id))
        self.assertIsNone(self._mgr.get_app(app.id))

    def test_delete_nonexistent(self):
        self.assertFalse(self._mgr.delete_app("nonexistent"))

    def test_list_apps_shadow_it_filter(self):
        self._mgr.register_app("ShadowApp")
        app2 = self._mgr.register_app("ApprovedApp")
        self._mgr.update_app(app2.id, {"approved": True})
        shadow = self._mgr.list_apps(shadow_it=True)
        approved = self._mgr.list_apps(shadow_it=False)
        names_shadow = [a.name for a in shadow]
        names_approved = [a.name for a in approved]
        self.assertIn("ShadowApp", names_shadow)
        self.assertIn("ApprovedApp", names_approved)
        self.assertNotIn("ApprovedApp", names_shadow)

    def test_list_apps_category_filter(self):
        self._mgr.register_app("Slack", domain="slack.com")
        self._mgr.register_app("GitHub", domain="github.com")
        comms = self._mgr.list_apps(category="Communication")
        names = [a.name for a in comms]
        self.assertIn("Slack", names)
        self.assertNotIn("GitHub", names)

    def test_config_update(self):
        self._mgr.set_config({"shadow_it_alerts": False, "discovery_interval": 7200})
        cfg = self._mgr.get_config()
        self.assertFalse(cfg.shadow_it_alerts)
        self.assertEqual(cfg.discovery_interval, 7200)


# ── User access tracking ──────────────────────────────────────────────────────

class TestUserAccess(unittest.TestCase):
    def setUp(self):
        self._tmp = Path(tempfile.mkdtemp())
        self._mgr = _mgr(self._tmp)

    def test_add_user_access(self):
        app = self._mgr.register_app("Notion")
        self._mgr.add_user_access(app.id, "alice@co.com")
        updated = self._mgr.get_app(app.id)
        self.assertIn("alice@co.com", updated.users)
        self.assertEqual(updated.seats_active, 1)

    def test_remove_user_access(self):
        app = self._mgr.register_app("Notion")
        self._mgr.add_user_access(app.id, "alice@co.com")
        self._mgr.add_user_access(app.id, "bob@co.com")
        self._mgr.remove_user_access(app.id, "alice@co.com")
        updated = self._mgr.get_app(app.id)
        self.assertNotIn("alice@co.com", updated.users)
        self.assertEqual(updated.seats_active, 1)

    def test_user_apps(self):
        app1 = self._mgr.register_app("Slack")
        app2 = self._mgr.register_app("Notion")
        self._mgr.register_app("GitHub")
        self._mgr.add_user_access(app1.id, "alice@co.com")
        self._mgr.add_user_access(app2.id, "alice@co.com")
        alice_apps = self._mgr.user_apps("alice@co.com")
        names = [a.name for a in alice_apps]
        self.assertIn("Slack", names)
        self.assertIn("Notion", names)
        self.assertNotIn("GitHub", names)

    def test_add_user_nonexistent_app(self):
        self.assertFalse(self._mgr.add_user_access("nonexistent", "alice@co.com"))


# ── Offboarding ───────────────────────────────────────────────────────────────

class TestOffboarding(unittest.TestCase):
    def setUp(self):
        self._tmp = Path(tempfile.mkdtemp())
        self._mgr = _mgr(self._tmp)

    def test_offboarding_checklist_structure(self):
        app1 = self._mgr.register_app("Slack", sso_integrated=True)
        app2 = self._mgr.register_app("WeirdTool", sso_integrated=False)
        self._mgr.update_app(app1.id, {"sso_integrated": True})
        self._mgr.add_user_access(app1.id, "bob@co.com")
        self._mgr.add_user_access(app2.id, "bob@co.com")
        checklist = self._mgr.create_offboarding_checklist("bob@co.com")
        self.assertEqual(checklist["user"], "bob@co.com")
        self.assertEqual(checklist["total_apps"], 2)
        self.assertIn("api_revocable", checklist)
        self.assertIn("manual_required", checklist)

    def test_offboarding_sso_vs_manual(self):
        sso_app = self._mgr.register_app("SlackSSO")
        manual_app = self._mgr.register_app("ManualTool")
        self._mgr.update_app(sso_app.id, {"sso_integrated": True})
        self._mgr.update_app(manual_app.id, {"sso_integrated": False})
        self._mgr.add_user_access(sso_app.id, "carol@co.com")
        self._mgr.add_user_access(manual_app.id, "carol@co.com")
        checklist = self._mgr.create_offboarding_checklist("carol@co.com")
        sso_names = [t["app_name"] for t in checklist["api_revocable"]]
        manual_names = [t["app_name"] for t in checklist["manual_required"]]
        self.assertIn("SlackSSO", sso_names)
        self.assertIn("ManualTool", manual_names)

    def test_offboarding_no_apps(self):
        checklist = self._mgr.create_offboarding_checklist("nobody@co.com")
        self.assertEqual(checklist["total_apps"], 0)


# ── Analytics ─────────────────────────────────────────────────────────────────

class TestAnalytics(unittest.TestCase):
    def setUp(self):
        self._tmp = Path(tempfile.mkdtemp())
        self._mgr = _mgr(self._tmp)

    def test_shadow_it_summary(self):
        self._mgr.register_app("ShadowApp1")
        self._mgr.register_app("ShadowApp2")
        app3 = self._mgr.register_app("ApprovedApp")
        self._mgr.update_app(app3.id, {"approved": True})
        summary = self._mgr.shadow_it_summary()
        self.assertEqual(summary["count"], 2)
        names = [a["name"] for a in summary["apps"]]
        self.assertIn("ShadowApp1", names)
        self.assertNotIn("ApprovedApp", names)

    def test_cost_summary(self):
        app1 = self._mgr.register_app("App1")
        app2 = self._mgr.register_app("App2")
        self._mgr.update_app(app1.id, {"monthly_cost": 100.0,
                                        "seats_licensed": 10, "seats_active": 6})
        self._mgr.update_app(app2.id, {"monthly_cost": 50.0,
                                        "seats_licensed": 5, "seats_active": 5})
        cs = self._mgr.cost_summary()
        self.assertEqual(cs["total_monthly_cost"], 150.0)
        self.assertEqual(cs["total_annual_cost"], 1800.0)
        # App1 has 4 wasted seats at $10/seat
        self.assertGreater(cs["estimated_wasted_monthly"], 0)

    def test_upcoming_renewals(self):
        from datetime import date, timedelta
        app = self._mgr.register_app("RenewalApp")
        soon = (date.today() + timedelta(days=25)).isoformat()
        self._mgr.update_app(app.id, {"renewal_date": soon, "monthly_cost": 200.0})
        renewals = self._mgr.upcoming_renewals(days=30)
        self.assertEqual(len(renewals), 1)
        self.assertEqual(renewals[0]["name"], "RenewalApp")

    def test_upcoming_renewals_excludes_future(self):
        from datetime import date, timedelta
        app = self._mgr.register_app("FarFutureApp")
        later = (date.today() + timedelta(days=200)).isoformat()
        self._mgr.update_app(app.id, {"renewal_date": later})
        renewals = self._mgr.upcoming_renewals(days=30)
        names = [r["name"] for r in renewals]
        self.assertNotIn("FarFutureApp", names)

    def test_duplicate_categories(self):
        app1 = self._mgr.register_app("Asana", domain="asana.com")
        app2 = self._mgr.register_app("Monday", domain="monday.com")
        # Both are Productivity; force category + approval
        self._mgr.update_app(app1.id, {"category": "Productivity", "approved": True})
        self._mgr.update_app(app2.id, {"category": "Productivity", "approved": True})
        dups = self._mgr.duplicate_categories()
        cats = [d["category"] for d in dups]
        self.assertIn("Productivity", cats)

    def test_vendor_risk_summary_pii_without_dpa(self):
        app = self._mgr.register_app("PiiApp")
        self._mgr.update_app(app.id, {
            "data_categories": ["PII", "email"],
            "dpa_signed": False,
        })
        risks = self._mgr.vendor_risk_summary()
        app_ids = [r["app_id"] for r in risks]
        self.assertIn(app.id, app_ids)
        risk_entry = next(r for r in risks if r["app_id"] == app.id)
        self.assertIn("pii_without_dpa", risk_entry["gaps"])

    def test_vendor_risk_no_soc2(self):
        app = self._mgr.register_app("NoSoc2App")
        self._mgr.update_app(app.id, {"vendor_soc2": False})
        risks = self._mgr.vendor_risk_summary()
        app_ids = [r["app_id"] for r in risks]
        self.assertIn(app.id, app_ids)


# ── Discovery ingestion ───────────────────────────────────────────────────────

class TestDiscoveryIngestion(unittest.TestCase):
    def setUp(self):
        self._tmp = Path(tempfile.mkdtemp())
        self._mgr = _mgr(self._tmp)

    def test_ingest_chrome_extensions(self):
        extensions = [
            {"name": "Notion Web Clipper", "identifier": "ext-abc"},
            {"name": "Grammarly"},
            # Skip non-SaaS patterns
            {"name": "uBlock Origin"},
            {"name": "Dark Mode"},
        ]
        new_count = self._mgr.ingest_chrome_extensions("node-1", extensions)
        # Only "Notion Web Clipper" and "Grammarly" should pass filter
        self.assertGreaterEqual(new_count, 1)
        apps = self._mgr.list_apps(source=SOURCE_OSQUERY)
        names = [a.name for a in apps]
        self.assertTrue(any("Notion" in n or "Grammarly" in n for n in names))

    def test_ingest_dns_domains(self):
        domains = ["app.slack.com", "notion.so", "www.github.com"]
        count = self._mgr.ingest_dns_domains(domains)
        self.assertGreater(count, 0)
        apps = self._mgr.list_apps(source=SOURCE_DNS)
        self.assertGreater(len(apps), 0)

    def test_ingest_dns_strips_prefix(self):
        self._mgr.ingest_dns_domains(["app.notion.so"])
        apps = self._mgr.list_apps(source=SOURCE_DNS)
        # domain should be stripped to "notion.so"
        domains = [a.domain for a in apps]
        self.assertIn("notion.so", domains)

    def test_ingest_dns_excluded_domain(self):
        self._mgr.set_config({"excluded_domains": ["excluded.com"]})
        before = len(self._mgr.list_apps())
        self._mgr.ingest_dns_domains(["excluded.com"])
        after = len(self._mgr.list_apps())
        self.assertEqual(before, after)

    def test_ingest_dns_deduplicates(self):
        self._mgr.ingest_dns_domains(["slack.com"])
        count1 = len(self._mgr.list_apps())
        self._mgr.ingest_dns_domains(["slack.com"])
        count2 = len(self._mgr.list_apps())
        self.assertEqual(count1, count2)

    def test_ingest_invoices_creates_app(self):
        invoices = [{"vendor": "Notion", "amount_usd": 96.0,
                     "renewal_date": "2026-12-01"}]
        count = self._mgr.ingest_invoice_data(invoices)
        self.assertEqual(count, 1)
        apps = self._mgr.list_apps(source=SOURCE_EMAIL)
        names = [a.name for a in apps]
        self.assertIn("Notion", names)

    def test_ingest_invoices_updates_existing(self):
        app = self._mgr.register_app("Jira")
        self._mgr.ingest_invoice_data([{"vendor": "Jira", "amount_usd": 150.0}])
        updated = self._mgr.get_app(app.id)
        self.assertEqual(updated.monthly_cost, 150.0)

    def test_ingest_chrome_extensions_deduplicates(self):
        self._mgr.ingest_chrome_extensions("n1", [{"name": "Grammarly"}])
        count1 = len(self._mgr.list_apps())
        new = self._mgr.ingest_chrome_extensions("n2", [{"name": "Grammarly"}])
        self.assertEqual(new, 0)  # Not new, already registered
        count2 = len(self._mgr.list_apps())
        self.assertEqual(count1, count2)


# ── Persistence ───────────────────────────────────────────────────────────────

class TestPersistence(unittest.TestCase):
    def test_apps_persist_across_reload(self):
        tmp = Path(tempfile.mkdtemp())
        mgr1 = SaaSManager(tmp)
        _run(mgr1.start())
        mgr1.register_app("Slack", domain="slack.com")
        mgr1.register_app("GitHub", domain="github.com")
        _run(mgr1.stop())

        mgr2 = SaaSManager(tmp)
        _run(mgr2.start())
        apps = mgr2.list_apps()
        names = [a.name for a in apps]
        _run(mgr2.stop())
        self.assertIn("Slack", names)
        self.assertIn("GitHub", names)

    def test_config_persists(self):
        tmp = Path(tempfile.mkdtemp())
        mgr1 = SaaSManager(tmp)
        _run(mgr1.start())
        mgr1.set_config({"discovery_interval": 7200, "shadow_it_alerts": False})
        _run(mgr1.stop())

        mgr2 = SaaSManager(tmp)
        _run(mgr2.start())
        cfg = mgr2.get_config()
        _run(mgr2.stop())
        self.assertEqual(cfg.discovery_interval, 7200)
        self.assertFalse(cfg.shadow_it_alerts)

    def test_user_access_persists(self):
        tmp = Path(tempfile.mkdtemp())
        mgr1 = SaaSManager(tmp)
        _run(mgr1.start())
        app = mgr1.register_app("Notion")
        mgr1.add_user_access(app.id, "alice@co.com")
        _run(mgr1.stop())

        mgr2 = SaaSManager(tmp)
        _run(mgr2.start())
        loaded_app = mgr2.get_app(app.id)
        _run(mgr2.stop())
        self.assertIn("alice@co.com", loaded_app.users)


# ── Status ────────────────────────────────────────────────────────────────────

class TestStatus(unittest.TestCase):
    def setUp(self):
        self._tmp = Path(tempfile.mkdtemp())
        self._mgr = _mgr(self._tmp)

    def test_status_structure(self):
        s = self._mgr.status()
        self.assertIn("total_apps", s)
        self.assertIn("approved", s)
        self.assertIn("shadow_it", s)
        self.assertIn("total_monthly_cost", s)
        self.assertIn("renewals_next_30d", s)
        self.assertIn("vendor_risk_count", s)

    def test_status_counts(self):
        app1 = self._mgr.register_app("ApprovedApp")
        self._mgr.update_app(app1.id, {"approved": True})
        self._mgr.register_app("ShadowApp")
        s = self._mgr.status()
        self.assertEqual(s["total_apps"], 2)
        self.assertEqual(s["approved"], 1)
        self.assertEqual(s["shadow_it"], 1)


# ── Lifecycle ─────────────────────────────────────────────────────────────────

class TestLifecycle(unittest.TestCase):
    def test_start_stop(self):
        tmp = Path(tempfile.mkdtemp())
        mgr = SaaSManager(tmp)
        _run(mgr.start())
        self.assertIsNotNone(mgr._task)
        _run(mgr.stop())
        self.assertTrue(mgr._task.done())

    def test_stop_without_start(self):
        tmp = Path(tempfile.mkdtemp())
        mgr = SaaSManager(tmp)
        # Should not raise
        _run(mgr.stop())


if __name__ == "__main__":
    unittest.main()
