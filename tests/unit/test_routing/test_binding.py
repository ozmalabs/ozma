# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Unit tests for the intent binding system (Phase 2)."""

from __future__ import annotations

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent.parent.parent.parent / "controller"))

import pytest
from routing.binding import (
    BindingCondition,
    BindingScope,
    RevertPolicy,
    IntentBinding,
    BindingRegistry,
    ConditionOp,
    ConditionMode,
    ConditionSource,
    RevertMode,
    DictStateResolver,
    StateResolver,
)
from routing.intent import BUILTIN_INTENTS
from routing.model import MediaType

pytestmark = pytest.mark.unit


# ── ConditionOp ───────────────────────────────────────────────────────────────

class TestConditionOp:
    def test_from_str_in(self):
        op = ConditionOp.from_str("in")
        assert op == ConditionOp.in_

    def test_from_str_eq(self):
        assert ConditionOp.from_str("eq") == ConditionOp.eq

    def test_to_str_in(self):
        assert ConditionOp.in_.to_str() == "in"

    def test_to_str_eq(self):
        assert ConditionOp.eq.to_str() == "eq"


# ── BindingCondition.evaluate ─────────────────────────────────────────────────

class TestBindingConditionEvaluate:
    def _cond(self, op, value):
        return BindingCondition(
            source=ConditionSource.activity,
            field="state",
            op=op,
            value=value,
        )

    def test_eq_true(self):
        assert self._cond(ConditionOp.eq, "gaming").evaluate("gaming")

    def test_eq_false(self):
        assert not self._cond(ConditionOp.eq, "gaming").evaluate("idle")

    def test_neq_true(self):
        assert self._cond(ConditionOp.neq, "idle").evaluate("gaming")

    def test_gt_true(self):
        assert self._cond(ConditionOp.gt, 50).evaluate(60)

    def test_gt_false(self):
        assert not self._cond(ConditionOp.gt, 50).evaluate(40)

    def test_lt_true(self):
        assert self._cond(ConditionOp.lt, 50).evaluate(30)

    def test_in_true(self):
        assert self._cond(ConditionOp.in_, ["a", "b", "c"]).evaluate("b")

    def test_in_false(self):
        assert not self._cond(ConditionOp.in_, ["a", "b"]).evaluate("z")

    def test_contains_true(self):
        assert self._cond(ConditionOp.contains, "gam").evaluate("gaming")

    def test_contains_false(self):
        assert not self._cond(ConditionOp.contains, "xyz").evaluate("gaming")

    def test_matches_true(self):
        assert self._cond(ConditionOp.matches, r"vm\d+").evaluate("vm2")

    def test_matches_false(self):
        assert not self._cond(ConditionOp.matches, r"vm\d+").evaluate("pc1")

    def test_none_actual_returns_false(self):
        assert not self._cond(ConditionOp.eq, "gaming").evaluate(None)

    def test_type_error_returns_false(self):
        # gt on non-comparable types
        assert not self._cond(ConditionOp.gt, "text").evaluate(42)

    def test_to_dict(self):
        c = BindingCondition(
            source=ConditionSource.device,
            field="name",
            op=ConditionOp.eq,
            value="desk-pc",
        )
        d = c.to_dict()
        assert d["source"] == "device"
        assert d["op"] == "eq"
        assert d["value"] == "desk-pc"

    def test_from_dict(self):
        d = {"source": "activity", "field": "state", "op": "in", "value": ["gaming"]}
        c = BindingCondition.from_dict(d)
        assert c.op == ConditionOp.in_
        assert c.value == ["gaming"]


# ── BindingScope ──────────────────────────────────────────────────────────────

class TestBindingScope:
    def test_defaults(self):
        s = BindingScope()
        assert s.target == "node"
        assert s.target_id is None
        assert s.streams == []

    def test_to_dict(self):
        s = BindingScope(target="node", target_id="node-1",
                         streams=[MediaType.video, MediaType.hid])
        d = s.to_dict()
        assert d["target_id"] == "node-1"
        assert "video" in d["streams"]
        assert "hid" in d["streams"]

    def test_from_dict(self):
        d = {"target": "controller", "target_id": None, "streams": ["audio"]}
        s = BindingScope.from_dict(d)
        assert s.target == "controller"
        assert MediaType.audio in s.streams


# ── RevertPolicy ──────────────────────────────────────────────────────────────

class TestRevertPolicy:
    def test_defaults(self):
        r = RevertPolicy()
        assert r.mode == RevertMode.revert
        assert r.timeout_ms is None

    def test_to_dict(self):
        r = RevertPolicy(mode=RevertMode.timeout, timeout_ms=5000)
        d = r.to_dict()
        assert d["mode"] == "timeout"
        assert d["timeout_ms"] == 5000

    def test_from_dict(self):
        r = RevertPolicy.from_dict({"mode": "hold", "timeout_ms": None})
        assert r.mode == RevertMode.hold


# ── IntentBinding ─────────────────────────────────────────────────────────────

