# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Unit tests for itsm.py — ITSM ticket lifecycle, agent escalation, on-call routing."""
import asyncio
import sys
import time
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

sys.path.insert(0, str(Path(__file__).parent.parent.parent.parent / "controller"))
pytestmark = pytest.mark.unit


# ── Fixtures ──────────────────────────────────────────────────────────────────

@pytest.fixture
def tmp_data(tmp_path):
    return tmp_path / "itsm"


@pytest.fixture
def events():
    return asyncio.Queue()


@pytest.fixture
def mgr(tmp_data, events):
    from itsm import ITSMManager
    return ITSMManager(tmp_data, event_queue=events)


@pytest.fixture
def notifier():
    n = MagicMock()
    n.on_event = AsyncMock()
    n._destinations = {}
    n._send = AsyncMock()
    return n


@pytest.fixture
def mgr_with_notifier(tmp_data, events, notifier):
    from itsm import ITSMManager
    return ITSMManager(tmp_data, notifier=notifier, event_queue=events)


async def drain_events(q: asyncio.Queue) -> list[dict]:
    out = []
    while not q.empty():
        out.append(q.get_nowait())
    return out


# ── TestTicketIDGeneration ─────────────────────────────────────────────────────

class TestTicketIDGeneration:
    @pytest.mark.asyncio
    async def test_first_ticket_id_format(self, mgr):
        t = await mgr.create_ticket("user", "incident", "medium", "Test", "desc")
        year = time.strftime("%Y")
        assert t.id == f"TKT-{year}-0001"

    @pytest.mark.asyncio
    async def test_sequential_ids(self, mgr):
        t1 = await mgr.create_ticket("user", "incident", "medium", "A", "")
        t2 = await mgr.create_ticket("user", "incident", "medium", "B", "")
        year = time.strftime("%Y")
        assert t1.id == f"TKT-{year}-0001"
        assert t2.id == f"TKT-{year}-0002"


# ── TestTicketCreation ────────────────────────────────────────────────────────

class TestTicketCreation:
    @pytest.mark.asyncio
    async def test_ticket_stored_in_memory(self, mgr):
        t = await mgr.create_ticket("agent", "hardware", "high", "Disk failing", "SMART errors")
        assert mgr.get_ticket(t.id) is t

    @pytest.mark.asyncio
    async def test_sla_deadlines_set(self, mgr):
        from itsm import SLA_RESPONSE, SLA_RESOLUTION
        t = await mgr.create_ticket("user", "incident", "critical", "Down", "")
        now = time.time()
        assert t.sla_response_deadline is not None
        assert abs(t.sla_response_deadline - (now + SLA_RESPONSE["critical"])) < 2
        assert abs(t.sla_resolution_deadline - (now + SLA_RESOLUTION["critical"])) < 2

    @pytest.mark.asyncio
    async def test_status_set_to_l1_triage(self, mgr):
        t = await mgr.create_ticket("user", "incident", "medium", "Issue", "")
        assert t.status == "l1_triage"

    @pytest.mark.asyncio
    async def test_agent_tier_set_to_l1(self, mgr):
        from itsm import AGENT_L1
        t = await mgr.create_ticket("user", "incident", "medium", "Issue", "")
        assert t.agent_tier == AGENT_L1

    @pytest.mark.asyncio
    async def test_created_event_fired(self, mgr, events):
        await mgr.create_ticket("user", "incident", "medium", "Issue", "")
        evts = await drain_events(events)
        types = [e["type"] for e in evts]
        assert "itsm.ticket.created" in types

    @pytest.mark.asyncio
    async def test_triage_event_fired(self, mgr, events):
        await mgr.create_ticket("user", "incident", "medium", "Issue", "")
        evts = await drain_events(events)
        types = [e["type"] for e in evts]
        assert "itsm.ticket.triage" in types

    @pytest.mark.asyncio
    async def test_triage_event_has_model_config(self, mgr, events):
        await mgr.create_ticket("user", "incident", "medium", "Issue", "")
        evts = await drain_events(events)
        triage = next(e for e in evts if e["type"] == "itsm.ticket.triage")
        mc = triage["model_config"]
        assert "provider" in mc
        assert "model" in mc
        assert mc["provider"] == "anthropic"
        assert "haiku" in mc["model"].lower()

    @pytest.mark.asyncio
    async def test_audit_trail_has_created_entry(self, mgr):
        t = await mgr.create_ticket("alert", "security", "high", "Breach", "")
        assert any(a.action == "created" for a in t.audit)

    @pytest.mark.asyncio
    async def test_invalid_priority_defaults_to_medium(self, mgr):
        t = await mgr.create_ticket("user", "incident", "banana", "Test", "")
        assert t.priority == "medium"

    @pytest.mark.asyncio
    async def test_ticket_persisted_to_disk(self, mgr, tmp_data):
        await mgr.create_ticket("user", "incident", "medium", "Saved", "")
        assert (tmp_data / "tickets.json").exists()


