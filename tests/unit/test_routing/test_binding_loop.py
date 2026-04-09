# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Unit tests for the BindingRegistry evaluation loop (Phase 2 runtime)."""

from __future__ import annotations

import asyncio
import sys
import time
from pathlib import Path
from unittest.mock import MagicMock

sys.path.insert(0, str(Path(__file__).parent.parent.parent.parent / "controller"))

import pytest
from routing.binding import (
    BindingCondition,
    BindingRegistry,
    BindingScope,
    ConditionMode,
    ConditionOp,
    ConditionSource,
    DictStateResolver,
    IntentBinding,
    RevertPolicy,
)
from routing.binding_loop import AppStateResolver, BindingLoop, EvaluationResult
from routing.intent import BUILTIN_INTENTS
from routing.monitoring import MonitoringJournal, StateChangeType

pytestmark = pytest.mark.unit


# ── Helpers ───────────────────────────────────────────────────────────────────

def _binding(
    bid: str = "b1",
    name: str = "Test",
    intent_name: str = "control",
    priority: int = 50,
    condition_value: bool = True,
) -> IntentBinding:
    """A binding that always matches (eq True) or never (eq False)."""
    return IntentBinding(
        id=bid,
        name=name,
        conditions=[BindingCondition(
            source=ConditionSource.activity,
            field="active_node_id",
            op=ConditionOp.neq,
            value=None,  # True when active_node_id is not None
        )],
        condition_mode=ConditionMode.all,
        intent_name=intent_name,
        priority=priority,
        enabled=True,
    )


def _always_active_binding(bid: str = "b1", intent_name: str = "control") -> IntentBinding:
    """A binding that evaluates to True when active_node_id is "vm1"."""
    return IntentBinding(
        id=bid,
        name=bid,
        conditions=[BindingCondition(
            source=ConditionSource.activity,
            field="active_node_id",
            op=ConditionOp.eq,
            value="vm1",
        )],
        intent_name=intent_name,
        priority=50,
        enabled=True,
    )


def _make_resolver(active_node_id: str | None = "vm1") -> DictStateResolver:
    r = DictStateResolver()
    r.set(ConditionSource.activity, "active_node_id", active_node_id)
    return r


# ── AppStateResolver ──────────────────────────────────────────────────────────

class TestAppStateResolver:
    def _make_state(self) -> MagicMock:
        state = MagicMock()
        state.active_node_id = "vm1"
        state.nodes = {"vm1": MagicMock(), "vm2": MagicMock()}
        return state

    def test_active_node_id(self):
        state = self._make_state()
        resolver = AppStateResolver(state)
        result = resolver(ConditionSource.activity, "active_node_id")
        assert result == "vm1"

    def test_node_count(self):
        state = self._make_state()
        resolver = AppStateResolver(state)
        assert resolver(ConditionSource.activity, "node_count") == 2

    def test_node_online_present(self):
        state = self._make_state()
        resolver = AppStateResolver(state)
        assert resolver(ConditionSource.activity, "node.vm1.online") is True

    def test_node_online_absent(self):
        state = self._make_state()
        resolver = AppStateResolver(state)
        assert resolver(ConditionSource.activity, "node.missing.online") is False

    def test_link_status(self):
        state = MagicMock()
        link = MagicMock()
        link.state.status.value = "active"
        link.source.device_id = "ctrl"
        state.routing_graph.get_link.return_value = link
        resolver = AppStateResolver(state)
        result = resolver(ConditionSource.link, "link-ab.status")
        assert result == "active"

    def test_link_not_found_returns_none(self):
        state = MagicMock()
        state.routing_graph.get_link.return_value = None
        resolver = AppStateResolver(state)
        result = resolver(ConditionSource.link, "nonexistent.status")
        assert result is None

    def test_device_property(self):
        state = MagicMock()
        device = MagicMock()
        device.properties = {"machine_class": "server"}
        state.routing_graph.get_device.return_value = device
        resolver = AppStateResolver(state)
        result = resolver(ConditionSource.device, "my-device.machine_class")
        assert result == "server"

    def test_unknown_source_returns_none(self):
        state = MagicMock()
        resolver = AppStateResolver(state)
        result = resolver(ConditionSource.calendar, "some.field")
        assert result is None

    def test_exception_in_resolve_returns_none(self):
        state = MagicMock()
        state.nodes = None  # will raise AttributeError on len()
        resolver = AppStateResolver(state)
        result = resolver(ConditionSource.activity, "node_count")
        assert result is None


