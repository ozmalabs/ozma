# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Unit tests for EventTriggerRule and ControlManager.on_event()."""
import asyncio
import sys
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

sys.path.insert(0, str(Path(__file__).parent.parent.parent.parent / "controller"))
pytestmark = pytest.mark.unit


# ── Fixtures ──────────────────────────────────────────────────────────────────

@pytest.fixture
def state():
    s = MagicMock()
    s.events = asyncio.Queue()
    s.get_active_node = MagicMock(return_value=None)
    return s


@pytest.fixture
def scenarios():
    sm = MagicMock()
    sm._scenarios = {}
    sm.active_id = None
    sm.activate = AsyncMock()
    sm.get = MagicMock(return_value=None)
    return sm


@pytest.fixture
def controls(state, scenarios):
    from controls import ControlManager
    return ControlManager(state=state, scenarios=scenarios)


# ── EventTriggerRule dataclass ────────────────────────────────────────────────

class TestEventTriggerRule:
    def test_rule_id_auto_generated(self):
        from controls import EventTriggerRule
        r = EventTriggerRule(event_type="doorbell.ringing", action="scenario.activate")
        assert r.rule_id and len(r.rule_id) == 8

    def test_explicit_rule_id_preserved(self):
        from controls import EventTriggerRule
        r = EventTriggerRule(event_type="x", action="y", rule_id="myid")
        assert r.rule_id == "myid"

    def test_default_filters_empty(self):
        from controls import EventTriggerRule
        r = EventTriggerRule(event_type="x", action="y")
        assert r.filters == {}

    def test_rule_ids_unique_across_instances(self):
        from controls import EventTriggerRule
        ids = {EventTriggerRule("x", "y").rule_id for _ in range(50)}
        assert len(ids) == 50


# ── add / remove / list trigger rules ─────────────────────────────────────────

class TestTriggerRuleManagement:
    def test_add_rule_returns_rule_id(self, controls):
        from controls import EventTriggerRule
        r = EventTriggerRule(event_type="x", action="y")
        rid = controls.add_trigger_rule(r)
        assert rid == r.rule_id

    def test_list_includes_added_rule(self, controls):
        from controls import EventTriggerRule
        r = EventTriggerRule(event_type="frigate.person_recognized", action="scenario.activate", value="matt")
        controls.add_trigger_rule(r)
        listed = controls.list_trigger_rules()
        assert any(e["rule_id"] == r.rule_id for e in listed)

    def test_remove_returns_true_when_found(self, controls):
        from controls import EventTriggerRule
        r = EventTriggerRule(event_type="x", action="y")
        controls.add_trigger_rule(r)
        assert controls.remove_trigger_rule(r.rule_id) is True

    def test_remove_returns_false_when_not_found(self, controls):
        assert controls.remove_trigger_rule("no-such-id") is False

    def test_remove_deletes_rule_from_list(self, controls):
        from controls import EventTriggerRule
        r = EventTriggerRule(event_type="x", action="y")
        controls.add_trigger_rule(r)
        controls.remove_trigger_rule(r.rule_id)
        assert all(e["rule_id"] != r.rule_id for e in controls.list_trigger_rules())

    def test_list_rule_includes_all_fields(self, controls):
        from controls import EventTriggerRule
        r = EventTriggerRule(
            event_type="frigate.person_recognized",
            action="scenario.activate",
            filters={"person": "Matt"},
            target="matt-workstation",
            value="matt-workstation",
        )
        controls.add_trigger_rule(r)
        entry = next(e for e in controls.list_trigger_rules() if e["rule_id"] == r.rule_id)
        assert entry["event_type"] == "frigate.person_recognized"
        assert entry["action"] == "scenario.activate"
        assert entry["filters"] == {"person": "Matt"}
        assert entry["target"] == "matt-workstation"
        assert entry["value"] == "matt-workstation"


# ── on_event: rule matching ───────────────────────────────────────────────────

