#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Unit tests for LicenseManager — software license and SaaS application tracking.
"""

import asyncio
import json
import sys
import time
import tempfile
import unittest
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock

sys.path.insert(0, str(Path(__file__).parent.parent.parent.parent / "controller"))

from license_manager import (
    LicenseManager, LicensedProduct, SaaSApplication,
    LicenseType, SaaSCategory, DiscoverySource, AlertSeverity, RenewalAlert,
)


def _mgr(tmp: Path, on_alert=None) -> LicenseManager:
    return LicenseManager(data_dir=tmp, on_alert=on_alert)


# ── LicensedProduct model ─────────────────────────────────────────────────────

class TestLicensedProductModel(unittest.TestCase):

    def _make(self, **kw) -> LicensedProduct:
        defaults = dict(id="p1", name="Acme Suite", vendor="Acme",
                        seats_licensed=10, seats_active=6)
        return LicensedProduct(**{**defaults, **kw})

    def test_utilisation_pct(self):
        p = self._make(seats_licensed=10, seats_active=6)
        self.assertAlmostEqual(p.utilisation_pct, 60.0)

    def test_wasted_seats(self):
        p = self._make(seats_licensed=10, seats_active=4)
        self.assertEqual(p.wasted_seats, 6)

    def test_wasted_seats_zero_when_over_deployed(self):
        p = self._make(seats_licensed=2, seats_active=5)
        self.assertEqual(p.wasted_seats, 0)

    def test_days_to_renewal_none_when_zero(self):
        p = self._make(renewal_date=0.0)
        self.assertIsNone(p.days_to_renewal)

    def test_days_to_renewal_future(self):
        future = time.time() + 30 * 86400
        p = self._make(renewal_date=future)
        self.assertAlmostEqual(p.days_to_renewal, 30, delta=1)

    def test_days_to_renewal_clamps_to_zero(self):
        past = time.time() - 86400
        p = self._make(renewal_date=past)
        self.assertEqual(p.days_to_renewal, 0)

    def test_to_dict_round_trip(self):
        p = self._make(license_type=LicenseType.SUBSCRIPTION,
                       annual_cost=999.0, renewal_date=time.time() + 86400)
        d = p.to_dict()
        p2 = LicensedProduct.from_dict(d)
        self.assertEqual(p2.name, p.name)
        self.assertEqual(p2.license_type, LicenseType.SUBSCRIPTION)
        self.assertAlmostEqual(p2.annual_cost, 999.0)

    def test_to_dict_includes_computed_fields(self):
        p = self._make()
        d = p.to_dict()
        self.assertIn("utilisation_pct", d)
        self.assertIn("wasted_seats", d)
        self.assertIn("days_to_renewal", d)

    def test_license_key_not_in_to_dict(self):
        """license_key must not appear in the serialised dict (stay at rest only)."""
        p = self._make(license_key="SUPER-SECRET-KEY")
        d = p.to_dict()
        self.assertNotIn("license_key", d)


# ── SaaSApplication model ─────────────────────────────────────────────────────

class TestSaaSApplicationModel(unittest.TestCase):

    def _make(self, **kw) -> SaaSApplication:
        defaults = dict(id="a1", name="Slack", vendor="Slack Technologies",
                        category=SaaSCategory.COMMUNICATION,
                        monthly_cost=12.0, seats_licensed=20, seats_active=15,
                        added_at=time.time(), last_seen=time.time())
        return SaaSApplication(**{**defaults, **kw})

    def test_annual_cost(self):
        a = self._make(monthly_cost=10.0)
        self.assertAlmostEqual(a.annual_cost, 120.0)

    def test_is_shadow_it_unapproved(self):
        a = self._make(approved=False)
        self.assertTrue(a.is_shadow_it)

    def test_is_shadow_it_approved(self):
        a = self._make(approved=True)
        self.assertFalse(a.is_shadow_it)

    def test_wasted_seats(self):
        a = self._make(seats_licensed=20, seats_active=12)
        self.assertEqual(a.wasted_seats, 8)

    def test_wasted_seats_zero_when_unlimited(self):
        a = self._make(seats_licensed=0)
        self.assertEqual(a.wasted_seats, 0)

    def test_to_dict_round_trip(self):
        a = self._make(dpa_signed=True, vendor_soc2=True,
                       discovery_sources=[DiscoverySource.OAUTH_GRANTS])
        d = a.to_dict()
        a2 = SaaSApplication.from_dict(d)
        self.assertEqual(a2.name, a.name)
        self.assertTrue(a2.dpa_signed)
        self.assertEqual(a2.discovery_sources, [DiscoverySource.OAUTH_GRANTS])

    def test_to_dict_includes_shadow_it_flag(self):
        a = self._make(approved=False)
        d = a.to_dict()
        self.assertTrue(d["is_shadow_it"])


# ── LicenseManager CRUD ───────────────────────────────────────────────────────

class TestLicenseManagerProductCRUD(unittest.TestCase):

    def setUp(self):
        self._tmpdir = tempfile.TemporaryDirectory()
        self.tmp = Path(self._tmpdir.name)
        self.mgr = _mgr(self.tmp)

    def tearDown(self):
        self._tmpdir.cleanup()

    def test_add_and_list(self):
        p = self.mgr.add_product("AutoCAD", vendor="Autodesk",
                                  license_type=LicenseType.SUBSCRIPTION,
                                  seats=5, annual_cost=2400.0)
        self.assertIsNotNone(p.id)
        products = self.mgr.list_products()
        self.assertEqual(len(products), 1)
        self.assertEqual(products[0].name, "AutoCAD")

    def test_get(self):
        p = self.mgr.add_product("MATLAB", vendor="MathWorks", seats=1)
        found = self.mgr.get_product(p.id)
        self.assertIsNotNone(found)
        self.assertEqual(found.vendor, "MathWorks")

    def test_get_missing(self):
        self.assertIsNone(self.mgr.get_product("nonexistent"))

    def test_update(self):
        p = self.mgr.add_product("Photoshop", seats=10)
        updated = self.mgr.update_product(p.id, seats_licensed=20)
        self.assertEqual(updated.seats_licensed, 20)

    def test_update_missing(self):
        self.assertIsNone(self.mgr.update_product("nope", seats_licensed=5))

    def test_remove(self):
        p = self.mgr.add_product("Office", vendor="Microsoft", seats=50)
        self.assertTrue(self.mgr.remove_product(p.id))
        self.assertIsNone(self.mgr.get_product(p.id))

    def test_remove_missing(self):
        self.assertFalse(self.mgr.remove_product("nope"))

    def test_persistence(self):
        self.mgr.add_product("Figma", vendor="Figma Inc", seats=3)
        # Load fresh instance from same directory
        mgr2 = _mgr(self.tmp)
        products = mgr2.list_products()
        self.assertEqual(len(products), 1)
        self.assertEqual(products[0].name, "Figma")


# ── LicenseManager SaaS CRUD ─────────────────────────────────────────────────

class TestLicenseManagerSaaSCRUD(unittest.TestCase):

    def setUp(self):
        self._tmpdir = tempfile.TemporaryDirectory()
        self.tmp = Path(self._tmpdir.name)
        self.mgr = _mgr(self.tmp)

    def tearDown(self):
        self._tmpdir.cleanup()

    def test_add_and_list(self):
        a = self.mgr.add_saas("GitHub", vendor="GitHub Inc",
                               category=SaaSCategory.DEVTOOLS,
                               monthly_cost=21.0, seats_licensed=10)
        self.assertIsNotNone(a.id)
        apps = self.mgr.list_saas()
        self.assertEqual(len(apps), 1)
        self.assertEqual(apps[0].name, "GitHub")

    def test_approve(self):
        a = self.mgr.add_saas("Notion", approved=False)
        approved = self.mgr.approve_saas(a.id, owner_user_id="alice@example.com")
        self.assertTrue(approved.approved)
        self.assertEqual(approved.owner_user_id, "alice@example.com")

    def test_approve_missing(self):
        self.assertIsNone(self.mgr.approve_saas("nope"))

    def test_remove_saas(self):
        a = self.mgr.add_saas("Zoom")
        self.assertTrue(self.mgr.remove_saas(a.id))
        self.assertIsNone(self.mgr.get_saas(a.id))

    def test_persistence_saas(self):
        self.mgr.add_saas("Jira", vendor="Atlassian",
                           category=SaaSCategory.DEVTOOLS, monthly_cost=8.0,
                           seats_licensed=20, approved=True)
        mgr2 = _mgr(self.tmp)
        self.assertEqual(len(mgr2.list_saas()), 1)
        self.assertEqual(mgr2.list_saas()[0].name, "Jira")


# ── Agent reconciliation ──────────────────────────────────────────────────────

class TestReconcileNode(unittest.TestCase):

    def setUp(self):
        self._tmpdir = tempfile.TemporaryDirectory()
        self.tmp = Path(self._tmpdir.name)
        self.mgr = _mgr(self.tmp)

    def tearDown(self):
        self._tmpdir.cleanup()

    def test_matched_product_updates_installed_nodes(self):
        p = self.mgr.add_product("AutoCAD", vendor="Autodesk", seats=5)
        report = self.mgr.reconcile_node("node-1", [{"name": "AutoCAD", "version": "2024"}])
        self.assertEqual(report["matched_count"], 1)
        self.assertEqual(report["unlicensed_count"], 0)
        self.assertIn("node-1", self.mgr.get_product(p.id).installed_nodes)

    def test_unlicensed_software_detected(self):
        report = self.mgr.reconcile_node("node-1",
                                          [{"name": "Pirated Software", "version": "1.0"}])
        self.assertEqual(report["unlicensed_count"], 1)
        self.assertEqual(report["unlicensed"][0]["name"], "Pirated Software")

    def test_absent_product_detected(self):
        self.mgr.add_product("Office", vendor="Microsoft",
                             license_type=LicenseType.SUBSCRIPTION, seats=10)
        report = self.mgr.reconcile_node("node-1", [])
        self.assertEqual(report["absent_count"], 1)

    def test_open_source_not_in_absent(self):
        """Open-source and freeware products are never flagged as absent."""
        self.mgr.add_product("VLC", license_type=LicenseType.OPEN_SOURCE)
        report = self.mgr.reconcile_node("node-1", [])
        self.assertEqual(report["absent_count"], 0)

    def test_seats_active_updated(self):
        p = self.mgr.add_product("Blender", license_type=LicenseType.OPEN_SOURCE,
                                  seats=3)
        self.mgr.reconcile_node("node-1", [{"name": "Blender", "version": "4.0"}])
        self.mgr.reconcile_node("node-2", [{"name": "Blender", "version": "4.0"}])
        self.assertEqual(self.mgr.get_product(p.id).seats_active, 2)

    def test_node_removed_when_uninstalled(self):
        p = self.mgr.add_product("Unity", seats=5)
        self.mgr.reconcile_node("node-1", [{"name": "Unity", "version": "2023"}])
        self.assertIn("node-1", self.mgr.get_product(p.id).installed_nodes)
        # Reconcile again with empty list (uninstalled)
        self.mgr.reconcile_node("node-1", [])
        self.assertNotIn("node-1", self.mgr.get_product(p.id).installed_nodes)


# ── Analytics ─────────────────────────────────────────────────────────────────

class TestAnalytics(unittest.TestCase):

    def setUp(self):
        self._tmpdir = tempfile.TemporaryDirectory()
        self.tmp = Path(self._tmpdir.name)
        self.mgr = _mgr(self.tmp)

    def tearDown(self):
        self._tmpdir.cleanup()

    def test_find_upcoming_renewals(self):
        soon = time.time() + 20 * 86400
        far = time.time() + 200 * 86400
        self.mgr.add_product("Soon", renewal_date=soon)
        self.mgr.add_product("Far", renewal_date=far)
        renewals = self.mgr.find_upcoming_renewals(days=90)
        self.assertEqual(len(renewals), 1)
        self.assertEqual(renewals[0]["name"], "Soon")

    def test_find_upcoming_renewals_sorted(self):
        t1 = time.time() + 5 * 86400
        t2 = time.time() + 60 * 86400
        self.mgr.add_product("B", renewal_date=t2)
        self.mgr.add_product("A", renewal_date=t1)
        renewals = self.mgr.find_upcoming_renewals(days=90)
        self.assertEqual(renewals[0]["name"], "A")

    def test_find_wasted_seats(self):
        self.mgr.add_product("Bloated", seats=100, annual_cost=5000.0)  # 0% utilisation
        self.mgr.add_product("Tight", seats=2)
        # Manually set seats_active
        p = self.mgr.list_products()[0]
        p.seats_active = 2
        wasted = self.mgr.find_wasted_seats(threshold_pct=50.0)
        # Only "Bloated" is below 50%
        self.assertTrue(any(w["name"] == "Bloated" for w in wasted))

    def test_find_shadow_it(self):
        self.mgr.add_saas("Approved App", approved=True)
        self.mgr.add_saas("Shadow App", approved=False)
        shadow = self.mgr.find_shadow_it()
        self.assertEqual(len(shadow), 1)
        self.assertEqual(shadow[0].name, "Shadow App")

    def test_find_no_dpa(self):
        self.mgr.add_saas("HRTool", category=SaaSCategory.HR,
                           data_categories=["hr"], dpa_signed=False)
        self.mgr.add_saas("SafeTool", category=SaaSCategory.MONITORING,
                           data_categories=["metrics"], dpa_signed=False)
        no_dpa = self.mgr.find_no_dpa()
        self.assertEqual(len(no_dpa), 1)
        self.assertEqual(no_dpa[0].name, "HRTool")

    def test_find_duplicate_categories(self):
        self.mgr.add_saas("Slack", category=SaaSCategory.COMMUNICATION, approved=True)
        self.mgr.add_saas("Teams", category=SaaSCategory.COMMUNICATION, approved=True)
        self.mgr.add_saas("Zoom", category=SaaSCategory.COMMUNICATION, approved=False)
        dups = self.mgr.find_duplicate_categories()
        # Only approved apps counted; Zoom is excluded
        self.assertIn("communication", dups)
        self.assertEqual(len(dups["communication"]), 2)

    def test_offboarding_checklist(self):
        a1 = self.mgr.add_saas("Slack", sso_integrated=True, url="https://slack.com",
                                users=["alice@example.com"])
        a2 = self.mgr.add_saas("Legacy", sso_integrated=False, url="https://legacy.com",
                                users=["alice@example.com", "bob@example.com"])
        checklist = self.mgr.offboarding_checklist("alice@example.com")
        self.assertEqual(len(checklist), 2)
        # Manual actions first (sorted: auto last)
        manual = [c for c in checklist if c["action"] == "manual"]
        auto_ = [c for c in checklist if c["action"] == "auto"]
        self.assertEqual(len(manual), 1)
        self.assertEqual(len(auto_), 1)

    def test_offboarding_excludes_other_users(self):
        self.mgr.add_saas("BobOnly", users=["bob@example.com"])
        checklist = self.mgr.offboarding_checklist("alice@example.com")
        self.assertEqual(len(checklist), 0)

    def test_cost_summary(self):
        self.mgr.add_product("P1", annual_cost=1200.0)
        self.mgr.add_saas("S1", monthly_cost=100.0)
        summary = self.mgr.cost_summary()
        self.assertAlmostEqual(summary["software_annual"], 1200.0)
        self.assertAlmostEqual(summary["saas_annual"], 1200.0)
        self.assertAlmostEqual(summary["total_annual"], 2400.0)
        self.assertEqual(summary["software_products"], 1)
        self.assertEqual(summary["saas_apps"], 1)

    def test_cost_summary_shadow_it_count(self):
        self.mgr.add_saas("Approved", approved=True)
        self.mgr.add_saas("Shadow", approved=False)
        summary = self.mgr.cost_summary()
        self.assertEqual(summary["shadow_it_count"], 1)


# ── Renewal alert logic ───────────────────────────────────────────────────────

class TestRenewalAlerts(unittest.TestCase):

    def setUp(self):
        self._tmpdir = tempfile.TemporaryDirectory()
        self.tmp = Path(self._tmpdir.name)

    def tearDown(self):
        self._tmpdir.cleanup()

    def test_maybe_alert_no_renewal(self):
        mgr = _mgr(self.tmp)
        result = mgr._maybe_alert("id", "software", "X", 0.0, 0.0, time.time())
        self.assertIsNone(result)

    def test_maybe_alert_expired(self):
        mgr = _mgr(self.tmp)
        past = time.time() - 86400
        result = mgr._maybe_alert("id", "software", "X", past, 0.0, time.time())
        self.assertIsNone(result)

    def test_maybe_alert_within_7_days_is_urgent(self):
        mgr = _mgr(self.tmp)
        soon = time.time() + 5 * 86400
        alert = mgr._maybe_alert("id", "software", "Name", soon, 500.0, time.time())
        self.assertIsNotNone(alert)
        self.assertEqual(alert.severity, AlertSeverity.URGENT)

    def test_maybe_alert_within_30_days_is_warning(self):
        mgr = _mgr(self.tmp)
        t = time.time() + 20 * 86400
        alert = mgr._maybe_alert("id", "saas", "Name", t, 500.0, time.time())
        self.assertIsNotNone(alert)
        self.assertEqual(alert.severity, AlertSeverity.WARNING)

    def test_maybe_alert_within_90_days_is_info(self):
        mgr = _mgr(self.tmp)
        t = time.time() + 60 * 86400
        alert = mgr._maybe_alert("id", "saas", "Name", t, 500.0, time.time())
        self.assertIsNotNone(alert)
        self.assertEqual(alert.severity, AlertSeverity.INFO)

    def test_maybe_alert_beyond_90_days_is_none(self):
        mgr = _mgr(self.tmp)
        t = time.time() + 200 * 86400
        result = mgr._maybe_alert("id", "software", "Name", t, 0.0, time.time())
        self.assertIsNone(result)

    def test_run_renewal_check_now_fires_callback(self):
        alerts_received = []

        async def on_alert(d):
            alerts_received.append(d)

        mgr = _mgr(self.tmp, on_alert=on_alert)
        soon = time.time() + 5 * 86400
        mgr.add_product("Expiring", renewal_date=soon, annual_cost=999.0)

        loop = asyncio.new_event_loop()
        fired = loop.run_until_complete(mgr.run_renewal_check_now())
        loop.close()

        self.assertEqual(len(fired), 1)
        self.assertEqual(len(alerts_received), 1)
        self.assertEqual(alerts_received[0]["resource_name"], "Expiring")

    def test_run_renewal_check_now_deduplicates(self):
        """Same alert should not fire twice in the same cycle."""
        calls = []

        async def on_alert(d):
            calls.append(d)

        mgr = _mgr(self.tmp, on_alert=on_alert)
        soon = time.time() + 5 * 86400
        mgr.add_product("Expiring", renewal_date=soon)

        loop = asyncio.new_event_loop()
        loop.run_until_complete(mgr.run_renewal_check_now())
        loop.run_until_complete(mgr.run_renewal_check_now())  # second call
        loop.close()

        # Callback should only have been called once
        self.assertEqual(len(calls), 1)

    def test_renewal_alert_to_dict(self):
        alert = RenewalAlert(
            id="a1", resource_id="r1", resource_type="software",
            resource_name="Office", days_remaining=7,
            severity=AlertSeverity.URGENT, renewal_date=time.time() + 7 * 86400,
            annual_cost=1200.0, fired_at=time.time(),
        )
        d = alert.to_dict()
        self.assertEqual(d["severity"], "urgent")
        self.assertEqual(d["days_remaining"], 7)


if __name__ == "__main__":
    unittest.main()
