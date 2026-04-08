# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Intent bindings — condition-driven automatic intent selection.

Implements the intent binding model from docs/routing/routing.md §Intent Bindings.

A binding maps a set of conditions on app state to an intent. When all (or any)
conditions are true, the binding's intent becomes the candidate for the affected
streams. Multiple bindings are ranked by priority; the highest-priority match wins.
"""

from __future__ import annotations

import re
from dataclasses import dataclass, field
from enum import Enum
from typing import Any

from .intent import Intent
from .model import MediaType


# ── Condition operator ────────────────────────────────────────────────────────

class ConditionOp(str, Enum):
    eq = "eq"
    neq = "neq"
    gt = "gt"
    lt = "lt"
    in_ = "in"          # JSON key is "in"; Python attribute is in_
    contains = "contains"
    matches = "matches"  # regex

    @classmethod
    def from_str(cls, s: str) -> "ConditionOp":
        if s == "in":
            return cls.in_
        return cls(s)

    def to_str(self) -> str:
        if self == ConditionOp.in_:
            return "in"
        return self.value


# ── Condition sources ─────────────────────────────────────────────────────────

class ConditionSource(str, Enum):
    activity = "activity"     # node/machine activity state
    device = "device"         # device property
    sensor = "sensor"         # sensor reading
    time = "time"             # wall clock / schedule
    power = "power"           # power state
    link = "link"             # link status
    presence = "presence"     # user/occupancy presence
    calendar = "calendar"     # calendar event
    input = "input"           # active input source


# ── BindingCondition ──────────────────────────────────────────────────────────

@dataclass
class BindingCondition:
    """
    A single predicate on app state.

    source: what kind of thing we are checking (activity, device, sensor, …)
    field:  the specific property path within that source
    op:     comparison operator
    value:  the RHS value to compare against
    """
    source: ConditionSource
    field: str
    op: ConditionOp
    value: Any

    def to_dict(self) -> dict:
        return {
            "source": self.source.value,
            "field": self.field,
            "op": self.op.to_str(),
            "value": self.value,
        }

    @classmethod
    def from_dict(cls, d: dict) -> "BindingCondition":
        return cls(
            source=ConditionSource(d["source"]),
            field=d["field"],
            op=ConditionOp.from_str(d["op"]),
            value=d["value"],
        )

    def evaluate(self, actual: Any) -> bool:
        """
        Evaluate this condition against *actual* (the resolved value from
        app state for self.source + self.field).

        Returns True if the condition is satisfied.
        """
        if actual is None:
            return False
        op = self.op
        v = self.value
        try:
            if op == ConditionOp.eq:
                return actual == v
            if op == ConditionOp.neq:
                return actual != v
            if op == ConditionOp.gt:
                return actual > v
            if op == ConditionOp.lt:
                return actual < v
            if op == ConditionOp.in_:
                return actual in v
            if op == ConditionOp.contains:
                return v in actual
            if op == ConditionOp.matches:
                return bool(re.fullmatch(str(v), str(actual)))
        except (TypeError, ValueError):
            return False
        return False


# ── Condition mode ────────────────────────────────────────────────────────────

class ConditionMode(str, Enum):
    all = "all"   # all conditions must be true (AND)
    any = "any"   # at least one must be true (OR)


# ── BindingScope ──────────────────────────────────────────────────────────────

@dataclass
class BindingScope:
    """
    Which part of the system this binding targets.

    target: "node", "controller", or a specific device type string
    target_id: specific device/node ID, or None for "all matching targets"
    streams: limit to specific media types, or empty for "all streams"
    """
    target: str = "node"
    target_id: str | None = None
    streams: list[MediaType] = field(default_factory=list)

    def to_dict(self) -> dict:
        return {
            "target": self.target,
            "target_id": self.target_id,
            "streams": [s.value for s in self.streams],
        }

    @classmethod
    def from_dict(cls, d: dict) -> "BindingScope":
        return cls(
            target=d.get("target", "node"),
            target_id=d.get("target_id"),
            streams=[MediaType(s) for s in d.get("streams", [])],
        )


# ── RevertPolicy ──────────────────────────────────────────────────────────────

class RevertMode(str, Enum):
    revert = "revert"     # return to previous intent when condition clears
    hold = "hold"         # keep intent even after condition clears
    timeout = "timeout"   # keep for timeout_ms then revert


@dataclass
class RevertPolicy:
    mode: RevertMode = RevertMode.revert
    timeout_ms: int | None = None

    def to_dict(self) -> dict:
        return {
            "mode": self.mode.value,
            "timeout_ms": self.timeout_ms,
        }

    @classmethod
    def from_dict(cls, d: dict) -> "RevertPolicy":
        return cls(
            mode=RevertMode(d.get("mode", "revert")),
            timeout_ms=d.get("timeout_ms"),
        )


# ── IntentBinding ─────────────────────────────────────────────────────────────

@dataclass
class IntentBinding:
    """
    Maps a set of conditions on app state to an Intent.

    When conditions are satisfied, the binding's intent replaces the active
    intent for the binding's scope. Multiple bindings are evaluated in priority
    order (highest first); the first match wins.
    """
    id: str
    name: str
    conditions: list[BindingCondition] = field(default_factory=list)
    condition_mode: ConditionMode = ConditionMode.all
    intent: Intent | None = None
    intent_name: str = ""      # used when intent is a reference to a built-in
    scope: BindingScope = field(default_factory=BindingScope)
    revert: RevertPolicy = field(default_factory=RevertPolicy)
    priority: int = 50
    enabled: bool = True

    def to_dict(self) -> dict:
        return {
            "id": self.id,
            "name": self.name,
            "conditions": [c.to_dict() for c in self.conditions],
            "condition_mode": self.condition_mode.value,
            "intent_name": self.intent_name or (self.intent.name if self.intent else ""),
            "scope": self.scope.to_dict(),
            "revert": self.revert.to_dict(),
            "priority": self.priority,
            "enabled": self.enabled,
        }

    def is_active(self, state_resolver: "StateResolver") -> bool:
        """
        Evaluate all conditions against the provided resolver.

        state_resolver is a callable: (source, field) -> Any
        Returns True if the binding's conditions are met.
        """
        if not self.enabled or not self.conditions:
            return False
        results = [
            c.evaluate(state_resolver(c.source, c.field))
            for c in self.conditions
        ]
        if self.condition_mode == ConditionMode.all:
            return all(results)
        return any(results)


# ── StateResolver protocol ────────────────────────────────────────────────────

class StateResolver:
    """
    Abstract resolver that maps (source, field) → current value.

    Concrete implementations query AppState, NodeInfo, sensor readings, etc.
    The default implementation always returns None (no-op, all conditions fail).
    """

    def __call__(self, source: ConditionSource, field: str) -> Any:
        return None


class DictStateResolver(StateResolver):
    """
    Simple resolver backed by a flat dict keyed by "{source}.{field}".

    Useful for tests and simple scenarios.
    """

    def __init__(self, state: dict[str, Any] | None = None):
        self._state: dict[str, Any] = state or {}

    def __call__(self, source: ConditionSource, field: str) -> Any:
        key = f"{source.value}.{field}"
        return self._state.get(key)

    def set(self, source: ConditionSource | str, field: str, value: Any) -> None:
        src = source.value if isinstance(source, ConditionSource) else source
        self._state[f"{src}.{field}"] = value


# ── BindingRegistry ───────────────────────────────────────────────────────────

class BindingRegistry:
    """
    Ordered collection of IntentBindings.

    evaluate() iterates bindings in priority order and returns the first
    active binding (and its resolved intent), or None if none match.
    """

    def __init__(self) -> None:
        self._bindings: list[IntentBinding] = []

    def add(self, binding: IntentBinding) -> None:
        self._bindings.append(binding)
        self._bindings.sort(key=lambda b: b.priority, reverse=True)

    def remove(self, binding_id: str) -> bool:
        before = len(self._bindings)
        self._bindings = [b for b in self._bindings if b.id != binding_id]
        return len(self._bindings) < before

    def get(self, binding_id: str) -> IntentBinding | None:
        for b in self._bindings:
            if b.id == binding_id:
                return b
        return None

    def list_all(self) -> list[IntentBinding]:
        return list(self._bindings)

    def evaluate(
        self,
        resolver: StateResolver,
        intent_lookup: dict[str, Intent] | None = None,
    ) -> tuple[IntentBinding, Intent] | None:
        """
        Find the highest-priority active binding.

        intent_lookup: optional dict of name→Intent for resolving intent_name
        references. Falls back to BUILTIN_INTENTS if not found there.

        Returns (binding, intent) or None if no binding is active.
        """
        from .intent import BUILTIN_INTENTS
        lookup = {**BUILTIN_INTENTS, **(intent_lookup or {})}

        for binding in self._bindings:
            if not binding.enabled:
                continue
            if binding.is_active(resolver):
                intent = binding.intent
                if intent is None and binding.intent_name:
                    intent = lookup.get(binding.intent_name)
                if intent is not None:
                    return binding, intent
        return None