# ── TestTicketResolution ──────────────────────────────────────────────────────

class TestTicketResolution:
    @pytest.mark.asyncio
    async def test_resolve_sets_status(self, mgr):
        t = await mgr.create_ticket("user", "incident", "medium", "Issue", "")
        ok = await mgr.resolve_ticket(t.id, "agent_l1", "Restarted service")
        assert ok
        assert t.status == "resolved"

    @pytest.mark.asyncio
    async def test_resolve_sets_resolution_text(self, mgr):
        t = await mgr.create_ticket("user", "incident", "medium", "Issue", "")
        await mgr.resolve_ticket(t.id, "agent_l1", "Fixed it")
        assert t.resolution == "Fixed it"

    @pytest.mark.asyncio
    async def test_resolve_sets_resolved_at(self, mgr):
        t = await mgr.create_ticket("user", "incident", "medium", "Issue", "")
        before = time.time()
        await mgr.resolve_ticket(t.id, "agent_l1", "Fixed")
        assert t.resolved_at is not None
        assert t.resolved_at >= before

    @pytest.mark.asyncio
    async def test_resolve_fires_event(self, mgr, events):
        t = await mgr.create_ticket("user", "incident", "medium", "Issue", "")
        await drain_events(events)
        await mgr.resolve_ticket(t.id, "agent_l1", "Fixed")
        evts = await drain_events(events)
        assert any(e["type"] == "itsm.ticket.resolved" for e in evts)

    @pytest.mark.asyncio
    async def test_resolve_already_resolved_returns_false(self, mgr):
        t = await mgr.create_ticket("user", "incident", "medium", "Issue", "")
        await mgr.resolve_ticket(t.id, "agent_l1", "Fixed")
        ok = await mgr.resolve_ticket(t.id, "agent_l1", "Fixed again")
        assert not ok

    @pytest.mark.asyncio
    async def test_resolve_nonexistent_returns_false(self, mgr):
        ok = await mgr.resolve_ticket("TKT-9999-9999", "agent_l1", "Fixed")
        assert not ok

    @pytest.mark.asyncio
    async def test_resolve_sets_responded_at_if_not_set(self, mgr):
        t = await mgr.create_ticket("user", "incident", "medium", "Issue", "")
        assert t.responded_at is None
        await mgr.resolve_ticket(t.id, "agent_l1", "Fixed")
        assert t.responded_at is not None


# ── TestAgentEscalation ───────────────────────────────────────────────────────

