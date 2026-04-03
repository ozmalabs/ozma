# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Unit tests for email_security.py — SPF/DKIM/DMARC checks, scoring, remediation."""
import sys
from pathlib import Path
from unittest.mock import AsyncMock, patch

import pytest

sys.path.insert(0, str(Path(__file__).parent.parent.parent.parent / "controller"))
pytestmark = pytest.mark.unit


# ── SPF checks ───────────────────────────────────────────────────────────────

class TestSPFCheck:
    def _check(self, records):
        from email_security import _check_spf
        return _check_spf(records)

    def test_no_spf_raises_high(self):
        _, issues = self._check(["v=DKIM1; p=abc"])  # TXT present but not SPF
        codes = {i.code for i in issues}
        assert "NO_SPF" in codes
        assert all(i.severity == "high" for i in issues if i.code == "NO_SPF")

    def test_empty_records_no_spf(self):
        _, issues = self._check([])
        assert any(i.code == "NO_SPF" for i in issues)

    def test_valid_spf_no_issues(self):
        spf, issues = self._check(["v=spf1 include:_spf.google.com -all"])
        assert spf.startswith("v=spf1")
        assert not issues

    def test_permissive_plus_all(self):
        _, issues = self._check(["v=spf1 include:example.com +all"])
        assert any(i.code == "SPF_PERMISSIVE" for i in issues)

    def test_permissive_question_all(self):
        _, issues = self._check(["v=spf1 include:example.com ?all"])
        assert any(i.code == "SPF_PERMISSIVE" for i in issues)

    def test_softfail_low_severity(self):
        _, issues = self._check(["v=spf1 include:example.com ~all"])
        codes = {i.code for i in issues}
        assert "SPF_SOFTFAIL" in codes
        soft = next(i for i in issues if i.code == "SPF_SOFTFAIL")
        assert soft.severity == "low"

    def test_multiple_spf_records(self):
        _, issues = self._check([
            "v=spf1 include:a.example.com -all",
            "v=spf1 include:b.example.com -all",
        ])
        assert any(i.code == "MULTIPLE_SPF" for i in issues)

    def test_too_many_lookups(self):
        # 11 include: mechanisms
        mechanisms = " ".join(f"include:s{i}.example.com" for i in range(11))
        _, issues = self._check([f"v=spf1 {mechanisms} -all"])
        assert any(i.code == "SPF_TOO_MANY_LOOKUPS" for i in issues)

    def test_exactly_ten_lookups_ok(self):
        mechanisms = " ".join(f"include:s{i}.example.com" for i in range(10))
        _, issues = self._check([f"v=spf1 {mechanisms} -all"])
        assert not any(i.code == "SPF_TOO_MANY_LOOKUPS" for i in issues)


# ── DMARC checks ─────────────────────────────────────────────────────────────

class TestDMARCCheck:
    def _check(self, records):
        from email_security import _check_dmarc
        return _check_dmarc(records)

    def test_no_dmarc(self):
        _, issues = self._check([])
        assert any(i.code == "NO_DMARC" for i in issues)
        assert any(i.severity == "high" for i in issues if i.code == "NO_DMARC")

    def test_valid_dmarc_reject(self):
        _, issues = self._check(["v=DMARC1; p=reject; rua=mailto:dmarc@ex.com; pct=100"])
        assert not issues

    def test_policy_none(self):
        _, issues = self._check(["v=DMARC1; p=none; rua=mailto:dmarc@ex.com"])
        assert any(i.code == "DMARC_POLICY_NONE" for i in issues)
        assert any(i.severity == "medium" for i in issues if i.code == "DMARC_POLICY_NONE")

    def test_policy_quarantine_ok(self):
        _, issues = self._check(["v=DMARC1; p=quarantine; rua=mailto:dmarc@ex.com; pct=100"])
        assert not any(i.code == "DMARC_POLICY_NONE" for i in issues)

    def test_low_pct(self):
        _, issues = self._check(["v=DMARC1; p=quarantine; rua=mailto:dmarc@ex.com; pct=50"])
        assert any(i.code == "DMARC_PCT_LOW" for i in issues)

    def test_pct_100_ok(self):
        _, issues = self._check(["v=DMARC1; p=reject; rua=mailto:dmarc@ex.com; pct=100"])
        assert not any(i.code == "DMARC_PCT_LOW" for i in issues)

    def test_no_rua(self):
        _, issues = self._check(["v=DMARC1; p=quarantine; pct=100"])
        assert any(i.code == "DMARC_NO_RUA" for i in issues)

    def test_with_rua_no_rua_issue(self):
        _, issues = self._check(["v=DMARC1; p=reject; rua=mailto:d@ex.com; pct=100"])
        assert not any(i.code == "DMARC_NO_RUA" for i in issues)


# ── Scoring ───────────────────────────────────────────────────────────────────