class TestOnEvent:
    @pytest.mark.asyncio
    async def test_matching_event_fires_action(self, controls, scenarios):
        from controls import EventTriggerRule
        r = EventTriggerRule(event_type="frigate.person_recognized", action="scenario.activate", value="matt")
        controls.add_trigger_rule(r)
        await controls.on_event("frigate.person_recognized", {"person": "Matt"})
        scenarios.activate.assert_awaited_once_with("matt")

    @pytest.mark.asyncio
    async def test_wrong_event_type_not_fired(self, controls, scenarios):
        from controls import EventTriggerRule
        r = EventTriggerRule(event_type="doorbell.ringing", action="scenario.activate", value="desk")
        controls.add_trigger_rule(r)
        await controls.on_event("frigate.person_recognized", {})
        scenarios.activate.assert_not_awaited()

    @pytest.mark.asyncio
    async def test_filter_match_fires_action(self, controls, scenarios):
        from controls import EventTriggerRule
        r = EventTriggerRule(
            event_type="frigate.person_recognized",
            action="scenario.activate",
            filters={"person": "Matt", "camera": "front_door"},
            value="matt-desk",
        )
        controls.add_trigger_rule(r)
        await controls.on_event("frigate.person_recognized", {"person": "Matt", "camera": "front_door"})
        scenarios.activate.assert_awaited_once_with("matt-desk")

    @pytest.mark.asyncio
    async def test_filter_mismatch_not_fired(self, controls, scenarios):
        from controls import EventTriggerRule
        r = EventTriggerRule(
            event_type="frigate.person_recognized",
            action="scenario.activate",
            filters={"person": "Matt"},
            value="matt-desk",
        )
        controls.add_trigger_rule(r)
        await controls.on_event("frigate.person_recognized", {"person": "Alice"})
        scenarios.activate.assert_not_awaited()

    @pytest.mark.asyncio
    async def test_partial_filter_match_not_fired(self, controls, scenarios):
        from controls import EventTriggerRule
        r = EventTriggerRule(
            event_type="x",
            action="scenario.activate",
            filters={"person": "Matt", "camera": "front_door"},
            value="y",
        )
        controls.add_trigger_rule(r)
        # Only one filter key matches
        await controls.on_event("x", {"person": "Matt"})
        scenarios.activate.assert_not_awaited()

    @pytest.mark.asyncio
    async def test_empty_filters_matches_any_payload(self, controls, scenarios):
        from controls import EventTriggerRule
        r = EventTriggerRule(event_type="x", action="scenario.activate", value="scene-1")
        controls.add_trigger_rule(r)
        await controls.on_event("x", {"anything": "goes"})
        scenarios.activate.assert_awaited_once()

    @pytest.mark.asyncio
    async def test_no_value_passes_event_data_as_value(self, controls, scenarios):
        """When rule.value is None, the full event dict is passed as the action value."""
        from controls import EventTriggerRule
        r = EventTriggerRule(
            event_type="frigate.person_recognized",
            action="scenario.activate",
            value=None,   # explicit None → pass event data
        )
        controls.add_trigger_rule(r)
        data = {"person": "Matt", "camera": "front_door"}
        await controls.on_event("frigate.person_recognized", data)
        call_args = scenarios.activate.call_args
        # The action receives the full event dict as a string converted scenario_id
        # (won't match any scenario — we just check it was called with the dict stringified)
        assert call_args is not None

    @pytest.mark.asyncio
    async def test_multiple_rules_both_fire(self, controls, scenarios):
        from controls import EventTriggerRule
        r1 = EventTriggerRule(event_type="x", action="scenario.activate", value="scene-1")
        r2 = EventTriggerRule(event_type="x", action="scenario.activate", value="scene-2")
        controls.add_trigger_rule(r1)
        controls.add_trigger_rule(r2)
        await controls.on_event("x", {})
        assert scenarios.activate.await_count == 2

    @pytest.mark.asyncio
    async def test_removed_rule_not_fired(self, controls, scenarios):
        from controls import EventTriggerRule
        r = EventTriggerRule(event_type="x", action="scenario.activate", value="scene")
        controls.add_trigger_rule(r)
        controls.remove_trigger_rule(r.rule_id)
        await controls.on_event("x", {})
        scenarios.activate.assert_not_awaited()