# ── EvaluationResult ──────────────────────────────────────────────────────────

class TestEvaluationResult:
    def test_to_dict_with_binding(self):
        binding = _always_active_binding()
        intent = BUILTIN_INTENTS["control"]
        result = EvaluationResult(
            binding=binding,
            intent=intent,
            evaluated_at=1000.0,
            elapsed_ms=0.5,
            total_bindings=3,
        )
        d = result.to_dict()
        assert d["binding_id"] == "b1"
        assert d["intent_name"] == "control"
        assert d["elapsed_ms"] == pytest.approx(0.5)
        assert d["total_bindings"] == 3

    def test_to_dict_no_match(self):
        result = EvaluationResult(
            binding=None,
            intent=None,
            evaluated_at=1000.0,
            elapsed_ms=0.1,
            total_bindings=0,
        )
        d = result.to_dict()
        assert d["binding_id"] is None
        assert d["intent_name"] is None


# ── BindingLoop.evaluate_once ─────────────────────────────────────────────────

class TestBindingLoopEvaluateOnce:
    def test_no_bindings_returns_none_result(self):
        registry = BindingRegistry()
        resolver = _make_resolver("vm1")
        loop = BindingLoop(registry, resolver)
        result = loop.evaluate_once()
        assert result.binding is None
        assert result.intent is None

    def test_active_binding_matched(self):
        registry = BindingRegistry()
        registry.add(_always_active_binding("b1", "control"))
        resolver = _make_resolver("vm1")  # active_node_id == "vm1" → binding matches
        loop = BindingLoop(registry, resolver)
        result = loop.evaluate_once()
        assert result.binding is not None
        assert result.binding.id == "b1"
        assert result.intent is not None
        assert result.intent.name == "control"

    def test_inactive_binding_not_matched(self):
        registry = BindingRegistry()
        registry.add(_always_active_binding("b1", "control"))
        resolver = _make_resolver(None)  # active_node_id is None → doesn't match eq "vm1"
        loop = BindingLoop(registry, resolver)
        result = loop.evaluate_once()
        assert result.binding is None

    def test_current_updated_after_evaluate(self):
        registry = BindingRegistry()
        resolver = _make_resolver()
        loop = BindingLoop(registry, resolver)
        assert loop.current is None
        loop.evaluate_once()
        assert loop.current is not None

    def test_higher_priority_binding_wins(self):
        registry = BindingRegistry()
        # Both bindings active (both match "vm1"), but b_high has higher priority
        b_low = IntentBinding(
            id="b-low", name="low", priority=10, intent_name="control",
            conditions=[BindingCondition(
                source=ConditionSource.activity, field="active_node_id",
                op=ConditionOp.eq, value="vm1",
            )],
        )
        b_high = IntentBinding(
            id="b-high", name="high", priority=100, intent_name="desktop",
            conditions=[BindingCondition(
                source=ConditionSource.activity, field="active_node_id",
                op=ConditionOp.eq, value="vm1",
            )],
        )
        registry.add(b_low)
        registry.add(b_high)
        resolver = _make_resolver("vm1")
        loop = BindingLoop(registry, resolver)
        result = loop.evaluate_once()
        assert result.binding.id == "b-high"

    def test_elapsed_ms_populated(self):
        registry = BindingRegistry()
        loop = BindingLoop(registry, _make_resolver())
        result = loop.evaluate_once()
        assert result.elapsed_ms >= 0.0

    def test_total_bindings_count(self):
        registry = BindingRegistry()
        registry.add(_always_active_binding("b1"))
        registry.add(_always_active_binding("b2", "desktop"))
        loop = BindingLoop(registry, _make_resolver())
        result = loop.evaluate_once()
        assert result.total_bindings == 2


# ── Journal event emission ────────────────────────────────────────────────────

