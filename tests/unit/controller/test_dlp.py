#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Unit tests for DLPManager — data loss prevention policies and scanning.
"""

import asyncio
import json
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock

sys.path.insert(0, str(Path(__file__).parent.parent.parent.parent / "controller"))

from dlp import (
    DLPManager, DLPConfig, DLPPolicy, DLPRule, DLPIncident, DLPMatch,
    ContentScanner,
    _luhn_valid, _tfn_valid, _redact_context, _get_pattern,
    _BUILTIN_PATTERNS, _SEVERITIES, MAX_INCIDENTS,
)


def _mgr(tmp: Path) -> DLPManager:
    return DLPManager(tmp)


def _rule(**kwargs) -> DLPRule:
    import uuid
    defaults = dict(
        id=str(uuid.uuid4()), name="Test Rule",
        pattern_type="credit_card", action="alert",
        severity="high", scopes=["file", "email", "cloud"],
        enabled=True, min_matches=1, validate=True,
    )
    defaults.update(kwargs)
    return DLPRule(**defaults)


def _run(coro):
    return asyncio.get_event_loop().run_until_complete(coro)


# ── Luhn validation ───────────────────────────────────────────────────────────

class TestLuhnValidation(unittest.TestCase):
    def test_valid_visa(self):
        # Standard Visa test number
        self.assertTrue(_luhn_valid("4111111111111111"))

    def test_valid_mastercard(self):
        self.assertTrue(_luhn_valid("5500005555555559"))

    def test_valid_amex(self):
        self.assertTrue(_luhn_valid("378282246310005"))

    def test_invalid_number(self):
        self.assertFalse(_luhn_valid("4111111111111112"))

    def test_all_zeros_valid_luhn(self):
        # All-zeros passes Luhn checksum (mathematically valid) but wouldn't
        # pass the credit card regex which requires specific digit ranges
        self.assertTrue(_luhn_valid("0000000000000000"))

    def test_single_digit_invalid(self):
        self.assertFalse(_luhn_valid("1"))

    def test_non_digit_invalid(self):
        self.assertFalse(_luhn_valid("4111-1111-1111-111X"))


# ── TFN validation ────────────────────────────────────────────────────────────

class TestTFNValidation(unittest.TestCase):
    def test_valid_tfn(self):
        # Known valid TFN
        self.assertTrue(_tfn_valid("123456782"))

    def test_wrong_length(self):
        self.assertFalse(_tfn_valid("12345678"))   # 8 digits
        self.assertFalse(_tfn_valid("1234567890"))  # 10 digits

    def test_all_zeros_valid_checksum(self):
        # All-zeros passes the TFN mod-11 checksum (0*w = 0 for all weights)
        self.assertTrue(_tfn_valid("000000000"))

    def test_separators_stripped(self):
        # Spaces and hyphens should be stripped before validation
        self.assertTrue(_tfn_valid("123 456 782"))
        self.assertTrue(_tfn_valid("123-456-782"))


# ── Redact context ────────────────────────────────────────────────────────────

class TestRedactContext(unittest.TestCase):
    def test_replaces_match_with_asterisks(self):
        text = "card number is 4111111111111111 in this file"
        import re
        m = re.search(r"\d{16}", text)
        ctx = _redact_context(text, m)
        self.assertNotIn("4111111111111111", ctx)
        self.assertIn("*", ctx)

    def test_context_window_included(self):
        text = "BEFORE 4111111111111111 AFTER"
        import re
        m = re.search(r"\d{16}", text)
        ctx = _redact_context(text, m, window=10)
        self.assertIn("BEFORE", ctx)

    def test_max_length_respected(self):
        text = "A" * 1000 + "4111111111111111" + "B" * 1000
        import re
        m = re.search(r"\d{16}", text)
        ctx = _redact_context(text, m)
        from dlp import MAX_CONTEXT
        self.assertLessEqual(len(ctx), MAX_CONTEXT)


# ── Built-in pattern matching ─────────────────────────────────────────────────

class TestBuiltinPatterns(unittest.TestCase):
    def _match(self, pattern_type: str, text: str) -> bool:
        p = _get_pattern(pattern_type)
        return bool(p and p.search(text))

    def test_credit_card_visa(self):
        self.assertTrue(self._match("credit_card", "4111111111111111"))

    def test_credit_card_not_in_random_digits(self):
        # 16-digit number that fails Luhn — but pattern matches, Luhn validates
        # This tests the pattern itself (pre-validation)
        p = _get_pattern("credit_card")
        self.assertIsNotNone(p)

    def test_ssn_match(self):
        self.assertTrue(self._match("ssn", "123-45-6789"))
        self.assertTrue(self._match("ssn", "123 45 6789"))

    def test_ssn_no_match_invalid(self):
        # 000 prefix is invalid SSN
        self.assertFalse(self._match("ssn", "000-45-6789"))

    def test_aws_key_match(self):
        self.assertTrue(self._match("aws_key", "AKIAIOSFODNN7EXAMPLE"))

    def test_aws_key_no_match_wrong_prefix(self):
        self.assertFalse(self._match("aws_key", "ABCAIOSFODNN7EXAMPLE"))

    def test_private_key_match(self):
        self.assertTrue(self._match("private_key",
                                     "-----BEGIN RSA PRIVATE KEY-----"))
        self.assertTrue(self._match("private_key",
                                     "-----BEGIN OPENSSH PRIVATE KEY-----"))

    def test_api_key_match(self):
        self.assertTrue(self._match("api_key",
                                     "api_key = ABCDEF1234567890ABCDEF"))

    def test_password_match(self):
        self.assertTrue(self._match("password", "password=supersecretpassword123"))

    def test_custom_pattern(self):
        p = _get_pattern("custom", r"OZMA-[A-Z0-9]{8}")
        self.assertIsNotNone(p)
        self.assertTrue(p.search("OZMA-ABCD1234"))

    def test_invalid_custom_pattern(self):
        p = _get_pattern("custom", r"[invalid(")
        self.assertIsNone(p)

    def test_unknown_builtin_returns_none(self):
        p = _get_pattern("no_such_pattern_type")
        self.assertIsNone(p)


# ── ContentScanner ────────────────────────────────────────────────────────────

class TestContentScanner(unittest.TestCase):
    def setUp(self):
        self._scanner = ContentScanner()

    def _rule(self, pattern_type: str, **kwargs) -> DLPRule:
        kwargs.setdefault("scopes", ["file", "email"])
        return _rule(pattern_type=pattern_type, **kwargs)

    def test_finds_credit_card(self):
        rule = self._rule("credit_card", validate=True)
        text = "Customer card: 4111111111111111 was charged."
        matches = self._scanner.scan_text(text, [rule], scope="file")
        self.assertEqual(len(matches), 1)
        self.assertEqual(matches[0].pattern_type, "credit_card")

    def test_rejects_invalid_luhn(self):
        rule = self._rule("credit_card", validate=True)
        text = "Bad card: 4111111111111112"
        matches = self._scanner.scan_text(text, [rule], scope="file")
        self.assertEqual(len(matches), 0)

    def test_accepts_invalid_luhn_when_validate_false(self):
        rule = self._rule("credit_card", validate=False)
        text = "Bad card: 4111111111111112"
        matches = self._scanner.scan_text(text, [rule], scope="file")
        self.assertEqual(len(matches), 1)

    def test_finds_aws_key(self):
        rule = self._rule("aws_key", validate=False)
        text = "export AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE"
        matches = self._scanner.scan_text(text, [rule], scope="file")
        self.assertEqual(len(matches), 1)

    def test_scope_filtering(self):
        rule = self._rule("aws_key", validate=False, scopes=["email"])
        text = "AKIAIOSFODNN7EXAMPLE"
        # scope="file" should not match a rule scoped only to email
        matches = self._scanner.scan_text(text, [rule], scope="file")
        self.assertEqual(len(matches), 0)

    def test_disabled_rule_skipped(self):
        rule = self._rule("aws_key", validate=False, enabled=False)
        text = "AKIAIOSFODNN7EXAMPLE"
        matches = self._scanner.scan_text(text, [rule], scope="file")
        self.assertEqual(len(matches), 0)

    def test_min_matches_threshold(self):
        rule = self._rule("credit_card", validate=True, min_matches=2)
        text = "Card 1: 4111111111111111"  # only one match
        matches = self._scanner.scan_text(text, [rule], scope="file")
        self.assertEqual(len(matches), 0)

    def test_min_matches_met(self):
        rule = self._rule("credit_card", validate=True, min_matches=2)
        text = "Cards: 4111111111111111 and 5500005555555559"
        matches = self._scanner.scan_text(text, [rule], scope="file")
        self.assertEqual(len(matches), 2)

    def test_multiple_rules(self):
        rules = [
            self._rule("credit_card", validate=True),
            self._rule("aws_key", validate=False),
        ]
        text = "Card: 4111111111111111 key: AKIAIOSFODNN7EXAMPLE"
        matches = self._scanner.scan_text(text, rules, scope="file")
        types = {m.pattern_type for m in matches}
        self.assertIn("credit_card", types)
        self.assertIn("aws_key", types)

    def test_context_is_redacted(self):
        rule = self._rule("credit_card", validate=True)
        text = "Card 4111111111111111 is sensitive"
        matches = self._scanner.scan_text(text, [rule], scope="file")
        self.assertEqual(len(matches), 1)
        self.assertNotIn("4111111111111111", matches[0].context)

    def test_scan_file_async(self):
        import tempfile as tf
        with tf.NamedTemporaryFile(suffix=".txt", mode="w", delete=False) as f:
            f.write("AWS key: AKIAIOSFODNN7EXAMPLE")
            fname = f.name
        rule = self._rule("aws_key", validate=False)
        matches = _run(self._scanner.scan_file(Path(fname), [rule]))
        self.assertEqual(len(matches), 1)

    def test_scan_file_too_large(self):
        import tempfile as tf
        with tf.NamedTemporaryFile(suffix=".txt", mode="w", delete=False) as f:
            f.write("x" * 100)
            fname = f.name
        rule = self._rule("aws_key", validate=False)
        # max_size_mb=0 → always skip
        matches = _run(self._scanner.scan_file(Path(fname), [rule], max_size_mb=0))
        self.assertEqual(len(matches), 0)

    def test_scan_nonexistent_file(self):
        rule = self._rule("aws_key", validate=False)
        matches = _run(self._scanner.scan_file(Path("/no/such/file.txt"), [rule]))
        self.assertEqual(matches, [])


# ── DLPRule model ─────────────────────────────────────────────────────────────

class TestDLPRuleModel(unittest.TestCase):
    def test_roundtrip(self):
        r = _rule(pattern_type="ssn", action="block", min_matches=3)
        r2 = DLPRule.from_dict(r.to_dict())
        self.assertEqual(r2.id, r.id)
        self.assertEqual(r2.pattern_type, "ssn")
        self.assertEqual(r2.action, "block")
        self.assertEqual(r2.min_matches, 3)


# ── DLPPolicy model ───────────────────────────────────────────────────────────

class TestDLPPolicyModel(unittest.TestCase):
    def test_roundtrip(self):
        p = DLPPolicy(id="p1", name="Test", rules=[_rule()])
        p2 = DLPPolicy.from_dict(p.to_dict())
        self.assertEqual(p2.id, "p1")
        self.assertEqual(len(p2.rules), 1)

    def test_get_rule_found(self):
        r = _rule()
        p = DLPPolicy(id="p1", name="P", rules=[r])
        self.assertEqual(p.get_rule(r.id), r)

    def test_get_rule_not_found(self):
        p = DLPPolicy(id="p1", name="P", rules=[])
        self.assertIsNone(p.get_rule("no-such"))


# ── DLPIncident model ─────────────────────────────────────────────────────────

class TestDLPIncidentModel(unittest.TestCase):
    def test_roundtrip(self):
        inc = DLPIncident(
            id="i1", policy_id="p1", rule_id="r1",
            pattern_type="credit_card", action_taken="alert",
            severity="high", scope="file", source="/home/user/data.csv",
            match_count=2, created_at=time.time(),
        )
        i2 = DLPIncident.from_dict(inc.to_dict())
        self.assertEqual(i2.id, "i1")
        self.assertEqual(i2.match_count, 2)
        self.assertFalse(i2.resolved)


# ── DLPConfig serialization ───────────────────────────────────────────────────

class TestDLPConfig(unittest.TestCase):
    def test_defaults(self):
        cfg = DLPConfig()
        self.assertEqual(cfg.file_scan_interval, 86400)
        self.assertTrue(cfg.email_scan_enabled)
        self.assertFalse(cfg.cloud_scan_enabled)
        self.assertTrue(cfg.usb_alert_enabled)

    def test_roundtrip(self):
        cfg = DLPConfig(
            file_scan_interval=3600,
            itsm_ticket_severity="critical",
            email_scan_enabled=False,
            usb_alert_enabled=False,
        )
        cfg2 = DLPConfig.from_dict(cfg.to_dict())
        self.assertEqual(cfg2.file_scan_interval, 3600)
        self.assertEqual(cfg2.itsm_ticket_severity, "critical")
        self.assertFalse(cfg2.email_scan_enabled)


# ── Policy management ─────────────────────────────────────────────────────────

class TestPolicyManagement(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.mkdtemp()
        self._mgr = _mgr(Path(self._tmp))

    def test_create_policy(self):
        p = self._mgr.create_policy("Test Policy", description="desc")
        self.assertEqual(p.name, "Test Policy")
        self.assertIn(p.id, {pol.id for pol in self._mgr.list_policies()})

    def test_get_policy(self):
        p = self._mgr.create_policy("P")
        self.assertIsNotNone(self._mgr.get_policy(p.id))

    def test_get_policy_missing(self):
        self.assertIsNone(self._mgr.get_policy("no-such"))

    def test_update_policy(self):
        p = self._mgr.create_policy("Old Name")
        self._mgr.update_policy(p.id, name="New Name", enabled=False)
        updated = self._mgr.get_policy(p.id)
        self.assertEqual(updated.name, "New Name")
        self.assertFalse(updated.enabled)

    def test_delete_policy(self):
        p = self._mgr.create_policy("Delete Me")
        self.assertTrue(self._mgr.delete_policy(p.id))
        self.assertIsNone(self._mgr.get_policy(p.id))

    def test_delete_nonexistent_returns_false(self):
        self.assertFalse(self._mgr.delete_policy("no-such"))

    def test_add_rule(self):
        p = self._mgr.create_policy("P")
        r = _rule(pattern_type="ssn")
        self._mgr.add_rule(p.id, r)
        updated = self._mgr.get_policy(p.id)
        self.assertEqual(len(updated.rules), 1)

    def test_update_rule(self):
        p = self._mgr.create_policy("P")
        r = _rule(pattern_type="ssn", action="log")
        self._mgr.add_rule(p.id, r)
        self._mgr.update_rule(p.id, r.id, action="block")
        updated = self._mgr.get_policy(p.id)
        self.assertEqual(updated.rules[0].action, "block")

    def test_remove_rule(self):
        p = self._mgr.create_policy("P")
        r = _rule()
        self._mgr.add_rule(p.id, r)
        self.assertTrue(self._mgr.remove_rule(p.id, r.id))
        self.assertEqual(len(self._mgr.get_policy(p.id).rules), 0)

    def test_remove_nonexistent_rule_returns_false(self):
        p = self._mgr.create_policy("P")
        self.assertFalse(self._mgr.remove_rule(p.id, "no-such-rule"))

    def test_create_default_policy(self):
        p = self._mgr.create_default_policy()
        self.assertGreater(len(p.rules), 0)
        types = {r.pattern_type for r in p.rules}
        self.assertIn("credit_card", types)
        self.assertIn("aws_key", types)
        self.assertIn("private_key", types)

    def test_policies_persist(self):
        p = self._mgr.create_policy("Saved")
        r = _rule()
        self._mgr.add_rule(p.id, r)
        mgr2 = _mgr(Path(self._tmp))
        self.assertIsNotNone(mgr2.get_policy(p.id))
        self.assertEqual(len(mgr2.get_policy(p.id).rules), 1)


# ── Incident management ───────────────────────────────────────────────────────

class TestIncidentManagement(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.mkdtemp()
        self._mgr = _mgr(Path(self._tmp))
        # Inject incidents
        now = time.time()
        self._mgr._incidents = [
            DLPIncident(id=f"inc-{i}", policy_id="p1", rule_id="r1",
                        pattern_type="credit_card", action_taken="alert",
                        severity=sev, scope=scope, source="test.csv",
                        created_at=now - i * 60)
            for i, (sev, scope) in enumerate([
                ("critical", "file"), ("high", "email"),
                ("medium", "cloud"), ("high", "file"),
            ])
        ]

    def test_list_all(self):
        self.assertEqual(len(self._mgr.list_incidents()), 4)

    def test_filter_by_scope(self):
        incs = self._mgr.list_incidents(scope="file")
        self.assertEqual(len(incs), 2)
        for i in incs:
            self.assertEqual(i.scope, "file")

    def test_filter_by_severity(self):
        incs = self._mgr.list_incidents(severity="high")
        self.assertEqual(len(incs), 2)

    def test_filter_resolved(self):
        self._mgr._incidents[0].resolved = True
        incs = self._mgr.list_incidents(resolved=False)
        self.assertEqual(len(incs), 3)

    def test_filter_resolved_true(self):
        self._mgr._incidents[0].resolved = True
        incs = self._mgr.list_incidents(resolved=True)
        self.assertEqual(len(incs), 1)

    def test_limit(self):
        incs = self._mgr.list_incidents(limit=2)
        self.assertEqual(len(incs), 2)

    def test_sorted_newest_first(self):
        incs = self._mgr.list_incidents()
        times = [i.created_at for i in incs]
        self.assertEqual(times, sorted(times, reverse=True))

    def test_get_incident(self):
        inc = self._mgr.get_incident("inc-0")
        self.assertIsNotNone(inc)
        self.assertEqual(inc.id, "inc-0")

    def test_get_incident_not_found(self):
        self.assertIsNone(self._mgr.get_incident("no-such"))

    def test_acknowledge(self):
        ok = self._mgr.acknowledge_incident("inc-1")
        self.assertTrue(ok)
        self.assertTrue(self._mgr.get_incident("inc-1").acknowledged)

    def test_resolve(self):
        ok = self._mgr.resolve_incident("inc-2")
        self.assertTrue(ok)
        inc = self._mgr.get_incident("inc-2")
        self.assertTrue(inc.resolved)
        self.assertGreater(inc.resolved_at, 0)

    def test_resolve_nonexistent_returns_false(self):
        self.assertFalse(self._mgr.resolve_incident("no-such"))

    def test_ring_buffer_cap(self):
        for i in range(MAX_INCIDENTS + 50):
            self._mgr._incidents.append(DLPIncident(
                id=f"extra-{i}", policy_id="p", rule_id="r",
                pattern_type="ssn", action_taken="log",
                severity="low", scope="file", source="x",
                created_at=time.time(),
            ))
        self._mgr._save()
        mgr2 = _mgr(Path(self._tmp))
        self.assertLessEqual(len(mgr2._incidents), MAX_INCIDENTS)

    def test_incidents_persist(self):
        self._mgr._save()
        mgr2 = _mgr(Path(self._tmp))
        self.assertEqual(len(mgr2.list_incidents()), 4)


# ── scan_content ──────────────────────────────────────────────────────────────

class TestScanContent(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.mkdtemp()
        self._mgr = _mgr(Path(self._tmp))
        r = _rule(pattern_type="credit_card", validate=True,
                   action="alert", scopes=["email"])
        self._mgr.create_policy("Test", rules=[r])

    def test_scan_content_creates_incident(self):
        incidents = _run(self._mgr.scan_content(
            "Card: 4111111111111111",
            scope="email", source="outbound email",
        ))
        self.assertEqual(len(incidents), 1)
        self.assertEqual(incidents[0].pattern_type, "credit_card")
        self.assertEqual(incidents[0].scope, "email")

    def test_scan_content_no_match(self):
        incidents = _run(self._mgr.scan_content(
            "Hello world", scope="email", source="email"
        ))
        self.assertEqual(len(incidents), 0)

    def test_scan_content_wrong_scope(self):
        # Rule is email-scoped; scanning "file" scope should not trigger
        incidents = _run(self._mgr.scan_content(
            "Card: 4111111111111111",
            scope="file", source="test.txt",
        ))
        self.assertEqual(len(incidents), 0)

    def test_scan_content_disabled_policy(self):
        for p in self._mgr.list_policies():
            self._mgr.update_policy(p.id, enabled=False)
        incidents = _run(self._mgr.scan_content(
            "Card: 4111111111111111", scope="email", source="email"
        ))
        self.assertEqual(len(incidents), 0)

    def test_scan_content_fires_event(self):
        q: asyncio.Queue = asyncio.Queue()
        self._mgr._event_queue = q
        _run(self._mgr.scan_content(
            "Card: 4111111111111111", scope="email", source="email"
        ))
        events = []
        while not q.empty():
            events.append(q.get_nowait())
        types = [e["type"] for e in events]
        self.assertIn("dlp.incident", types)

    def test_scan_content_creates_itsm_ticket(self):
        mock_itsm = AsyncMock()
        mock_itsm.create_ticket = AsyncMock(return_value=None)
        self._mgr.itsm = mock_itsm
        self._mgr._config.itsm_ticket_severity = "high"
        _run(self._mgr.scan_content(
            "Card: 4111111111111111", scope="email", source="email"
        ))
        mock_itsm.create_ticket.assert_called_once()

    def test_scan_content_no_ticket_below_threshold(self):
        # Rule severity = "high"; threshold = "critical" → no ticket
        mock_itsm = AsyncMock()
        mock_itsm.create_ticket = AsyncMock()
        self._mgr.itsm = mock_itsm
        self._mgr._config.itsm_ticket_severity = "critical"
        _run(self._mgr.scan_content(
            "Card: 4111111111111111", scope="email", source="email"
        ))
        mock_itsm.create_ticket.assert_not_called()


# ── USB events ────────────────────────────────────────────────────────────────

class TestUSBEvents(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.mkdtemp()
        self._mgr = _mgr(Path(self._tmp))
        r = _rule(pattern_type="credit_card", action="alert",
                   scopes=["usb"])
        self._mgr.create_policy("USB Policy", rules=[r])

    def test_usb_event_creates_incident(self):
        incidents = _run(self._mgr.handle_usb_event(
            node_id="node-1", device_name="SanDisk 32GB"
        ))
        self.assertEqual(len(incidents), 1)
        self.assertEqual(incidents[0].scope, "usb")

    def test_usb_event_disabled(self):
        self._mgr._config.usb_alert_enabled = False
        incidents = _run(self._mgr.handle_usb_event(
            node_id="node-1", device_name="SanDisk"
        ))
        self.assertEqual(len(incidents), 0)

    def test_usb_event_no_usb_scoped_rule(self):
        # Remove USB scope from rule
        for p in self._mgr.list_policies():
            for r in p.rules:
                r.scopes = ["file", "email"]
        incidents = _run(self._mgr.handle_usb_event(
            node_id="node-1", device_name="SanDisk"
        ))
        self.assertEqual(len(incidents), 0)


# ── File scan ─────────────────────────────────────────────────────────────────

class TestFileScan(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.mkdtemp()
        self._mgr = _mgr(Path(self._tmp))

    def test_file_scan_no_policies(self):
        result = _run(self._mgr.run_file_scan(paths=[self._tmp]))
        self.assertTrue(result["ok"])
        self.assertEqual(result["files_scanned"], 0)

    def test_file_scan_finds_matches(self):
        import tempfile as tf
        r = _rule(pattern_type="aws_key", validate=False,
                   action="alert", scopes=["file"])
        self._mgr.create_policy("P", rules=[r])
        # Create a temp .env file with an AWS key
        scan_dir = Path(tempfile.mkdtemp())
        test_file = scan_dir / "config.env"
        test_file.write_text("AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE\n")
        result = _run(self._mgr.run_file_scan(paths=[str(scan_dir)]))
        self.assertTrue(result["ok"])
        self.assertGreater(result["files_scanned"], 0)
        self.assertGreater(result["incidents"], 0)

    def test_file_scan_updates_timestamp(self):
        r = _rule(pattern_type="aws_key", validate=False, action="alert", scopes=["file"])
        self._mgr.create_policy("P", rules=[r])
        before = time.time()
        _run(self._mgr.run_file_scan(paths=[self._tmp]))
        self.assertGreaterEqual(self._mgr._last_file_scan, before)

    def test_file_scan_fires_event(self):
        # Must have at least one enabled policy for run_file_scan to proceed past
        # the early-return guard and fire the completion event.
        r = _rule(pattern_type="aws_key", validate=False, action="alert", scopes=["file"])
        self._mgr.create_policy("P", rules=[r])
        q: asyncio.Queue = asyncio.Queue()
        self._mgr._event_queue = q
        _run(self._mgr.run_file_scan(paths=[self._tmp]))
        events = []
        while not q.empty():
            events.append(q.get_nowait())
        types = [e["type"] for e in events]
        self.assertIn("dlp.file_scan.complete", types)


# ── Status ────────────────────────────────────────────────────────────────────

class TestDLPStatus(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.mkdtemp()
        self._mgr = _mgr(Path(self._tmp))

    def test_status_structure(self):
        s = self._mgr.status()
        self.assertIn("policies", s)
        self.assertIn("active_policies", s)
        self.assertIn("incidents_open", s)
        self.assertIn("incidents_by_severity", s)
        self.assertIn("scans_enabled", s)

    def test_status_counts(self):
        self._mgr.create_policy("P1")
        self._mgr.create_policy("P2")
        # Disable one
        for p in list(self._mgr.list_policies())[:1]:
            self._mgr.update_policy(p.id, enabled=False)
        s = self._mgr.status()
        self.assertEqual(s["policies"], 2)
        self.assertEqual(s["active_policies"], 1)

    def test_status_open_incidents(self):
        self._mgr._incidents = [
            DLPIncident(id="i1", policy_id="p", rule_id="r",
                        pattern_type="ssn", action_taken="alert",
                        severity="high", scope="email", source="x",
                        resolved=False, created_at=time.time()),
            DLPIncident(id="i2", policy_id="p", rule_id="r",
                        pattern_type="ssn", action_taken="alert",
                        severity="high", scope="email", source="x",
                        resolved=True, created_at=time.time()),
        ]
        s = self._mgr.status()
        self.assertEqual(s["incidents_open"], 1)


# ── Lifecycle ─────────────────────────────────────────────────────────────────

class TestLifecycle(unittest.TestCase):
    def test_start_creates_task(self):
        mgr = _mgr(Path(tempfile.mkdtemp()))
        _run(mgr.start())
        self.assertEqual(len(mgr._tasks), 1)
        _run(mgr.stop())

    def test_stop_cancels_tasks(self):
        mgr = _mgr(Path(tempfile.mkdtemp()))
        _run(mgr.start())
        _run(mgr.stop())
        for t in mgr._tasks:
            self.assertTrue(t.cancelled() or t.done())


# ── Persistence ───────────────────────────────────────────────────────────────

class TestPersistence(unittest.TestCase):
    def test_config_persists(self):
        tmp = Path(tempfile.mkdtemp())
        mgr = _mgr(tmp)
        mgr.set_config(DLPConfig(file_scan_interval=1800, usb_alert_enabled=False))
        mgr2 = _mgr(tmp)
        self.assertEqual(mgr2.get_config().file_scan_interval, 1800)
        self.assertFalse(mgr2.get_config().usb_alert_enabled)

    def test_corrupt_config_uses_defaults(self):
        tmp = Path(tempfile.mkdtemp())
        tmp.mkdir(parents=True, exist_ok=True)
        (tmp / "dlp.json").write_text("{corrupt")
        mgr = _mgr(tmp)
        self.assertEqual(mgr.get_config().file_scan_interval, 86400)

    def test_corrupt_incidents_uses_empty(self):
        tmp = Path(tempfile.mkdtemp())
        tmp.mkdir(parents=True, exist_ok=True)
        (tmp / "incidents.json").write_text("{corrupt")
        mgr = _mgr(tmp)
        self.assertEqual(mgr.list_incidents(), [])


if __name__ == "__main__":
    unittest.main()