class TestAgentEscalation:
    @pytest.mark.asyncio
    async def test_l1_escalate_starts_l2(self, mgr, events):
        from itsm import AGENT_L2
        t = await mgr.create_ticket("user", "incident", "medium", "Issue", "")
        await drain_events(events)
        await mgr.escalate_ticket(t.id, "agent_l1", "Cannot fix")
        assert t.status == "l2_triage"
        assert t.agent_tier == AGENT_L2

    @pytest.mark.asyncio
    async def test_l2_escalate_goes_to_human(self, mgr, events):
        from itsm import AGENT_HUMAN
        t = await mgr.create_ticket("user", "incident", "medium", "Issue", "")
        await mgr.escalate_ticket(t.id, "agent_l1", "L1 failed")
        await mgr.escalate_ticket(t.id, "agent_l2", "L2 failed")
        assert t.status == "pending_human"
        assert t.agent_tier == AGENT_HUMAN

    @pytest.mark.asyncio
    async def test_l2_triage_event_has_l2_model_config(self, mgr, events):
        from itsm import AGENT_L2
        t = await mgr.create_ticket("user", "incident", "medium", "Issue", "")
        await drain_events(events)
        await mgr.escalate_ticket(t.id, "agent_l1", "L1 failed")
        evts = await drain_events(events)
        triage = next((e for e in evts if e["type"] == "itsm.ticket.triage"), None)
        assert triage is not None
        mc = triage["model_config"]
        assert mc["provider"] == "anthropic"
        assert "opus" in mc["model"].lower()
        assert triage["tier"] == AGENT_L2

    @pytest.mark.asyncio
    async def test_escalate_resolved_ticket_returns_false(self, mgr):
        t = await mgr.create_ticket("user", "incident", "medium", "Issue", "")
        await mgr.resolve_ticket(t.id, "agent_l1", "Fixed")
        ok = await mgr.escalate_ticket(t.id, "agent_l1", "Try to escalate")
        assert not ok

    @pytest.mark.asyncio
    async def test_escalate_fires_needs_human_event(self, mgr, events):
        t = await mgr.create_ticket("user", "incident", "medium", "Issue", "")
        await mgr.escalate_ticket(t.id, "agent_l1", "L1 failed")
        await mgr.escalate_ticket(t.id, "agent_l2", "L2 failed")
        evts = await drain_events(events)
        assert any(e["type"] == "itsm.ticket.needs_human" for e in evts)

    @pytest.mark.asyncio
    async def test_l1_attempts_incremented(self, mgr):
        t = await mgr.create_ticket("user", "incident", "medium", "Issue", "")
        assert t.l1_attempts == 1

    @pytest.mark.asyncio
    async def test_l2_attempts_incremented_on_escalation(self, mgr):
        t = await mgr.create_ticket("user", "incident", "medium", "Issue", "")
        await mgr.escalate_ticket(t.id, "agent_l1", "L1 failed")
        assert t.l2_attempts == 1

    @pytest.mark.asyncio
    async def test_audit_records_escalation(self, mgr):
        t = await mgr.create_ticket("user", "incident", "medium", "Issue", "")
        await mgr.escalate_ticket(t.id, "agent_l1", "L1 failed")
        actions = [a.action for a in t.audit]
        assert "l2_triage_started" in actions


# ── TestTriageTimeout ─────────────────────────────────────────────────────────

class TestTriageTimeout:
    @pytest.mark.asyncio
    async def test_l1_timeout_triggers_l2(self, tmp_data, events):
        from itsm import ITSMManager, ITSMConfig
        cfg = ITSMConfig(l1_timeout_seconds=0, l2_timeout_seconds=9999)
        m = ITSMManager(tmp_data, config=cfg, event_queue=events)
        from itsm import AGENT_L2
        t = await m.create_ticket("user", "incident", "medium", "Issue", "")
        await asyncio.sleep(0.05)
        assert t.status == "l2_triage"
        assert t.agent_tier == AGENT_L2

    @pytest.mark.asyncio
    async def test_l2_timeout_sends_to_human(self, tmp_data, events):
        from itsm import ITSMManager, ITSMConfig, AGENT_HUMAN
        cfg = ITSMConfig(l1_timeout_seconds=0, l2_timeout_seconds=0)
        m = ITSMManager(tmp_data, config=cfg, event_queue=events)
        t = await m.create_ticket("user", "incident", "medium", "Issue", "")
        await asyncio.sleep(0.1)
        assert t.status == "pending_human"
        assert t.agent_tier == AGENT_HUMAN


# ── TestAcknowledge ───────────────────────────────────────────────────────────

class TestAcknowledge:
    @pytest.mark.asyncio
    async def test_acknowledge_sets_status(self, mgr):
        t = await mgr.create_ticket("user", "incident", "medium", "Issue", "")
        # Escalate to human first
        await mgr.escalate_ticket(t.id, "agent_l1", "L1 failed")
        await mgr.escalate_ticket(t.id, "agent_l2", "L2 failed")
        ok = await mgr.acknowledge_ticket(t.id, "bob")
        assert ok
        assert t.status == "acknowledged"
        assert t.acknowledged_by == "bob"

    @pytest.mark.asyncio
    async def test_acknowledge_fires_event(self, mgr, events):
        t = await mgr.create_ticket("user", "incident", "medium", "Issue", "")
        await mgr.escalate_ticket(t.id, "agent_l1", "L1 failed")
        await mgr.escalate_ticket(t.id, "agent_l2", "L2 failed")
        await drain_events(events)
        await mgr.acknowledge_ticket(t.id, "bob")
        evts = await drain_events(events)
        assert any(e["type"] == "itsm.ticket.acknowledged" for e in evts)

    @pytest.mark.asyncio
    async def test_acknowledge_resolved_ticket_fails(self, mgr):
        t = await mgr.create_ticket("user", "incident", "medium", "Issue", "")
        await mgr.resolve_ticket(t.id, "agent_l1", "Fixed")
        ok = await mgr.acknowledge_ticket(t.id, "bob")
        assert not ok