class TestIntentBinding:
    def _make_binding(self, mode=ConditionMode.all, conditions=None):
        conds = conditions or [
            BindingCondition(
                source=ConditionSource.activity,
                field="state",
                op=ConditionOp.eq,
                value="gaming",
            )
        ]
        return IntentBinding(
            id="b1",
            name="Gaming binding",
            conditions=conds,
            condition_mode=mode,
            intent=BUILTIN_INTENTS["gaming"],
            priority=80,
        )

    def test_is_active_true(self):
        b = self._make_binding()
        resolver = DictStateResolver()
        resolver.set(ConditionSource.activity, "state", "gaming")
        assert b.is_active(resolver)

    def test_is_active_false(self):
        b = self._make_binding()
        resolver = DictStateResolver()
        resolver.set(ConditionSource.activity, "state", "idle")
        assert not b.is_active(resolver)

    def test_is_active_all_mode_requires_all(self):
        conds = [
            BindingCondition(ConditionSource.activity, "state", ConditionOp.eq, "gaming"),
            BindingCondition(ConditionSource.device, "type", ConditionOp.eq, "workstation"),
        ]
        b = self._make_binding(mode=ConditionMode.all, conditions=conds)
        resolver = DictStateResolver()
        resolver.set(ConditionSource.activity, "state", "gaming")
        resolver.set(ConditionSource.device, "type", "server")
        assert not b.is_active(resolver)

    def test_is_active_any_mode_one_true(self):
        conds = [
            BindingCondition(ConditionSource.activity, "state", ConditionOp.eq, "gaming"),
            BindingCondition(ConditionSource.device, "type", ConditionOp.eq, "workstation"),
        ]
        b = self._make_binding(mode=ConditionMode.any, conditions=conds)
        resolver = DictStateResolver()
        resolver.set(ConditionSource.activity, "state", "gaming")
        resolver.set(ConditionSource.device, "type", "server")
        assert b.is_active(resolver)

    def test_disabled_binding_never_active(self):
        b = self._make_binding()
        b.enabled = False
        resolver = DictStateResolver()
        resolver.set(ConditionSource.activity, "state", "gaming")
        assert not b.is_active(resolver)

    def test_empty_conditions_not_active(self):
        b = self._make_binding(conditions=[])
        resolver = DictStateResolver()
        assert not b.is_active(resolver)

    def test_to_dict(self):
        b = self._make_binding()
        d = b.to_dict()
        assert d["id"] == "b1"
        assert d["intent_name"] == "gaming"
        assert d["priority"] == 80


# ── BindingRegistry ───────────────────────────────────────────────────────────

class TestBindingRegistry:
    def _binding(self, bid, priority, intent_name, field_value):
        return IntentBinding(
            id=bid,
            name=bid,
            conditions=[
                BindingCondition(
                    source=ConditionSource.activity,
                    field="state",
                    op=ConditionOp.eq,
                    value=field_value,
                )
            ],
            intent=BUILTIN_INTENTS[intent_name],
            priority=priority,
        )

    def test_add_and_list(self):
        reg = BindingRegistry()
        reg.add(self._binding("b1", 50, "desktop", "work"))
        assert len(reg.list_all()) == 1

    def test_sorted_by_priority(self):
        reg = BindingRegistry()
        reg.add(self._binding("low", 10, "preview", "x"))
        reg.add(self._binding("high", 90, "gaming", "y"))
        bindings = reg.list_all()
        assert bindings[0].id == "high"
        assert bindings[1].id == "low"

    def test_evaluate_returns_highest_priority_match(self):
        reg = BindingRegistry()
        reg.add(self._binding("low", 10, "desktop", "work"))
        reg.add(self._binding("high", 90, "gaming", "work"))

        resolver = DictStateResolver()
        resolver.set(ConditionSource.activity, "state", "work")

        result = reg.evaluate(resolver)
        assert result is not None
        binding, intent = result
        assert binding.id == "high"
        assert intent.name == "gaming"

    def test_evaluate_returns_none_when_no_match(self):
        reg = BindingRegistry()
        reg.add(self._binding("b1", 50, "desktop", "gaming"))
        resolver = DictStateResolver()
        resolver.set(ConditionSource.activity, "state", "idle")
        assert reg.evaluate(resolver) is None

    def test_remove_existing(self):
        reg = BindingRegistry()
        reg.add(self._binding("b1", 50, "desktop", "work"))
        assert reg.remove("b1")
        assert len(reg.list_all()) == 0

    def test_remove_nonexistent_returns_false(self):
        reg = BindingRegistry()
        assert not reg.remove("nope")

    def test_get_by_id(self):
        reg = BindingRegistry()
        b = self._binding("myid", 50, "desktop", "work")
        reg.add(b)
        found = reg.get("myid")
        assert found is b

    def test_get_missing_returns_none(self):
        reg = BindingRegistry()
        assert reg.get("missing") is None

    def test_evaluate_with_intent_name_resolves_builtin(self):
        binding = IntentBinding(
            id="b1",
            name="b1",
            conditions=[
                BindingCondition(
                    source=ConditionSource.activity,
                    field="state",
                    op=ConditionOp.eq,
                    value="active",
                )
            ],
            intent=None,
            intent_name="desktop",
            priority=50,
        )
        reg = BindingRegistry()
        reg.add(binding)
        resolver = DictStateResolver()
        resolver.set(ConditionSource.activity, "state", "active")
        result = reg.evaluate(resolver)
        assert result is not None
        _, intent = result
        assert intent.name == "desktop"

    def test_disabled_bindings_skipped_in_evaluate(self):
        reg = BindingRegistry()
        b = self._binding("b1", 90, "gaming", "active")
        b.enabled = False
        reg.add(b)
        resolver = DictStateResolver()
        resolver.set(ConditionSource.activity, "state", "active")
        assert reg.evaluate(resolver) is None


# ── DictStateResolver ─────────────────────────────────────────────────────────

class TestDictStateResolver:
    def test_get_set(self):
        r = DictStateResolver()
        r.set(ConditionSource.activity, "state", "gaming")
        assert r(ConditionSource.activity, "state") == "gaming"

    def test_missing_key_returns_none(self):
        r = DictStateResolver()
        assert r(ConditionSource.device, "name") is None

    def test_string_source(self):
        r = DictStateResolver()
        r.set("sensor", "temperature", 72)
        assert r(ConditionSource.sensor, "temperature") == 72