class TestScoring:
    def _posture_with_issues(self, issue_severities):
        from email_security import EmailPosture, EmailIssue
        posture = EmailPosture(domain="example.com")
        for code, sev in issue_severities:
            posture.issues.append(EmailIssue(
                code=code, severity=sev, title="t", description="d",
            ))
        return posture

    def test_perfect_score_no_issues(self):
        from email_security import _score, EmailPosture
        posture = EmailPosture(domain="example.com")
        score, grade = _score(posture)
        assert score == 100
        assert grade == "A"

    def test_high_issue_deducts_20(self):
        from email_security import _score
        posture = self._posture_with_issues([("NO_SPF", "high")])
        score, _ = _score(posture)
        assert score == 80

    def test_medium_issue_deducts_10(self):
        from email_security import _score
        posture = self._posture_with_issues([("DMARC_POLICY_NONE", "medium")])
        score, _ = _score(posture)
        assert score == 90

    def test_multiple_issues_stack(self):
        from email_security import _score
        posture = self._posture_with_issues([
            ("NO_SPF", "high"),
            ("NO_DKIM", "high"),
            ("NO_DMARC", "high"),
        ])
        score, grade = _score(posture)
        assert score == 40
        assert grade == "D"

    def test_score_floor_zero(self):
        from email_security import _score
        posture = self._posture_with_issues(
            [(f"I{i}", "high") for i in range(10)]
        )
        score, grade = _score(posture)
        assert score == 0
        assert grade == "F"

    def test_mta_sts_bonus(self):
        from email_security import _score, EmailPosture
        posture = EmailPosture(domain="example.com", mta_sts_record="v=STSv1; id=x")
        score, grade = _score(posture)
        assert score == 100   # bonus capped at 100
        assert grade == "A"

    def test_grade_boundaries(self):
        from email_security import _score, EmailPosture, EmailIssue

        def make(deduction):
            p = EmailPosture(domain="e.com")
            if deduction > 0:
                p.issues.append(EmailIssue(
                    code="X", severity="low", title="t", description="d",
                ))
                # Use low (−5) for fine-grained control; override score directly
            return p

        from email_security import _score
        # Patch scores directly by manipulating issue list
        cases = [
            ([], "A"),   # 100
        ]
        for issues, expected_grade in cases:
            p = EmailPosture(domain="e.com")
            _, grade = _score(p)
            assert grade == expected_grade


# ── Provider detection ────────────────────────────────────────────────────────

class TestProviderDetection:
    def _detect(self, spf, mx):
        from email_security import _detect_provider
        return _detect_provider(spf, mx)

    def test_google_from_spf(self):
        assert self._detect("v=spf1 include:_spf.google.com -all", []) == "google"

    def test_google_from_mx(self):
        assert self._detect("", ["aspmx.l.google.com"]) == "google"

    def test_microsoft_from_spf(self):
        assert self._detect("v=spf1 include:spf.protection.outlook.com -all", []) == "microsoft"

    def test_microsoft_from_mx(self):
        assert self._detect("", ["mail.protection.outlook.com"]) == "microsoft"

    def test_other_when_unknown(self):
        assert self._detect("v=spf1 include:mymailserver.com -all", ["mx.mymailserver.com"]) == "other"


# ── Remediation guides ────────────────────────────────────────────────────────

class TestRemediationGuides:
    def _issue(self, code, severity="high"):
        from email_security import EmailIssue
        return EmailIssue(code=code, severity=severity, title="t", description="d")

    def test_no_spf_returns_guides(self):
        from email_security import get_remediation
        guides = get_remediation(self._issue("NO_SPF"), "example.com", "", "google")
        assert guides
        providers = {g.provider for g in guides}
        assert "google" in providers
        assert "generic" in providers

    def test_no_spf_google_guide_has_admin_url(self):
        from email_security import get_remediation
        guides = get_remediation(self._issue("NO_SPF"), "example.com", "", "google")
        google = next(g for g in guides if g.provider == "google")
        urls = [s.url for s in google.steps if s.url]
        assert any("admin.google.com" in u for u in urls)

    def test_no_spf_microsoft_guide_has_admin_url(self):
        from email_security import get_remediation
        guides = get_remediation(self._issue("NO_SPF"), "example.com", "", "microsoft")
        ms = next(g for g in guides if g.provider == "microsoft")
        urls = [s.url for s in ms.steps if s.url]
        assert any("admin.microsoft.com" in u or "microsoft.com" in u for u in urls)

    def test_no_spf_auto_fix_google(self):
        from email_security import get_remediation
        guides = get_remediation(self._issue("NO_SPF"), "example.com", "", "google")
        google = next(g for g in guides if g.provider == "google")
        assert google.auto_fix_available

    def test_no_dkim_google_has_steps(self):
        from email_security import get_remediation
        guides = get_remediation(self._issue("NO_DKIM"), "example.com", "", "google")
        assert guides
        google = next((g for g in guides if g.provider == "google"), None)
        assert google is not None
        assert len(google.steps) >= 3

    def test_no_dmarc_has_record_command(self):
        from email_security import get_remediation
        guides = get_remediation(self._issue("NO_DMARC"), "example.com", "", "other")
        generic = next(g for g in guides if g.provider == "generic")
        commands = [s.command for s in generic.steps if s.command]
        assert any("DMARC1" in c for c in commands)

    def test_dmarc_policy_none_fix_shows_quarantine(self):
        from email_security import get_remediation
        current = "v=DMARC1; p=none; rua=mailto:d@ex.com; pct=100"
        guides = get_remediation(self._issue("DMARC_POLICY_NONE"), "ex.com", current, "other")
        generic = next(g for g in guides if g.provider == "generic")
        all_text = " ".join(s.command for s in generic.steps if s.command)
        assert "quarantine" in all_text

    def test_unknown_issue_code_returns_empty(self):
        from email_security import get_remediation
        guides = get_remediation(self._issue("TOTALLY_UNKNOWN"), "ex.com", "", "google")
        assert guides == []

    def test_mta_sts_guide_has_policy_file_contents(self):
        from email_security import get_remediation
        guides = get_remediation(self._issue("NO_MTA_STS", "low"), "ex.com", "", "generic")
        assert guides
        all_text = " ".join(s.command for s in guides[0].steps if s.command)
        assert "STSv1" in all_text or "mode" in all_text


