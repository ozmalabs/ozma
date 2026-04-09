# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
BindingRegistry evaluation loop — Phase 2 runtime component.

Runs as a background asyncio task. Periodically evaluates all registered
IntentBindings against current app state and emits journal events when
bindings become active or inactive.

The loop does NOT directly activate pipelines — that is Phase 3+. It tracks
the currently winning binding and exposes it so the API and future pipeline
activation code can read it.

State transitions:
  None → binding    : binding becomes active  → intent_bound journal entry
  binding → None    : binding clears          → intent_unbound journal entry
  binding_A → binding_B : priority switch    → intent_unbound (A) + intent_bound (B)
"""

from __future__ import annotations

import asyncio
import logging
import time
from typing import TYPE_CHECKING, Any

from .binding import BindingCondition, BindingRegistry, ConditionSource, StateResolver

if TYPE_CHECKING:
    from .binding import IntentBinding
    from .intent import Intent
    from .monitoring import MonitoringJournal

log = logging.getLogger("ozma.routing.binding_loop")

# How often to re-evaluate bindings (seconds)
_EVAL_INTERVAL_S = 5.0


# ── AppState resolver ─────────────────────────────────────────────────────────

class AppStateResolver(StateResolver):
    """
    Resolves binding condition values from live AppState.

    Supported sources and fields:
      activity.active_node_id        → str | None
      activity.node_count            → int
      activity.node.{node_id}.online → bool
      link.{link_id}.status          → str (LinkStatus value)
      link.{link_id}.latency_ms      → float | None (from MeasurementStore)
      device.{device_id}.{property}  → Any (from routing graph device.properties)
    """

    def __init__(self, state: Any) -> None:
        self._state = state

    def __call__(self, source: ConditionSource, field: str) -> Any:
        try:
            return self._resolve(source, field)
        except Exception as exc:
            log.debug("AppStateResolver failed for %s.%s: %s", source, field, exc)
            return None

    def _resolve(self, source: ConditionSource, field: str) -> Any:
        s = self._state

        if source == ConditionSource.activity:
            return self._resolve_activity(field)

        if source == ConditionSource.link:
            return self._resolve_link(field)

        if source == ConditionSource.device:
            return self._resolve_device(field)

        if source == ConditionSource.sensor:
            # Phase 5+: sensor readings from MeasurementStore
            store = getattr(s, "measurement_store", None)
            if store is None:
                return None
            # field format: "device_id/metric_key"
            parts = field.split("/", 1)
            if len(parts) != 2:
                return None
            qv = store.get(parts[0], parts[1], apply_decay=True)
            return qv.value if qv else None

        return None

    def _resolve_activity(self, field: str) -> Any:
        s = self._state
        if field == "active_node_id":
            return getattr(s, "active_node_id", None)
        if field == "node_count":
            return len(getattr(s, "nodes", {}))
        # "node.{node_id}.online"
        if field.startswith("node.") and field.endswith(".online"):
            node_id = field[5:-7]  # strip "node." prefix and ".online" suffix
            return node_id in getattr(s, "nodes", {})
        return None

    def _resolve_link(self, field: str) -> Any:
        # field: "{link_id}.status" or "{link_id}.latency_ms"
        parts = field.rsplit(".", 1)
        if len(parts) != 2:
            return None
        link_id, prop = parts
        graph = getattr(self._state, "routing_graph", None)
        if graph is None:
            return None
        link = graph.get_link(link_id)
        if link is None:
            return None
        if prop == "status":
            return link.state.status.value if link.state else None
        if prop == "latency_ms":
            store = getattr(self._state, "measurement_store", None)
            if store is None:
                return None
            dev_id = link.source.device_id
            qv = store.get(dev_id, f"link.{link_id}.latency_ms", apply_decay=True)
            return qv.value if qv else None
        if prop == "loss_rate":
            store = getattr(self._state, "measurement_store", None)
            if store is None:
                return None
            dev_id = link.source.device_id
            qv = store.get(dev_id, f"link.{link_id}.loss_rate", apply_decay=True)
            return qv.value if qv else None
        return None

    def _resolve_device(self, field: str) -> Any:
        # field: "{device_id}.{property}"
        parts = field.split(".", 1)
        if len(parts) != 2:
            return None
        device_id, prop = parts
        graph = getattr(self._state, "routing_graph", None)
        if graph is None:
            return None
        device = graph.get_device(device_id)
        if device is None:
            return None
        return device.properties.get(prop)


# ── Evaluation result ─────────────────────────────────────────────────────────

class EvaluationResult:
    """Snapshot of one evaluation cycle."""

    def __init__(
        self,
        binding: "IntentBinding | None",
        intent: "Intent | None",
        evaluated_at: float,
        elapsed_ms: float,
        total_bindings: int,
    ) -> None:
        self.binding = binding
        self.intent = intent
        self.evaluated_at = evaluated_at
        self.elapsed_ms = elapsed_ms
        self.total_bindings = total_bindings

    def to_dict(self) -> dict:
        return {
            "binding_id": self.binding.id if self.binding else None,
            "binding_name": self.binding.name if self.binding else None,
            "intent_name": self.intent.name if self.intent else None,
            "evaluated_at": self.evaluated_at,
            "elapsed_ms": round(self.elapsed_ms, 2),
            "total_bindings": self.total_bindings,
        }


# ── Binding loop ──────────────────────────────────────────────────────────────

class BindingLoop:
    """
    Background asyncio task that periodically evaluates the BindingRegistry
    and emits journal events on state transitions.

    The active result is exposed via `current` for API and pipeline code.
    """

    def __init__(
        self,
        registry: BindingRegistry,
        resolver: StateResolver,
        journal: "MonitoringJournal | None" = None,
        interval_s: float = _EVAL_INTERVAL_S,
    ) -> None:
        self._registry = registry
        self._resolver = resolver
        self._journal = journal
        self._interval_s = interval_s
        self._task: asyncio.Task | None = None
        self._running = False
        # Last evaluation result
        self._current: EvaluationResult | None = None
        # ID of the previously winning binding (for transition detection)
        self._prev_binding_id: str | None = None

    async def start(self) -> None:
        """Start the background evaluation loop."""
        if self._task is not None:
            return
        self._running = True
        self._task = asyncio.create_task(
            self._run(), name="routing.binding_loop"
        )
        log.info("Binding evaluation loop started (interval=%.1fs)", self._interval_s)

    async def stop(self) -> None:
        """Stop the loop and wait for it to exit."""
        self._running = False
        if self._task is not None:
            self._task.cancel()
            try:
                await self._task
            except asyncio.CancelledError:
                pass
            self._task = None
        log.info("Binding evaluation loop stopped")

    def evaluate_once(self) -> EvaluationResult:
        """
        Run one evaluation cycle synchronously and return the result.

        Also updates `self._current` and emits journal events.
        """
        t0 = time.monotonic()
        match = self._registry.evaluate(self._resolver)
        elapsed_ms = (time.monotonic() - t0) * 1000.0

        binding = match[0] if match else None
        intent = match[1] if match else None

        result = EvaluationResult(
            binding=binding,
            intent=intent,
            evaluated_at=t0,
            elapsed_ms=elapsed_ms,
            total_bindings=len(self._registry.list_all()),
        )
        self._current = result
        self._emit_transitions(binding)
        return result

    @property
    def current(self) -> EvaluationResult | None:
        """The most recent evaluation result, or None if not yet evaluated."""
        return self._current

    def to_dict(self) -> dict:
        return {
            "running": self._running,
            "task_active": self._task is not None and not self._task.done(),
            "interval_s": self._interval_s,
            "current": self._current.to_dict() if self._current else None,
        }

    # ── Internal ──────────────────────────────────────────────────────────────

    async def _run(self) -> None:
        """Main evaluation loop."""
        while self._running:
            try:
                self.evaluate_once()
            except Exception as exc:
                log.warning("BindingLoop evaluation error: %s", exc, exc_info=True)
            await asyncio.sleep(self._interval_s)

    def _emit_transitions(self, new_binding: "IntentBinding | None") -> None:
        """Emit journal events when the active binding changes."""
        new_id = new_binding.id if new_binding else None
        old_id = self._prev_binding_id

        if new_id == old_id:
            return  # no change

        if self._journal is not None:
            from .monitoring import StateChangeRecord, StateChangeType

            # Emit unbound for the old binding
            if old_id is not None:
                self._journal.append(StateChangeRecord(
                    type=StateChangeType.intent_unbound,
                    device_id="controller",
                    message=f"Binding {old_id!r} deactivated",
                    source="binding_loop",
                    severity="info",
                ))

            # Emit bound for the new binding
            if new_binding is not None:
                intent_name = (
                    new_binding.intent.name if new_binding.intent else new_binding.intent_name
                )
                self._journal.append(StateChangeRecord(
                    type=StateChangeType.intent_bound,
                    device_id="controller",
                    message=(
                        f"Binding {new_binding.id!r} activated "
                        f"(intent={intent_name!r}, priority={new_binding.priority})"
                    ),
                    source="binding_loop",
                    severity="info",
                ))

        self._prev_binding_id = new_id
        log.info(
            "Binding transition: %r → %r",
            old_id, new_id,
        )