# ── TestOnCallUser ────────────────────────────────────────────────────────────

class TestOnCallUser:
    def test_interrupt_any_always_available(self):
        from itsm import OnCallUser
        u = OnCallUser(user_id="bob", interrupt_any=True)
        assert u.is_available("low")
        assert u.is_available("medium")
        assert u.is_available("critical")

    def test_active_oncall_window_makes_available(self):
        from itsm import OnCallUser, OnCallWindow
        now = time.time()
        u = OnCallUser(
            user_id="alice",
            oncall_windows=[OnCallWindow(start_ts=now - 60, end_ts=now + 3600)],
        )
        assert u.is_available("low")

    def test_expired_oncall_window_not_available(self):
        from itsm import OnCallUser, OnCallWindow
        now = time.time()
        u = OnCallUser(
            user_id="alice",
            oncall_windows=[OnCallWindow(start_ts=now - 7200, end_ts=now - 3600)],
        )
        assert not u.is_available("low")

    def test_interrupt_critical_true_always_notified_for_critical(self):
        from itsm import OnCallUser
        u = OnCallUser(user_id="dave", interrupt_critical=True)
        # No working hours, no on-call, but critical interrupt enabled
        assert u.is_available("critical")

    def test_interrupt_critical_false_not_notified_outside_hours(self):
        from itsm import OnCallUser
        u = OnCallUser(user_id="dave", interrupt_critical=False, interrupt_any=False)
        # No working hours, no on-call, critical interrupt disabled
        assert not u.is_available("critical")

    def test_interrupt_high_false_not_notified_for_high(self):
        from itsm import OnCallUser
        u = OnCallUser(user_id="dave", interrupt_high=False, interrupt_any=False)
        assert not u.is_available("high")

    def test_interrupt_high_true_notified_for_high(self):
        from itsm import OnCallUser
        u = OnCallUser(user_id="dave", interrupt_high=True)
        assert u.is_available("high")

    def test_in_working_hours(self):
        """Working hours covering the current time should make user available."""
        import datetime
        from itsm import OnCallUser, WorkingHours
        now = datetime.datetime.now()
        wh = WorkingHours(day=now.weekday(), start=0, end=24)
        u = OnCallUser(user_id="carol", working_hours=[wh])
        assert u.is_available("medium")

    def test_outside_working_hours_not_available_for_medium(self):
        """Working hours on a different day → not available for medium priority."""
        from itsm import OnCallUser, WorkingHours
        import datetime
        now = datetime.datetime.now()
        # Different day
        other_day = (now.weekday() + 1) % 7
        wh = WorkingHours(day=other_day, start=9, end=17)
        u = OnCallUser(user_id="carol", working_hours=[wh])
        assert not u.is_available("medium")

    def test_roundtrip_serialisation(self):
        from itsm import OnCallUser, WorkingHours, OnCallWindow
        u = OnCallUser(
            user_id="x",
            channels=["slack1"],
            working_hours=[WorkingHours(0, 9, 17)],
            oncall_windows=[OnCallWindow(1000.0, 2000.0, "weekend cover")],
            interrupt_critical=True,
            interrupt_high=True,
        )
        u2 = OnCallUser.from_dict(u.to_dict())
        assert u2.user_id == u.user_id
        assert len(u2.working_hours) == 1
        assert u2.working_hours[0].start == 9
        assert u2.oncall_windows[0].note == "weekend cover"
        assert u2.interrupt_high is True


# ── TestEscalationPolicy ──────────────────────────────────────────────────────