class TestBindingLoopJournalEvents:
    def test_no_events_when_binding_unchanged(self):
        journal = MonitoringJournal()
        registry = BindingRegistry()
        registry.add(_always_active_binding("b1"))
        resolver = _make_resolver("vm1")
        loop = BindingLoop(registry, resolver, journal=journal)
        loop.evaluate_once()
        count_after_first = len(journal)
        loop.evaluate_once()  # same binding active — no change
        assert len(journal) == count_after_first

    def test_intent_bound_emitted_on_activation(self):
        journal = MonitoringJournal()
        registry = BindingRegistry()
        registry.add(_always_active_binding("b1"))
        resolver = _make_resolver("vm1")
        loop = BindingLoop(registry, resolver, journal=journal)
        loop.evaluate_once()
        entries = journal.query()
        assert any(e.type == StateChangeType.intent_bound for e in entries)

    def test_intent_unbound_emitted_on_deactivation(self):
        journal = MonitoringJournal()
        registry = BindingRegistry()
        registry.add(_always_active_binding("b1"))
        resolver = _make_resolver("vm1")
        loop = BindingLoop(registry, resolver, journal=journal)
        loop.evaluate_once()  # binding activates
        # Now switch resolver to return None — binding deactivates
        resolver2 = _make_resolver(None)
        loop._resolver = resolver2
        loop.evaluate_once()  # binding deactivates
        entries = journal.query()
        assert any(e.type == StateChangeType.intent_unbound for e in entries)

    def test_no_journal_no_error(self):
        """When journal=None, transitions don't crash."""
        registry = BindingRegistry()
        registry.add(_always_active_binding("b1"))
        resolver = _make_resolver("vm1")
        loop = BindingLoop(registry, resolver, journal=None)
        loop.evaluate_once()  # should not raise
        loop._resolver = _make_resolver(None)
        loop.evaluate_once()  # deactivation without journal — should not raise

    def test_binding_switch_emits_unbound_then_bound(self):
        """When winning binding changes, old is unbound and new is bound."""
        journal = MonitoringJournal()
        registry = BindingRegistry()
        b1 = _always_active_binding("b1", "control")
        b2 = IntentBinding(
            id="b2", name="b2", priority=100, intent_name="desktop",
            conditions=[BindingCondition(
                source=ConditionSource.activity, field="active_node_id",
                op=ConditionOp.eq, value="vm2",  # matches vm2 only
            )],
        )
        registry.add(b1)
        registry.add(b2)
        resolver_vm1 = _make_resolver("vm1")
        resolver_vm2 = _make_resolver("vm2")
        loop = BindingLoop(registry, resolver_vm1, journal=journal)
        loop.evaluate_once()  # b1 active (vm1 matches b1)
        loop._resolver = resolver_vm2
        loop.evaluate_once()  # b2 wins (vm2 matches b2 and b2 has higher priority)
        types = [e.type for e in journal.query()]
        assert StateChangeType.intent_unbound in types
        assert StateChangeType.intent_bound in types


# ── to_dict ───────────────────────────────────────────────────────────────────

class TestBindingLoopToDict:
    def test_initial_state(self):
        loop = BindingLoop(BindingRegistry(), _make_resolver())
        d = loop.to_dict()
        assert d["running"] is False
        assert d["task_active"] is False
        assert d["current"] is None
        assert "interval_s" in d

    def test_after_evaluate(self):
        loop = BindingLoop(BindingRegistry(), _make_resolver())
        loop.evaluate_once()
        d = loop.to_dict()
        assert d["current"] is not None


# ── Start/stop ────────────────────────────────────────────────────────────────

class TestBindingLoopStartStop:
    @pytest.mark.asyncio
    async def test_start_creates_task(self):
        loop = BindingLoop(BindingRegistry(), _make_resolver(), interval_s=100.0)
        await loop.start()
        try:
            assert loop._running is True
            assert loop._task is not None
        finally:
            await loop.stop()

    @pytest.mark.asyncio
    async def test_stop_cancels_task(self):
        loop = BindingLoop(BindingRegistry(), _make_resolver(), interval_s=100.0)
        await loop.start()
        await loop.stop()
        assert loop._running is False
        assert loop._task is None

    @pytest.mark.asyncio
    async def test_double_start_idempotent(self):
        loop = BindingLoop(BindingRegistry(), _make_resolver(), interval_s=100.0)
        await loop.start()
        first = loop._task
        await loop.start()
        assert loop._task is first
        await loop.stop()
