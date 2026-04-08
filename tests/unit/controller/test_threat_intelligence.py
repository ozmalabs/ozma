#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Unit tests for ThreatIntelligenceEngine — KEV, advisories, credential exposure,
ATT&CK coverage, posture adjustment.
"""

import asyncio
import json
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest.mock import patch, AsyncMock, MagicMock

sys.path.insert(0, str(Path(__file__).parent.parent.parent.parent / "controller"))

from threat_intelligence import (
    ThreatIntelligenceEngine, ThreatConfig,
    KEVEntry, Advisory, CredentialExposure, PostureChange,
    ATTACK_COVERAGE, SECTOR_KEYWORDS,
    _levenshtein,
)


def _engine(tmp: Path) -> ThreatIntelligenceEngine:
    return ThreatIntelligenceEngine(tmp)


def _run(coro):
    return asyncio.run(coro)


# ── Levenshtein ───────────────────────────────────────────────────────────────

class TestLevenshtein(unittest.TestCase):
    def test_identical(self):
        self.assertEqual(_levenshtein("abc", "abc"), 0)

    def test_one_insertion(self):
        self.assertEqual(_levenshtein("abc", "abcd"), 1)

    def test_one_deletion(self):
        self.assertEqual(_levenshtein("abcd", "abc"), 1)

    def test_one_substitution(self):
        self.assertEqual(_levenshtein("abc", "xbc"), 1)

    def test_empty_strings(self):
        self.assertEqual(_levenshtein("", ""), 0)
        self.assertEqual(_levenshtein("abc", ""), 3)
        self.assertEqual(_levenshtein("", "abc"), 3)

    def test_typical_typosquat(self):
        # "paypa1" vs "paypal" — 1 substitution
        self.assertEqual(_levenshtein("paypal", "paypa1"), 1)


# ── Typosquat detection heuristic ────────────────────────────────────────────

class TestTyposquatHeuristic(unittest.TestCase):
    def test_near_match_is_typosquat(self):
        # "paypa1.com" vs "paypal.com" — base distance = 1
        self.assertTrue(ThreatIntelligenceEngine._is_typosquat("paypal.com", "paypa1.com"))

    def test_exact_match_not_typosquat(self):
        self.assertFalse(ThreatIntelligenceEngine._is_typosquat("paypal.com", "paypal.com"))

    def test_subdomain_not_typosquat(self):
        self.assertFalse(ThreatIntelligenceEngine._is_typosquat("paypal.com", "api.paypal.com"))

    def test_different_name_not_typosquat(self):
        # "completely-different.com" — Levenshtein > 2
        self.assertFalse(
            ThreatIntelligenceEngine._is_typosquat("paypal.com", "completely-different.com")
        )

    def test_transposition(self):
        # "paylpal" vs "paypal" — 1 transposition
        self.assertTrue(
            ThreatIntelligenceEngine._is_typosquat("paypal.com", "paylpal.com")
        )


# ── Models ────────────────────────────────────────────────────────────────────

class TestKEVEntry(unittest.TestCase):
    def test_roundtrip(self):
        entry = KEVEntry(
            cve_id="CVE-2024-1234",
            vendor="Acme Corp",
            product="FooApp",
            vulnerability_name="Remote Code Execution in FooApp",
            date_added="2024-01-15",
            short_description="A critical RCE",
            required_action="Apply vendor patch",
            due_date="2024-02-15",
            first_seen=1000.0,
            matched_sbom=True,
        )
        d = entry.to_dict()
        restored = KEVEntry.from_dict(d)
        self.assertEqual(restored.cve_id, "CVE-2024-1234")
        self.assertTrue(restored.matched_sbom)
        self.assertEqual(restored.vendor, "Acme Corp")


class TestAdvisory(unittest.TestCase):
    def test_roundtrip(self):
        adv = Advisory(
            id="2024-001",
            title="Critical Ransomware Campaign Targeting Finance",
            source="acsc",
            published="2024-01-15",
            severity="critical",
            summary="Active ransomware campaign",
            cves=["CVE-2024-001"],
            attack_techniques=["T1486"],
            sectors=["financial"],
            first_seen=1000.0,
        )
        d = adv.to_dict()
        restored = Advisory.from_dict(d)
        self.assertEqual(restored.id, "2024-001")
        self.assertEqual(restored.severity, "critical")
        self.assertIn("T1486", restored.attack_techniques)
        self.assertIn("financial", restored.sectors)


class TestCredentialExposure(unittest.TestCase):
    def test_roundtrip(self):
        exp = CredentialExposure(
            id="abc123",
            domain="company.com",
            email="alice@company.com",
            breach_name="DataBreach2024",
            breach_date="2024-01-01",
            data_classes=["Passwords", "Email addresses"],
            first_seen=1000.0,
        )
        d = exp.to_dict()
        restored = CredentialExposure.from_dict(d)
        self.assertEqual(restored.domain, "company.com")
        self.assertIn("Passwords", restored.data_classes)
        self.assertFalse(restored.resolved)


class TestPostureChange(unittest.TestCase):
    def test_roundtrip(self):
        pc = PostureChange(
            id="pc-1",
            timestamp=1000.0,
            change_type="monitoring_elevated",
            description="Elevated monitoring due to advisory",
            source_advisory_id="2024-001",
            auto_applied=True,
            approved=True,
        )
        d = pc.to_dict()
        restored = PostureChange.from_dict(d)
        self.assertEqual(restored.change_type, "monitoring_elevated")
        self.assertTrue(restored.auto_applied)
        self.assertEqual(restored.source_advisory_id, "2024-001")


class TestThreatConfig(unittest.TestCase):
    def test_defaults(self):
        c = ThreatConfig()
        self.assertTrue(c.enabled)
        self.assertEqual(c.region, "AU")
        self.assertEqual(c.kev_poll_interval, 86400)
        self.assertIn("critical", c.auto_adjust_severities)

    def test_roundtrip(self):
        c = ThreatConfig(
            monitored_domains=["company.com"],
            sector="financial",
            hibp_api_key="test-key",
            kev_poll_interval=3600,
        )
        c2 = ThreatConfig.from_dict(c.to_dict())
        self.assertEqual(c2.monitored_domains, ["company.com"])
        self.assertEqual(c2.sector, "financial")
        self.assertEqual(c2.kev_poll_interval, 3600)


# ── Engine: KEV processing ────────────────────────────────────────────────────

class TestKEVProcessing(unittest.TestCase):
    def setUp(self):
        self._tmp = Path(tempfile.mkdtemp())
        self._engine = _engine(self._tmp)
        _run(self._engine.start())

    def tearDown(self):
        _run(self._engine.stop())

    def _fake_kev_response(self, cve_ids: list[str]) -> dict:
        return {
            "vulnerabilities": [
                {
                    "cveID": cve_id,
                    "vendorProject": "TestVendor",
                    "product": "TestProduct",
                    "vulnerabilityName": f"Test vuln for {cve_id}",
                    "dateAdded": "2024-01-01",
                    "shortDescription": "A test vulnerability",
                    "requiredAction": "Apply patch",
                    "dueDate": "2024-02-01",
                }
                for cve_id in cve_ids
            ]
        }

    def test_poll_kev_adds_entries(self):
        fake = self._fake_kev_response(["CVE-2024-0001", "CVE-2024-0002"])
        with patch("threat_intelligence._async_http_get_json",
                   new=AsyncMock(return_value=fake)):
            new_entries = _run(self._engine.poll_cisa_kev())
        self.assertEqual(len(new_entries), 2)
        self.assertIn("CVE-2024-0001", [e.cve_id for e in new_entries])

    def test_poll_kev_deduplicates(self):
        fake = self._fake_kev_response(["CVE-2024-0001"])
        with patch("threat_intelligence._async_http_get_json",
                   new=AsyncMock(return_value=fake)):
            _run(self._engine.poll_cisa_kev())
            new_entries = _run(self._engine.poll_cisa_kev())
        # Second poll should find no new entries
        self.assertEqual(len(new_entries), 0)

    def test_poll_kev_marks_sbom_match(self):
        # Set up SBOM before poll
        self._engine._sbom_cves.add("CVE-2024-0001")
        fake = self._fake_kev_response(["CVE-2024-0001", "CVE-2024-0002"])
        with patch("threat_intelligence._async_http_get_json",
                   new=AsyncMock(return_value=fake)):
            _run(self._engine.poll_cisa_kev())
        kev = self._engine._kev.get("CVE-2024-0001")
        self.assertIsNotNone(kev)
        self.assertTrue(kev.matched_sbom)
        # CVE not in SBOM should not be matched
        kev2 = self._engine._kev.get("CVE-2024-0002")
        self.assertFalse(kev2.matched_sbom)

    def test_poll_kev_network_error_graceful(self):
        with patch("threat_intelligence._async_http_get_json",
                   new=AsyncMock(side_effect=Exception("network error"))):
            result = _run(self._engine.poll_cisa_kev())
        self.assertEqual(result, [])

    def test_update_sbom_cves(self):
        fake = self._fake_kev_response(["CVE-2024-0001"])
        with patch("threat_intelligence._async_http_get_json",
                   new=AsyncMock(return_value=fake)):
            _run(self._engine.poll_cisa_kev())
        # Now update SBOM to include this CVE
        matches = self._engine.update_sbom_cves({"CVE-2024-0001"})
        self.assertEqual(matches, 1)
        self.assertTrue(self._engine._kev["CVE-2024-0001"].matched_sbom)

    def test_list_kev_filter(self):
        fake = self._fake_kev_response(["CVE-2024-0001", "CVE-2024-0002"])
        with patch("threat_intelligence._async_http_get_json",
                   new=AsyncMock(return_value=fake)):
            _run(self._engine.poll_cisa_kev())
        self._engine.update_sbom_cves({"CVE-2024-0001"})
        matched = self._engine.list_kev(matched_sbom=True)
        not_matched = self._engine.list_kev(matched_sbom=False)
        self.assertEqual(len(matched), 1)
        self.assertEqual(matched[0].cve_id, "CVE-2024-0001")
        self.assertEqual(len(not_matched), 1)


# ── Engine: advisory ingestion ────────────────────────────────────────────────

class TestAdvisoryIngestion(unittest.TestCase):
    def setUp(self):
        self._tmp = Path(tempfile.mkdtemp())
        self._engine = _engine(self._tmp)
        _run(self._engine.start())

    def tearDown(self):
        _run(self._engine.stop())

    def test_ingest_advisory_manual(self):
        adv = self._engine.ingest_advisory({
            "id": "TEST-001",
            "title": "Critical RCE in Apache Log4j",
            "source": "manual",
            "published": "2024-01-01",
            "severity": "critical",
            "cves": ["CVE-2021-44228"],
        })
        self.assertEqual(adv.id, "TEST-001")
        self.assertIn("TEST-001", self._engine._advisories)

    def test_ingest_deduplicates(self):
        self._engine.ingest_advisory({
            "id": "TEST-001", "title": "Test", "source": "manual",
            "published": "2024-01-01",
        })
        self._engine.ingest_advisory({
            "id": "TEST-001", "title": "Updated Title", "source": "manual",
            "published": "2024-01-01",
        })
        # Both calls update the record; there should still be one
        self.assertEqual(len(self._engine._advisories), 1)

    def test_acknowledge_advisory(self):
        self._engine.ingest_advisory({
            "id": "ADV-001", "title": "Test", "source": "acsc",
            "published": "2024-01-01",
        })
        result = self._engine.acknowledge_advisory("ADV-001")
        self.assertIsNotNone(result)
        self.assertTrue(result.acknowledged)

    def test_acknowledge_nonexistent(self):
        self.assertIsNone(self._engine.acknowledge_advisory("nonexistent"))

    def test_list_advisories_filters(self):
        self._engine.ingest_advisory({
            "id": "A1", "title": "High ACSC Advisory", "source": "acsc",
            "published": "2024-01-01", "severity": "high",
        })
        self._engine.ingest_advisory({
            "id": "A2", "title": "Medium CISA Advisory", "source": "cisa",
            "published": "2024-01-02", "severity": "medium",
        })
        acsc = self._engine.list_advisories(source="acsc")
        high = self._engine.list_advisories(severity="high")
        self.assertEqual(len(acsc), 1)
        self.assertEqual(acsc[0].id, "A1")
        self.assertEqual(len(high), 1)

    def test_poll_acsc_network_failure_graceful(self):
        with patch("threat_intelligence._async_http_get_json",
                   new=AsyncMock(side_effect=Exception("network error"))):
            result = _run(self._engine.poll_acsc_advisories())
        self.assertEqual(result, [])


# ── Engine: posture adjustment ────────────────────────────────────────────────

class TestPostureAdjustment(unittest.TestCase):
    def setUp(self):
        self._tmp = Path(tempfile.mkdtemp())
        self._engine = _engine(self._tmp)
        _run(self._engine.start())

    def tearDown(self):
        _run(self._engine.stop())

    def test_critical_advisory_triggers_posture_change(self):
        adv = Advisory(
            id="RANSOM-001",
            title="Active Ransomware Campaign",
            source="acsc",
            published="2024-01-01",
            severity="critical",
            first_seen=time.time(),
        )
        _run(self._engine._evaluate_posture_adjustment(adv))
        changes = self._engine.list_posture_changes()
        self.assertGreater(len(changes), 0)
        change_types = [c.change_type for c in changes]
        self.assertIn("monitoring_elevated", change_types)

    def test_ransomware_advisory_triggers_firewall_rule(self):
        adv = Advisory(
            id="RANSOM-002",
            title="Critical SMB Ransomware Lateral Movement",
            source="acsc",
            published="2024-01-01",
            severity="critical",
            first_seen=time.time(),
        )
        _run(self._engine._evaluate_posture_adjustment(adv))
        changes = self._engine.list_posture_changes()
        change_types = [c.change_type for c in changes]
        self.assertIn("firewall_rule", change_types)

    def test_low_severity_no_posture_change(self):
        adv = Advisory(
            id="INFO-001",
            title="Informational Advisory",
            source="acsc",
            published="2024-01-01",
            severity="info",
            first_seen=time.time(),
        )
        _run(self._engine._evaluate_posture_adjustment(adv))
        changes = self._engine.list_posture_changes()
        self.assertEqual(len(changes), 0)


# ── Engine: ATT&CK coverage ───────────────────────────────────────────────────

class TestATTACKCoverage(unittest.TestCase):
    def setUp(self):
        self._tmp = Path(tempfile.mkdtemp())
        self._engine = _engine(self._tmp)

    def test_no_controls_zero_coverage(self):
        result = self._engine.compute_attack_coverage([])
        self.assertEqual(result["covered_techniques"], 0)
        self.assertEqual(result["coverage_pct"], 0.0)
        self.assertEqual(result["total_techniques"], len(ATTACK_COVERAGE))

    def test_mfa_covers_brute_force(self):
        result = self._engine.compute_attack_coverage(["mfa_enforced"])
        covered = result["covered"]
        self.assertIn("T1110", covered)  # Brute Force
        self.assertIn("T1078", covered)  # Valid Accounts

    def test_full_control_set_high_coverage(self):
        all_controls = list({
            c for controls in ATTACK_COVERAGE.values() for c in controls
        })
        result = self._engine.compute_attack_coverage(all_controls)
        self.assertEqual(result["coverage_pct"], 100.0)
        self.assertEqual(len(result["gaps"]), 0)

    def test_gaps_list_missing_controls(self):
        result = self._engine.compute_attack_coverage([])
        gaps = result["gaps"]
        self.assertGreater(len(gaps), 0)
        for gap in gaps:
            self.assertIn("technique_id", gap)
            self.assertIn("required_controls", gap)
            self.assertIn("missing_controls", gap)


# ── Engine: credential exposure ───────────────────────────────────────────────

class TestCredentialExposureCheck(unittest.TestCase):
    def setUp(self):
        self._tmp = Path(tempfile.mkdtemp())
        self._engine = _engine(self._tmp)
        _run(self._engine.start())

    def tearDown(self):
        _run(self._engine.stop())

    def test_no_api_key_returns_empty(self):
        self._engine._config.monitored_domains = ["company.com"]
        # No hibp_api_key set
        result = _run(self._engine.check_credential_exposure())
        self.assertEqual(result, [])

    def test_with_api_key_processes_breaches(self):
        self._engine._config.monitored_domains = ["company.com"]
        self._engine._config.hibp_api_key = "test-key-123"
        fake_breaches = [
            {
                "Name": "TestBreach",
                "Title": "Test Breach",
                "Domain": "company.com",
                "BreachDate": "2024-01-01",
                "DataClasses": ["Passwords", "Email addresses"],
                "IsVerified": True,
            }
        ]
        with patch("threat_intelligence._async_http_get_json",
                   new=AsyncMock(return_value=fake_breaches)):
            new_exp = _run(self._engine.check_credential_exposure())
        self.assertEqual(len(new_exp), 1)
        self.assertEqual(new_exp[0].breach_name, "TestBreach")
        self.assertIn("Passwords", new_exp[0].data_classes)

    def test_exposure_deduplication(self):
        self._engine._config.monitored_domains = ["company.com"]
        self._engine._config.hibp_api_key = "test-key"
        fake_breaches = [{"Name": "BreachA", "Domain": "company.com",
                           "BreachDate": "2024-01-01", "DataClasses": ["Passwords"]}]
        with patch("threat_intelligence._async_http_get_json",
                   new=AsyncMock(return_value=fake_breaches)):
            r1 = _run(self._engine.check_credential_exposure())
            r2 = _run(self._engine.check_credential_exposure())
        self.assertEqual(len(r1), 1)
        self.assertEqual(len(r2), 0)  # Already recorded

    def test_resolve_exposure(self):
        self._engine._config.monitored_domains = ["co.com"]
        self._engine._config.hibp_api_key = "test-key"
        fake_breaches = [{"Name": "BreachB", "Domain": "co.com",
                           "BreachDate": "2024-01-01", "DataClasses": ["Passwords"]}]
        with patch("threat_intelligence._async_http_get_json",
                   new=AsyncMock(return_value=fake_breaches)):
            new_exp = _run(self._engine.check_credential_exposure())
        self.assertEqual(len(new_exp), 1)
        exp_id = new_exp[0].id
        resolved = self._engine.resolve_exposure(exp_id)
        self.assertIsNotNone(resolved)
        self.assertTrue(resolved.resolved)

    def test_resolve_nonexistent(self):
        self.assertIsNone(self._engine.resolve_exposure("nonexistent"))


# ── Engine: threat briefing ───────────────────────────────────────────────────

class TestThreatBriefing(unittest.TestCase):
    def setUp(self):
        self._tmp = Path(tempfile.mkdtemp())
        self._engine = _engine(self._tmp)

    def test_briefing_structure(self):
        b = self._engine.generate_threat_briefing([])
        self.assertIn("generated_at", b)
        self.assertIn("advisories_this_week", b)
        self.assertIn("kev_matches_in_estate", b)
        self.assertIn("attack_coverage_pct", b)
        self.assertIn("open_exposures", b)

    def test_briefing_with_recent_advisory(self):
        self._engine.ingest_advisory({
            "id": "RECENT-001", "title": "Critical Advisory",
            "source": "acsc", "published": "2024-01-01",
            "severity": "critical",
        })
        b = self._engine.generate_threat_briefing([])
        self.assertEqual(b["advisories_this_week"], 1)
        self.assertEqual(b["critical_advisories"], 1)


# ── Persistence ───────────────────────────────────────────────────────────────

class TestPersistence(unittest.TestCase):
    def test_kev_persists(self):
        tmp = Path(tempfile.mkdtemp())
        eng1 = ThreatIntelligenceEngine(tmp)
        _run(eng1.start())
        fake = {
            "vulnerabilities": [
                {"cveID": "CVE-2024-9999", "vendorProject": "V", "product": "P",
                 "vulnerabilityName": "Test", "dateAdded": "2024-01-01",
                 "shortDescription": "Desc", "requiredAction": "Patch",
                 "dueDate": "2024-02-01"}
            ]
        }
        with patch("threat_intelligence._async_http_get_json",
                   new=AsyncMock(return_value=fake)):
            _run(eng1.poll_cisa_kev())
        _run(eng1.stop())

        eng2 = ThreatIntelligenceEngine(tmp)
        _run(eng2.start())
        kev = eng2.list_kev()
        _run(eng2.stop())
        self.assertEqual(len(kev), 1)
        self.assertEqual(kev[0].cve_id, "CVE-2024-9999")

    def test_advisory_persists(self):
        tmp = Path(tempfile.mkdtemp())
        eng1 = ThreatIntelligenceEngine(tmp)
        _run(eng1.start())
        eng1.ingest_advisory({"id": "PERSIST-001", "title": "Test",
                               "source": "manual", "published": "2024-01-01"})
        _run(eng1.stop())

        eng2 = ThreatIntelligenceEngine(tmp)
        _run(eng2.start())
        advisories = eng2.list_advisories()
        _run(eng2.stop())
        self.assertEqual(len(advisories), 1)
        self.assertEqual(advisories[0].id, "PERSIST-001")

    def test_config_persists(self):
        tmp = Path(tempfile.mkdtemp())
        eng1 = ThreatIntelligenceEngine(tmp)
        _run(eng1.start())
        eng1.set_config({"sector": "financial", "kev_poll_interval": 3600})
        _run(eng1.stop())

        eng2 = ThreatIntelligenceEngine(tmp)
        _run(eng2.start())
        cfg = eng2.get_config()
        _run(eng2.stop())
        self.assertEqual(cfg.sector, "financial")
        self.assertEqual(cfg.kev_poll_interval, 3600)


# ── Status ────────────────────────────────────────────────────────────────────

class TestStatus(unittest.TestCase):
    def setUp(self):
        self._tmp = Path(tempfile.mkdtemp())
        self._engine = _engine(self._tmp)

    def test_status_structure(self):
        s = self._engine.status()
        self.assertIn("kev_entries", s)
        self.assertIn("advisories", s)
        self.assertIn("open_exposures", s)
        self.assertIn("monitored_domains", s)
        self.assertIn("enabled", s)

    def test_status_counts(self):
        self._engine.ingest_advisory({
            "id": "S1", "title": "T", "source": "acsc", "published": "2024-01-01"
        })
        s = self._engine.status()
        self.assertEqual(s["advisories"], 1)
        self.assertEqual(s["unacknowledged_advisories"], 1)


# ── Lifecycle ─────────────────────────────────────────────────────────────────

class TestLifecycle(unittest.TestCase):
    def test_start_stop(self):
        tmp = Path(tempfile.mkdtemp())
        eng = ThreatIntelligenceEngine(tmp)
        _run(eng.start())
        self.assertIsNotNone(eng._task)
        _run(eng.stop())
        self.assertTrue(eng._task.done())

    def test_stop_without_start(self):
        tmp = Path(tempfile.mkdtemp())
        eng = ThreatIntelligenceEngine(tmp)
        _run(eng.stop())  # Should not raise


if __name__ == "__main__":
    unittest.main()