class TestEscalationPolicy:
    def test_roundtrip_serialisation(self):
        from itsm import EscalationPolicy, EscalationTier
        p = EscalationPolicy(
            id="p1", name="Default",
            tiers=[
                EscalationTier(user_ids=["alice"], ack_timeout_seconds=300),
                EscalationTier(user_ids=["bob", "carol"], ack_timeout_seconds=900,
                               channels_override=["pager"]),
            ],
        )
        p2 = EscalationPolicy.from_dict(p.to_dict())
        assert len(p2.tiers) == 2
        assert p2.tiers[1].user_ids == ["bob", "carol"]
        assert p2.tiers[1].channels_override == ["pager"]


# ── TestOnCallConfig ──────────────────────────────────────────────────────────

class TestOnCallConfig:
    def test_upsert_oncall_user(self, mgr):
        from itsm import OnCallUser
        u = OnCallUser(user_id="alice", channels=["slack"])
        mgr.upsert_oncall_user(u)
        cfg = mgr.get_config()
        assert "alice" in cfg.oncall_users
        assert cfg.oncall_users["alice"].channels == ["slack"]

    def test_remove_oncall_user(self, mgr):
        from itsm import OnCallUser
        mgr.upsert_oncall_user(OnCallUser(user_id="alice"))
        ok = mgr.remove_oncall_user("alice")
        assert ok
        assert "alice" not in mgr.get_config().oncall_users

    def test_remove_nonexistent_user_returns_false(self, mgr):
        assert not mgr.remove_oncall_user("nobody")

    def test_upsert_escalation_policy(self, mgr):
        from itsm import EscalationPolicy, EscalationTier
        p = EscalationPolicy("p1", "Test", [EscalationTier(["alice"], 300)])
        mgr.upsert_escalation_policy(p)
        assert "p1" in mgr.get_config().escalation_policies

    def test_remove_escalation_policy(self, mgr):
        from itsm import EscalationPolicy
        mgr.upsert_escalation_policy(EscalationPolicy("p1", "Test"))
        ok = mgr.remove_escalation_policy("p1")
        assert ok
        assert "p1" not in mgr.get_config().escalation_policies

    def test_remove_policy_clears_default(self, mgr):
        from itsm import EscalationPolicy
        mgr.upsert_escalation_policy(EscalationPolicy("p1", "Test"))
        mgr.get_config().default_policy_id = "p1"
        mgr.remove_escalation_policy("p1")
        assert mgr.get_config().default_policy_id == ""


# ── TestOnCallNotification ────────────────────────────────────────────────────

class TestOnCallNotification:
    @pytest.mark.asyncio
    async def test_escalation_notifies_available_user(self, tmp_data, events, notifier):
        import datetime
        from itsm import ITSMManager, ITSMConfig, OnCallUser, EscalationPolicy, EscalationTier, WorkingHours
        cfg = ITSMConfig(
            l1_max_attempts=1, l2_max_attempts=1,
            l1_timeout_seconds=9999, l2_timeout_seconds=9999,
        )
        m = ITSMManager(tmp_data, config=cfg, notifier=notifier, event_queue=events)
        # User available all week
        now = datetime.datetime.now()
        u = OnCallUser(
            user_id="alice",
            channels=[],
            working_hours=[WorkingHours(now.weekday(), 0, 24)],
        )
        m.upsert_oncall_user(u)
        policy = EscalationPolicy(
            "default", "Default",
            [EscalationTier(["alice"], ack_timeout_seconds=9999)],
        )
        m.upsert_escalation_policy(policy)
        m.get_config().default_policy_id = "default"

        t = await m.create_ticket("user", "incident", "medium", "Issue", "")
        await m.escalate_ticket(t.id, "agent_l1", "L1 failed")
        await m.escalate_ticket(t.id, "agent_l2", "L2 failed")
        assert t.status == "pending_human"
        notifier.on_event.assert_called()

    @pytest.mark.asyncio
    async def test_unavailable_user_not_notified(self, tmp_data, events, notifier):
        from itsm import ITSMManager, ITSMConfig, OnCallUser, EscalationPolicy, EscalationTier
        cfg = ITSMConfig(l1_timeout_seconds=9999, l2_timeout_seconds=9999)
        m = ITSMManager(tmp_data, config=cfg, notifier=notifier, event_queue=events)
        # User with no working hours and no interrupt opt-ins → never available for medium
        u = OnCallUser(user_id="bob", interrupt_critical=False, interrupt_any=False)
        m.upsert_oncall_user(u)
        policy = EscalationPolicy(
            "default", "Default",
            [EscalationTier(["bob"], ack_timeout_seconds=9999)],
        )
        m.upsert_escalation_policy(policy)
        m.get_config().default_policy_id = "default"

        t = await m.create_ticket("user", "incident", "medium", "Issue", "")
        await m.escalate_ticket(t.id, "agent_l1", "")
        await m.escalate_ticket(t.id, "agent_l2", "")
        # on_event should not have been called for a medium-priority, unavailable user
        notifier.on_event.assert_not_called()