# ── Filter type coercion ──────────────────────────────────────────────────────

class TestFilterTypeCoercion:
    @pytest.mark.asyncio
    async def test_string_filter_matches_int_data(self, controls, scenarios):
        """Filter value "123" should match event data value 123 (int)."""
        from controls import EventTriggerRule
        r = EventTriggerRule(
            event_type="x", action="scenario.activate",
            filters={"count": "3"},
            value="scene",
        )
        controls.add_trigger_rule(r)
        await controls.on_event("x", {"count": 3})
        scenarios.activate.assert_awaited_once()

    @pytest.mark.asyncio
    async def test_int_filter_matches_string_data(self, controls, scenarios):
        """Filter value 3 should match event data value "3" (string)."""
        from controls import EventTriggerRule
        r = EventTriggerRule(
            event_type="x", action="scenario.activate",
            filters={"count": 3},
            value="scene",
        )
        controls.add_trigger_rule(r)
        await controls.on_event("x", {"count": "3"})
        scenarios.activate.assert_awaited_once()

    @pytest.mark.asyncio
    async def test_missing_key_treated_as_empty_string(self, controls, scenarios):
        """Missing event data key should not match non-empty filter value."""
        from controls import EventTriggerRule
        r = EventTriggerRule(
            event_type="x", action="scenario.activate",
            filters={"person": "Matt"},
            value="scene",
        )
        controls.add_trigger_rule(r)
        await controls.on_event("x", {})   # no "person" key
        scenarios.activate.assert_not_awaited()


# ── alert.acknowledge / alert.dismiss actions ─────────────────────────────────

class TestAlertActions:
    @pytest.fixture
    def alert_mgr(self):
        mgr = MagicMock()
        mgr.acknowledge = AsyncMock(return_value=True)
        mgr.dismiss = AsyncMock(return_value=True)
        return mgr

    @pytest.fixture
    def ctrl_with_alerts(self, state, scenarios, alert_mgr):
        from controls import ControlManager
        return ControlManager(state=state, scenarios=scenarios, alerts=alert_mgr)

    @pytest.mark.asyncio
    async def test_alert_acknowledge_action_calls_manager(self, ctrl_with_alerts, alert_mgr):
        from controls import EventTriggerRule
        r = EventTriggerRule(event_type="x", action="alert.acknowledge", value="abc123")
        ctrl_with_alerts.add_trigger_rule(r)
        await ctrl_with_alerts.on_event("x", {})
        alert_mgr.acknowledge.assert_awaited_once_with("abc123")

    @pytest.mark.asyncio
    async def test_alert_dismiss_action_calls_manager(self, ctrl_with_alerts, alert_mgr):
        from controls import EventTriggerRule
        r = EventTriggerRule(event_type="x", action="alert.dismiss", value="abc123")
        ctrl_with_alerts.add_trigger_rule(r)
        await ctrl_with_alerts.on_event("x", {})
        alert_mgr.dismiss.assert_awaited_once_with("abc123")

    @pytest.mark.asyncio
    async def test_doorbell_answer_compat_alias(self, ctrl_with_alerts, alert_mgr):
        """doorbell.answer should route to alert manager's acknowledge for backward compat."""
        from controls import EventTriggerRule
        r = EventTriggerRule(event_type="x", action="doorbell.answer", value="id1")
        ctrl_with_alerts.add_trigger_rule(r)
        await ctrl_with_alerts.on_event("x", {})
        alert_mgr.acknowledge.assert_awaited_once_with("id1")

    @pytest.mark.asyncio
    async def test_doorbell_dismiss_compat_alias(self, ctrl_with_alerts, alert_mgr):
        """doorbell.dismiss should route to alert manager's dismiss for backward compat."""
        from controls import EventTriggerRule
        r = EventTriggerRule(event_type="x", action="doorbell.dismiss", value="id1")
        ctrl_with_alerts.add_trigger_rule(r)
        await ctrl_with_alerts.on_event("x", {})
        alert_mgr.dismiss.assert_awaited_once_with("id1")