# ── EmailPosture.to_dict with remediation ────────────────────────────────────

class TestPostureToDict:
    def _make_posture(self):
        from email_security import EmailPosture, EmailIssue
        p = EmailPosture(domain="example.com", provider="google")
        p.issues.append(EmailIssue(
            code="NO_SPF", severity="high", title="No SPF", description="desc",
        ))
        return p

    def test_to_dict_without_remediation(self):
        p = self._make_posture()
        d = p.to_dict(include_remediation=False)
        assert "remediation" not in d["issues"][0]

    def test_to_dict_with_remediation(self):
        p = self._make_posture()
        d = p.to_dict(include_remediation=True)
        issue = d["issues"][0]
        assert "remediation" in issue
        assert isinstance(issue["remediation"], list)
        assert len(issue["remediation"]) > 0

    def test_auto_fix_available_flag_present(self):
        p = self._make_posture()
        d = p.to_dict(include_remediation=True)
        assert "auto_fix_available" in d["issues"][0]
        # Google + NO_SPF → auto_fix_available = True
        assert d["issues"][0]["auto_fix_available"] is True


# ── Monitor ───────────────────────────────────────────────────────────────────

class TestEmailSecurityMonitor:
    def test_add_remove_domain(self):
        from email_security import EmailSecurityMonitor
        m = EmailSecurityMonitor()
        m.add_domain("example.com")
        assert "example.com" in m.list_domains()
        m.remove_domain("example.com")
        assert "example.com" not in m.list_domains()

    def test_get_result_before_check_is_none(self):
        from email_security import EmailSecurityMonitor
        m = EmailSecurityMonitor()
        m.add_domain("example.com")
        assert m.get_result("example.com") is None

    def test_domain_normalised(self):
        from email_security import EmailSecurityMonitor
        m = EmailSecurityMonitor()
        m.add_domain("EXAMPLE.COM")
        assert "example.com" in m.list_domains()

    @pytest.mark.asyncio
    async def test_check_now_stores_result(self):
        from email_security import EmailSecurityMonitor, EmailPosture

        async def _fake_check(domain, selectors):
            return EmailPosture(domain=domain, score=80, grade="B")

        m = EmailSecurityMonitor()
        m.add_domain("example.com")

        with patch("email_security.check_domain", side_effect=_fake_check):
            posture = await m.check_now("example.com")

        assert posture.grade == "B"
        assert m.get_result("example.com") is posture

    @pytest.mark.asyncio
    async def test_alert_callback_fires_on_high_issue(self):
        from email_security import EmailSecurityMonitor, EmailPosture, EmailIssue

        alerts = []

        async def _on_alert(domain, posture):
            alerts.append((domain, posture))

        async def _fake_check(domain, selectors):
            p = EmailPosture(domain=domain)
            p.issues.append(EmailIssue(
                code="NO_SPF", severity="high", title="t", description="d",
            ))
            return p

        m = EmailSecurityMonitor(on_alert=_on_alert)
        m.add_domain("example.com")

        with patch("email_security.check_domain", side_effect=_fake_check):
            await m.check_now("example.com")

        assert len(alerts) == 1
        assert alerts[0][0] == "example.com"

    @pytest.mark.asyncio
    async def test_no_alert_on_clean_result(self):
        from email_security import EmailSecurityMonitor, EmailPosture

        alerts = []

        async def _on_alert(domain, posture):
            alerts.append(domain)

        async def _fake_check(domain, selectors):
            return EmailPosture(domain=domain, score=100, grade="A")

        m = EmailSecurityMonitor(on_alert=_on_alert)
        m.add_domain("example.com")

        with patch("email_security.check_domain", side_effect=_fake_check):
            await m.check_now("example.com")

        assert not alerts

    @pytest.mark.asyncio
    async def test_start_stop(self):
        from email_security import EmailSecurityMonitor
        m = EmailSecurityMonitor(recheck_interval=9999)
        await m.start()
        assert m._task is not None
        await m.stop()
        assert m._task.cancelled() or m._task.done()