# ── TestEscalationTierAdvance ─────────────────────────────────────────────────

class TestEscalationTierAdvance:
    @pytest.mark.asyncio
    async def test_ack_timeout_advances_tier(self, tmp_data, events):
        import datetime
        from itsm import (
            ITSMManager, ITSMConfig, OnCallUser, EscalationPolicy,
            EscalationTier, WorkingHours,
        )
        cfg = ITSMConfig(l1_timeout_seconds=9999, l2_timeout_seconds=9999)
        m = ITSMManager(tmp_data, config=cfg, event_queue=events)
        now = datetime.datetime.now()
        u1 = OnCallUser("tier1", working_hours=[WorkingHours(now.weekday(), 0, 24)])
        u2 = OnCallUser("tier2", working_hours=[WorkingHours(now.weekday(), 0, 24)])
        m.upsert_oncall_user(u1)
        m.upsert_oncall_user(u2)
        policy = EscalationPolicy("default", "Test", [
            EscalationTier(["tier1"], ack_timeout_seconds=0),
            EscalationTier(["tier2"], ack_timeout_seconds=9999),
        ])
        m.upsert_escalation_policy(policy)
        m.get_config().default_policy_id = "default"

        t = await m.create_ticket("user", "incident", "medium", "Issue", "")
        await m.escalate_ticket(t.id, "agent_l1", "")
        await m.escalate_ticket(t.id, "agent_l2", "")
        # Wait for ack timeout on tier 0 to fire
        await asyncio.sleep(0.1)
        assert t.escalation_tier_index == 1

    @pytest.mark.asyncio
    async def test_exhausted_all_tiers_fires_event(self, tmp_data, events):
        from itsm import ITSMManager, ITSMConfig, EscalationPolicy, EscalationTier
        cfg = ITSMConfig(l1_timeout_seconds=9999, l2_timeout_seconds=9999)
        m = ITSMManager(tmp_data, config=cfg, event_queue=events)
        policy = EscalationPolicy("default", "Test", [
            EscalationTier([], ack_timeout_seconds=0),  # no users — advances immediately
        ])
        m.upsert_escalation_policy(policy)
        m.get_config().default_policy_id = "default"

        t = await m.create_ticket("user", "incident", "medium", "Issue", "")
        await m.escalate_ticket(t.id, "agent_l1", "")
        await m.escalate_ticket(t.id, "agent_l2", "")
        await asyncio.sleep(0.1)
        evts = await drain_events(events)
        assert any(e["type"] == "itsm.ticket.escalation_exhausted" for e in evts)


# ── TestTicketList ────────────────────────────────────────────────────────────

class TestTicketList:
    @pytest.mark.asyncio
    async def test_list_all(self, mgr):
        await mgr.create_ticket("user", "incident", "medium", "A", "")
        await mgr.create_ticket("user", "incident", "high", "B", "")
        assert len(mgr.list_tickets()) == 2

    @pytest.mark.asyncio
    async def test_filter_by_status(self, mgr):
        t = await mgr.create_ticket("user", "incident", "medium", "A", "")
        await mgr.resolve_ticket(t.id, "agent_l1", "Fixed")
        await mgr.create_ticket("user", "incident", "medium", "B", "")
        resolved = mgr.list_tickets(status="resolved")
        assert len(resolved) == 1
        assert resolved[0].id == t.id

    @pytest.mark.asyncio
    async def test_filter_by_priority(self, mgr):
        await mgr.create_ticket("user", "incident", "high", "A", "")
        await mgr.create_ticket("user", "incident", "medium", "B", "")
        high = mgr.list_tickets(priority="high")
        assert len(high) == 1

    @pytest.mark.asyncio
    async def test_limit_honoured(self, mgr):
        for i in range(5):
            await mgr.create_ticket("user", "incident", "medium", f"T{i}", "")
        assert len(mgr.list_tickets(limit=3)) == 3

    @pytest.mark.asyncio
    async def test_sorted_newest_first(self, mgr):
        t1 = await mgr.create_ticket("user", "incident", "medium", "Old", "")
        await asyncio.sleep(0.01)
        t2 = await mgr.create_ticket("user", "incident", "medium", "New", "")
        tickets = mgr.list_tickets()
        assert tickets[0].id == t2.id


