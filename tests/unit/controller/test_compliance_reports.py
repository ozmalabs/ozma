#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Unit tests for ComplianceReportEngine — E8, ISO 27001, SOC 2 compliance.
"""

import asyncio
import json
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest.mock import MagicMock, patch

sys.path.insert(0, str(Path(__file__).parent.parent.parent.parent / "controller"))

from compliance_reports import (
    ComplianceReportEngine, ComplianceConfig, ComplianceGap, ComplianceReport,
    ControlResult, EvidenceCollector,
    _evaluate_e8, _evaluate_iso27001, _evaluate_soc2,
    E8_CONTROLS, ISO27001_CONTROLS, SOC2_CRITERIA,
)


def _engine(tmp: Path) -> ComplianceReportEngine:
    return ComplianceReportEngine(tmp)


def _run(coro):
    return asyncio.get_event_loop().run_until_complete(coro)


# ── Data model ────────────────────────────────────────────────────────────────

class TestControlResult(unittest.TestCase):
    def test_to_dict(self):
        cr = ControlResult(
            control_id="e8.backups.ml1",
            control_name="Backups ML1",
            status="pass",
            evidence={"backup_configured": True},
        )
        d = cr.to_dict()
        self.assertEqual(d["control_id"], "e8.backups.ml1")
        self.assertEqual(d["status"], "pass")
        self.assertTrue(d["evidence"]["backup_configured"])


class TestComplianceGap(unittest.TestCase):
    def test_roundtrip(self):
        gap = ComplianceGap(
            id="g1",
            framework="iso27001_2022",
            control_id="iso27001.8.8",
            control_name="8.8: Technical Vulnerabilities",
            description="Critical CVEs unpatched",
            remediation="Apply patches",
            severity="critical",
            first_seen=1000.0,
        )
        d = gap.to_dict()
        restored = ComplianceGap.from_dict(d)
        self.assertEqual(restored.control_id, "iso27001.8.8")
        self.assertEqual(restored.severity, "critical")
        self.assertFalse(restored.resolved)


class TestComplianceConfig(unittest.TestCase):
    def test_defaults(self):
        c = ComplianceConfig()
        self.assertIn("essential_eight_ml1", c.active_frameworks)
        self.assertEqual(c.country, "AU")

    def test_roundtrip(self):
        c = ComplianceConfig(
            industry="financial",
            employee_count=50,
            has_development_team=True,
            active_frameworks=["iso27001_2022", "soc2_type1"],
        )
        c2 = ComplianceConfig.from_dict(c.to_dict())
        self.assertEqual(c2.industry, "financial")
        self.assertEqual(c2.employee_count, 50)
        self.assertIn("soc2_type1", c2.active_frameworks)


# ── Evidence collector ────────────────────────────────────────────────────────

class TestEvidenceCollector(unittest.TestCase):
    def test_collect_no_managers(self):
        ec = EvidenceCollector()
        result = ec.collect()
        # Should return empty dict (no managers configured)
        self.assertIsInstance(result, dict)

    def test_collect_backup_manager(self):
        ec = EvidenceCollector()
        mock_backup = MagicMock()
        mock_backup.get_status.return_value = {
            "sources_configured": 2,
            "last_success": time.time(),
            "object_lock_enabled": True,
            "recovery_tested": False,
        }
        ec.backup_mgr = mock_backup
        result = ec.collect()
        self.assertTrue(result["backup_configured"])
        self.assertTrue(result["backup_object_lock"])
        self.assertFalse(result["backup_recovery_tested"])

    def test_collect_threat_intel(self):
        ec = EvidenceCollector()
        mock_ti = MagicMock()
        mock_ti.status.return_value = {
            "enabled": True,
            "kev_entries": 100,
            "last_kev_poll": time.time(),
            "kev_matches_in_estate": 3,
        }
        ec.threat_intel = mock_ti
        result = ec.collect()
        self.assertTrue(result["threat_intel_configured"])
        self.assertEqual(result["cve_open_critical"], 3)

    def test_collect_graceful_on_exception(self):
        ec = EvidenceCollector()
        mock_bad = MagicMock()
        mock_bad.status.side_effect = Exception("broken")
        ec.backup_mgr = mock_bad  # backup expects get_status, not status
        # Should not raise
        result = ec.collect()
        self.assertIsInstance(result, dict)

    def test_collect_dlp(self):
        ec = EvidenceCollector()
        mock_dlp = MagicMock()
        mock_dlp.status.return_value = {
            "active_policies": 2,
            "policies": 2,
            "incidents_open": 0,
        }
        ec.dlp = mock_dlp
        result = ec.collect()
        self.assertTrue(result["dlp_configured"])
        self.assertEqual(result["dlp_policy_count"], 2)


# ── Framework evaluators ──────────────────────────────────────────────────────

class TestE8Evaluation(unittest.TestCase):
    def _evidence(self, **kwargs) -> dict:
        defaults = {
            "backup_configured": False,
            "backup_object_lock": False,
            "backup_recovery_tested": False,
            "cve_open_critical": 0,
            "network_scan_configured": False,
            "threat_intel_configured": False,
            "dlp_configured": False,
            "mdm_configured": False,
        }
        defaults.update(kwargs)
        return defaults

    def test_ml1_backup_pass(self):
        ev = self._evidence(backup_configured=True)
        results = _evaluate_e8(1, ev)
        backup = next(r for r in results if r.control_id == "e8.backups.ml1")
        self.assertEqual(backup.status, "pass")

    def test_ml1_backup_fail(self):
        ev = self._evidence(backup_configured=False)
        results = _evaluate_e8(1, ev)
        backup = next(r for r in results if r.control_id == "e8.backups.ml1")
        self.assertEqual(backup.status, "fail")
        self.assertNotEqual(backup.gap_description, "")
        self.assertEqual(backup.severity, "critical")

    def test_ml2_includes_ml1_controls(self):
        ev = self._evidence()
        ml1_results = _evaluate_e8(1, ev)
        ml2_results = _evaluate_e8(2, ev)
        ml1_ids = {r.control_id for r in ml1_results}
        ml2_ids = {r.control_id for r in ml2_results}
        # ML2 should include all ML1 controls plus more
        self.assertTrue(ml1_ids.issubset(ml2_ids))

    def test_ml3_object_lock_required(self):
        ev = self._evidence(backup_configured=True, backup_object_lock=True)
        results = _evaluate_e8(3, ev)
        ml3_backup = next((r for r in results if r.control_id == "e8.backups.ml3"), None)
        self.assertIsNotNone(ml3_backup)
        self.assertEqual(ml3_backup.status, "pass")

    def test_ml3_object_lock_fail(self):
        ev = self._evidence(backup_configured=True, backup_object_lock=False)
        results = _evaluate_e8(3, ev)
        ml3_backup = next((r for r in results if r.control_id == "e8.backups.ml3"), None)
        self.assertIsNotNone(ml3_backup)
        self.assertEqual(ml3_backup.status, "fail")

    def test_mfa_is_manual(self):
        ev = self._evidence()
        results = _evaluate_e8(1, ev)
        mfa = next(r for r in results if "mfa" in r.control_id)
        self.assertEqual(mfa.status, "manual")

    def test_patch_with_open_cves_is_partial(self):
        ev = self._evidence(network_scan_configured=True, cve_open_critical=5)
        results = _evaluate_e8(1, ev)
        patch = next(r for r in results if r.control_id == "e8.patch_apps.ml1")
        self.assertEqual(patch.status, "partial")
        self.assertIn("5", patch.gap_description)

    def test_patch_no_open_cves_passes(self):
        ev = self._evidence(network_scan_configured=True, cve_open_critical=0)
        results = _evaluate_e8(1, ev)
        patch = next(r for r in results if r.control_id == "e8.patch_apps.ml1")
        self.assertEqual(patch.status, "pass")


class TestISO27001Evaluation(unittest.TestCase):
    def _evidence(self, **kwargs) -> dict:
        defaults = {
            "backup_configured": False,
            "backup_object_lock": False,
            "audit_log_entries": 0,
            "audit_log_integrity": False,
            "threat_intel_configured": False,
            "cve_open_critical": 0,
            "network_scan_configured": False,
            "dlp_configured": False,
            "mdm_configured": False,
            "itsm_configured": False,
            "itam_asset_count": 0,
            "saas_apps_tracked": 0,
            "saas_vendor_risk": 0,
        }
        defaults.update(kwargs)
        return defaults

    def test_iso_backup_control(self):
        ev = self._evidence(backup_configured=True)
        results = _evaluate_iso27001(ev)
        backup_ctrl = next(r for r in results if r.control_id == "iso27001.8.13")
        self.assertEqual(backup_ctrl.status, "pass")

    def test_iso_threat_intel_control(self):
        ev = self._evidence(threat_intel_configured=True)
        results = _evaluate_iso27001(ev)
        ti_ctrl = next(r for r in results if r.control_id == "iso27001.5.7")
        self.assertEqual(ti_ctrl.status, "pass")

    def test_iso_cve_control_fail(self):
        ev = self._evidence(cve_open_critical=3)
        results = _evaluate_iso27001(ev)
        cve_ctrl = next(r for r in results if r.control_id == "iso27001.8.8")
        self.assertEqual(cve_ctrl.status, "fail")

    def test_iso_cve_control_pass(self):
        ev = self._evidence(cve_open_critical=0)
        results = _evaluate_iso27001(ev)
        cve_ctrl = next(r for r in results if r.control_id == "iso27001.8.8")
        self.assertEqual(cve_ctrl.status, "pass")

    def test_iso_manual_controls_exist(self):
        ev = self._evidence()
        results = _evaluate_iso27001(ev)
        manual = [r for r in results if r.status == "manual"]
        self.assertGreater(len(manual), 0)

    def test_iso_all_controls_present(self):
        ev = self._evidence()
        results = _evaluate_iso27001(ev)
        result_ids = {r.control_id.replace("iso27001.", "") for r in results}
        for control_id in ISO27001_CONTROLS:
            self.assertIn(control_id, result_ids, f"Missing control {control_id}")


class TestSOC2Evaluation(unittest.TestCase):
    def _evidence(self, **kwargs) -> dict:
        defaults = {
            "mdm_configured": False,
            "network_scan_configured": False,
            "itsm_configured": False,
            "audit_log_integrity": False,
            "dlp_configured": False,
            "saas_vendor_risk": 0,
        }
        defaults.update(kwargs)
        return defaults

    def test_soc2_cc6_1_with_mdm(self):
        ev = self._evidence(mdm_configured=True)
        results = _evaluate_soc2(ev)
        cc6_1 = next(r for r in results if "CC6.1" in r.control_id)
        self.assertEqual(cc6_1.status, "pass")

    def test_soc2_cc6_6_always_pass(self):
        # Transport encryption is architectural — always pass
        ev = self._evidence()
        results = _evaluate_soc2(ev)
        cc6_6 = next((r for r in results if "CC6.6" in r.control_id), None)
        if cc6_6:
            self.assertEqual(cc6_6.status, "pass")

    def test_soc2_manual_criteria_exist(self):
        ev = self._evidence()
        results = _evaluate_soc2(ev)
        manual = [r for r in results if r.status == "manual"]
        self.assertGreater(len(manual), 0)


# ── Engine: report generation ─────────────────────────────────────────────────

class TestReportGeneration(unittest.TestCase):
    def setUp(self):
        self._tmp = Path(tempfile.mkdtemp())
        self._engine = _engine(self._tmp)
        _run(self._engine.start())

    def tearDown(self):
        _run(self._engine.stop())

    def test_generate_e8_ml1_report(self):
        report = _run(self._engine.generate_report("essential_eight_ml1"))
        self.assertIsNotNone(report)
        self.assertEqual(report.framework, "essential_eight_ml1")
        self.assertGreater(len(report.controls), 0)
        self.assertIn("total_controls", report.summary)
        self.assertIn("score_pct", report.summary)
        self.assertNotEqual(report.signature, "")

    def test_generate_iso27001_report(self):
        report = _run(self._engine.generate_report("iso27001_2022"))
        self.assertEqual(report.framework, "iso27001_2022")
        self.assertEqual(report.summary["total_controls"], len(ISO27001_CONTROLS))

    def test_generate_soc2_report(self):
        report = _run(self._engine.generate_report("soc2_type1"))
        self.assertEqual(report.framework, "soc2_type1")
        self.assertGreater(len(report.controls), 0)

    def test_invalid_framework_raises(self):
        with self.assertRaises(ValueError):
            _run(self._engine.generate_report("invalid_framework"))

    def test_report_persisted(self):
        report = _run(self._engine.generate_report("essential_eight_ml1"))
        reports = self._engine.list_reports()
        ids = [r["id"] for r in reports]
        self.assertIn(report.id, ids)

    def test_get_report(self):
        report = _run(self._engine.generate_report("essential_eight_ml1"))
        fetched = self._engine.get_report(report.id)
        self.assertIsNotNone(fetched)
        self.assertEqual(fetched.framework, "essential_eight_ml1")

    def test_get_nonexistent_report(self):
        self.assertIsNone(self._engine.get_report("nonexistent"))

    def test_report_signature_is_sha256(self):
        report = _run(self._engine.generate_report("essential_eight_ml1"))
        # SHA256 hex is 64 chars
        self.assertEqual(len(report.signature), 64)

    def test_report_fires_event(self):
        q: asyncio.Queue = asyncio.Queue()
        self._engine._event_queue = q
        _run(self._engine.generate_report("essential_eight_ml1"))
        events = []
        while not q.empty():
            events.append(q.get_nowait())
        types = [e["type"] for e in events]
        self.assertIn("compliance.report.generated", types)


# ── Engine: gap management ────────────────────────────────────────────────────

class TestGapManagement(unittest.TestCase):
    def setUp(self):
        self._tmp = Path(tempfile.mkdtemp())
        self._engine = _engine(self._tmp)
        _run(self._engine.start())

    def tearDown(self):
        _run(self._engine.stop())

    def test_gaps_created_from_failed_controls(self):
        # No backup → backup control fails → gap created
        _run(self._engine.generate_report("essential_eight_ml1"))
        gaps = self._engine.list_gaps(resolved=False)
        self.assertGreater(len(gaps), 0)

    def test_gaps_filter_by_framework(self):
        _run(self._engine.generate_report("essential_eight_ml1"))
        _run(self._engine.generate_report("iso27001_2022"))
        e8_gaps = self._engine.list_gaps(framework="essential_eight_ml1")
        iso_gaps = self._engine.list_gaps(framework="iso27001_2022")
        # Both should have gaps (no managers configured)
        self.assertGreater(len(e8_gaps), 0)
        self.assertGreater(len(iso_gaps), 0)
        # Gaps should be in different frameworks
        e8_ids = {g.control_id for g in e8_gaps}
        iso_ids = {g.control_id for g in iso_gaps}
        self.assertFalse(e8_ids.intersection(iso_ids))

    def test_resolve_gap(self):
        _run(self._engine.generate_report("essential_eight_ml1"))
        gaps = self._engine.list_gaps(resolved=False)
        self.assertGreater(len(gaps), 0)
        gap_id = gaps[0].id
        resolved_gap = self._engine.resolve_gap(gap_id)
        self.assertIsNotNone(resolved_gap)
        self.assertTrue(resolved_gap.resolved)
        # Should not appear in open gaps anymore
        open_gaps = self._engine.list_gaps(resolved=False)
        open_ids = [g.id for g in open_gaps]
        self.assertNotIn(gap_id, open_ids)

    def test_resolve_nonexistent_gap(self):
        self.assertIsNone(self._engine.resolve_gap("nonexistent"))

    def test_gaps_filter_by_severity(self):
        _run(self._engine.generate_report("essential_eight_ml1"))
        critical = self._engine.list_gaps(severity="critical", resolved=False)
        # Backup not configured → critical gap
        self.assertGreater(len(critical), 0)

    def test_gaps_auto_resolve_when_control_passes(self):
        # First report: no backup → gap created
        _run(self._engine.generate_report("essential_eight_ml1"))
        initial_gaps = self._engine.list_gaps(
            framework="essential_eight_ml1", resolved=False
        )
        backup_gap = next(
            (g for g in initial_gaps if "backup" in g.control_id.lower()), None
        )
        self.assertIsNotNone(backup_gap)

        # Inject backup manager that says backup is configured
        mock_backup = MagicMock()
        mock_backup.get_status.return_value = {
            "sources_configured": 1,
            "last_success": time.time(),
            "object_lock_enabled": True,
            "recovery_tested": True,
        }
        self._engine._evidence.backup_mgr = mock_backup

        # Second report: backup now passing → gap should be resolved
        _run(self._engine.generate_report("essential_eight_ml1"))
        still_open = self._engine.list_gaps(
            framework="essential_eight_ml1", resolved=False
        )
        open_ids = [g.id for g in still_open]
        self.assertNotIn(backup_gap.id, open_ids)


# ── Engine: SoA generation ────────────────────────────────────────────────────

class TestSoAGeneration(unittest.TestCase):
    def setUp(self):
        self._tmp = Path(tempfile.mkdtemp())
        self._engine = _engine(self._tmp)

    def test_soa_structure(self):
        soa = _run(self._engine.generate_soa())
        self.assertIn("title", soa)
        self.assertIn("standard", soa)
        self.assertIn("total_controls", soa)
        self.assertIn("coverage_pct", soa)
        self.assertIn("entries", soa)
        self.assertEqual(soa["standard"], "ISO/IEC 27001:2022")

    def test_soa_entry_structure(self):
        soa = _run(self._engine.generate_soa())
        entry = soa["entries"][0]
        self.assertIn("control_id", entry)
        self.assertIn("applicable", entry)
        self.assertIn("implementation_status", entry)
        self.assertIn("justification", entry)

    def test_soa_coverage_is_float(self):
        soa = _run(self._engine.generate_soa())
        self.assertIsInstance(soa["coverage_pct"], float)
        self.assertGreaterEqual(soa["coverage_pct"], 0.0)
        self.assertLessEqual(soa["coverage_pct"], 100.0)


# ── Persistence ───────────────────────────────────────────────────────────────

class TestPersistence(unittest.TestCase):
    def test_config_persists(self):
        tmp = Path(tempfile.mkdtemp())
        eng1 = ComplianceReportEngine(tmp)
        _run(eng1.start())
        eng1.set_config({"industry": "healthcare", "employee_count": 100})
        _run(eng1.stop())

        eng2 = ComplianceReportEngine(tmp)
        _run(eng2.start())
        cfg = eng2.get_config()
        _run(eng2.stop())
        self.assertEqual(cfg.industry, "healthcare")
        self.assertEqual(cfg.employee_count, 100)

    def test_gaps_persist(self):
        tmp = Path(tempfile.mkdtemp())
        eng1 = ComplianceReportEngine(tmp)
        _run(eng1.start())
        _run(eng1.generate_report("essential_eight_ml1"))
        gap_count = len(eng1.list_gaps(resolved=False))
        _run(eng1.stop())

        eng2 = ComplianceReportEngine(tmp)
        _run(eng2.start())
        loaded_count = len(eng2.list_gaps(resolved=False))
        _run(eng2.stop())
        self.assertEqual(gap_count, loaded_count)

    def test_reports_persist(self):
        tmp = Path(tempfile.mkdtemp())
        eng1 = ComplianceReportEngine(tmp)
        _run(eng1.start())
        report = _run(eng1.generate_report("essential_eight_ml1"))
        _run(eng1.stop())

        eng2 = ComplianceReportEngine(tmp)
        _run(eng2.start())
        fetched = eng2.get_report(report.id)
        _run(eng2.stop())
        self.assertIsNotNone(fetched)
        self.assertEqual(fetched.id, report.id)


# ── Status ────────────────────────────────────────────────────────────────────

class TestStatus(unittest.TestCase):
    def setUp(self):
        self._tmp = Path(tempfile.mkdtemp())
        self._engine = _engine(self._tmp)
        _run(self._engine.start())

    def tearDown(self):
        _run(self._engine.stop())

    def test_status_structure(self):
        s = self._engine.status()
        self.assertIn("active_frameworks", s)
        self.assertIn("total_gaps", s)
        self.assertIn("critical_gaps", s)
        self.assertIn("reports_generated", s)

    def test_status_after_report(self):
        _run(self._engine.generate_report("essential_eight_ml1"))
        s = self._engine.status()
        self.assertGreater(s["reports_generated"], 0)
        self.assertGreater(s["total_gaps"], 0)


# ── Lifecycle ─────────────────────────────────────────────────────────────────

class TestLifecycle(unittest.TestCase):
    def test_start_stop(self):
        tmp = Path(tempfile.mkdtemp())
        eng = ComplianceReportEngine(tmp)
        _run(eng.start())
        self.assertIsNotNone(eng._task)
        _run(eng.stop())
        self.assertTrue(eng._task.done())

    def test_stop_without_start(self):
        tmp = Path(tempfile.mkdtemp())
        eng = ComplianceReportEngine(tmp)
        _run(eng.stop())  # Should not raise


if __name__ == "__main__":
    unittest.main()