# ── TestSLAFractions ──────────────────────────────────────────────────────────

class TestSLAFractions:
    @pytest.mark.asyncio
    async def test_fraction_near_zero_at_creation(self, mgr):
        t = await mgr.create_ticket("user", "incident", "medium", "Issue", "")
        assert t.sla_response_fraction() < 0.01

    @pytest.mark.asyncio
    async def test_fraction_is_one_at_deadline(self, mgr):
        t = await mgr.create_ticket("user", "incident", "medium", "Issue", "")
        t.sla_response_deadline = time.time() - 1  # just past
        assert t.sla_response_fraction() >= 1.0
        assert t.sla_response_breached

    @pytest.mark.asyncio
    async def test_resolved_ticket_not_breached(self, mgr):
        t = await mgr.create_ticket("user", "incident", "medium", "Issue", "")
        t.sla_resolution_deadline = time.time() - 1
        await mgr.resolve_ticket(t.id, "agent_l1", "Fixed")
        assert not t.sla_resolution_breached


# ── TestStatus ────────────────────────────────────────────────────────────────

class TestStatus:
    @pytest.mark.asyncio
    async def test_status_returns_counts(self, mgr):
        await mgr.create_ticket("user", "incident", "critical", "P1", "")
        await mgr.create_ticket("user", "incident", "medium", "P3", "")
        s = mgr.status()
        assert s["total"] == 2
        assert s["open"] == 2
        assert s["by_priority"]["critical"] == 1

    @pytest.mark.asyncio
    async def test_resolved_not_counted_as_open(self, mgr):
        t = await mgr.create_ticket("user", "incident", "medium", "Issue", "")
        await mgr.resolve_ticket(t.id, "agent_l1", "Fixed")
        s = mgr.status()
        assert s["open"] == 0


# ── TestITSMConfig ────────────────────────────────────────────────────────────

class TestITSMConfig:
    def test_roundtrip(self):
        from itsm import ITSMConfig, AgentModelConfig
        cfg = ITSMConfig(
            default_policy_id="p1",
            l1_timeout_seconds=120,
            l2_timeout_seconds=600,
            external_webhook_url="https://example.com/hook",
            l1_model=AgentModelConfig(provider="openai", model="gpt-4o-mini"),
            l2_model=AgentModelConfig(provider="openai", model="gpt-4o"),
        )
        cfg2 = ITSMConfig.from_dict(cfg.to_dict())
        assert cfg2.default_policy_id == "p1"
        assert cfg2.l1_timeout_seconds == 120
        assert cfg2.external_webhook_url == "https://example.com/hook"
        assert cfg2.l1_model.provider == "openai"
        assert cfg2.l1_model.model == "gpt-4o-mini"
        assert cfg2.l2_model.model == "gpt-4o"

    def test_set_config_persists(self, mgr, tmp_data):
        cfg = mgr.get_config()
        cfg.l1_timeout_seconds = 999
        mgr.set_config(cfg)
        assert (tmp_data / "itsm_config.json").exists()
        data = (tmp_data / "itsm_config.json").read_text()
        assert "999" in data

    def test_model_config_included_in_persisted_config(self, mgr, tmp_data):
        from itsm import AgentModelConfig
        cfg = mgr.get_config()
        cfg.l1_model = AgentModelConfig(
            provider="ollama", model="llama3.2:3b",
            base_url="http://localhost:11434",
        )
        mgr.set_config(cfg)
        import json
        saved = json.loads((tmp_data / "itsm_config.json").read_text())
        assert saved["l1_model"]["provider"] == "ollama"
        assert saved["l1_model"]["model"] == "llama3.2:3b"
        assert saved["l1_model"]["base_url"] == "http://localhost:11434"


# ── TestAgentModelConfig ──────────────────────────────────────────────────────

class TestAgentModelConfig:
    def test_defaults_to_anthropic(self):
        from itsm import AgentModelConfig
        m = AgentModelConfig()
        assert m.provider == "anthropic"

    def test_roundtrip_all_fields(self):
        from itsm import AgentModelConfig
        m = AgentModelConfig(
            provider="groq",
            model="llama-3.1-8b-instant",
            base_url="https://api.groq.com/openai/v1",
            api_key_env="GROQ_API_KEY",
            extra={"temperature": 0.1, "max_tokens": 2048},
        )
        m2 = AgentModelConfig.from_dict(m.to_dict())
        assert m2.provider == "groq"
        assert m2.model == "llama-3.1-8b-instant"
        assert m2.base_url == "https://api.groq.com/openai/v1"
        assert m2.api_key_env == "GROQ_API_KEY"
        assert m2.extra["temperature"] == 0.1

    def test_api_key_env_not_the_key_itself(self):
        """api_key_env must be an env var *name*, not the secret value."""
        from itsm import AgentModelConfig
        m = AgentModelConfig(provider="openai", model="gpt-4o", api_key_env="OPENAI_API_KEY")
        d = m.to_dict()
        assert d["api_key_env"] == "OPENAI_API_KEY"
        # The dict should never contain an actual key value
        assert not any(v for v in d.values() if isinstance(v, str) and v.startswith("sk-"))

    def test_ollama_config(self):
        from itsm import AgentModelConfig
        m = AgentModelConfig(
            provider="ollama",
            model="qwen2.5-coder:7b",
            base_url="http://192.168.1.5:11434",
        )
        d = m.to_dict()
        assert d["provider"] == "ollama"
        assert d["base_url"] == "http://192.168.1.5:11434"
        assert d["api_key_env"] == ""

    @pytest.mark.asyncio
    async def test_triage_event_uses_configured_model(self, tmp_data, events):
        from itsm import ITSMManager, ITSMConfig, AgentModelConfig
        cfg = ITSMConfig(
            l1_model=AgentModelConfig(
                provider="ollama",
                model="llama3.2:3b",
                base_url="http://localhost:11434",
            ),
            l2_model=AgentModelConfig(
                provider="openai",
                model="gpt-4o",
                api_key_env="OPENAI_API_KEY",
            ),
        )
        m = ITSMManager(tmp_data, config=cfg, event_queue=events)
        await m.create_ticket("user", "incident", "medium", "Issue", "")
        evts = await drain_events(events)
        triage = next(e for e in evts if e["type"] == "itsm.ticket.triage")
        mc = triage["model_config"]
        assert mc["provider"] == "ollama"
        assert mc["model"] == "llama3.2:3b"
        assert mc["base_url"] == "http://localhost:11434"

    @pytest.mark.asyncio
    async def test_l2_triage_event_uses_l2_model(self, tmp_data, events):
        from itsm import ITSMManager, ITSMConfig, AgentModelConfig
        cfg = ITSMConfig(
            l2_model=AgentModelConfig(
                provider="openai", model="gpt-4o", api_key_env="OPENAI_API_KEY"
            ),
        )
        m = ITSMManager(tmp_data, config=cfg, event_queue=events)
        t = await m.create_ticket("user", "incident", "medium", "Issue", "")
        await drain_events(events)
        await m.escalate_ticket(t.id, "agent_l1", "L1 failed")
        evts = await drain_events(events)
        triage = next(e for e in evts if e["type"] == "itsm.ticket.triage")
        mc = triage["model_config"]
        assert mc["provider"] == "openai"
        assert mc["model"] == "gpt-4o"

    @pytest.mark.asyncio
    async def test_config_survives_reload(self, tmp_data, events):
        """Model config written to disk is read back correctly on reload."""
        from itsm import ITSMManager, ITSMConfig, AgentModelConfig
        cfg = ITSMConfig(
            l1_model=AgentModelConfig(provider="groq", model="llama-3.1-8b-instant"),
        )
        m1 = ITSMManager(tmp_data, config=cfg, event_queue=events)
        m1.save_config()
        # Reload from disk
        m2 = ITSMManager(tmp_data, event_queue=events)
        assert m2.get_config().l1_model.provider == "groq"
        assert m2.get_config().l1_model.model == "llama-3.1-8b-instant"
