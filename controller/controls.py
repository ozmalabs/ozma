# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Control surface abstraction — bridges physical devices (MIDI controllers,
hotkeys, web UI) to ozma actions (scenario switching, volume, mute).

Architecture:

    Physical Device          Control Surface           Ozma Action
    ─────────────────       ─────────────────         ─────────────────
    MIDI fader        ─┐    ┌─ FaderControl ────────── audio.volume
    Web UI slider     ─┤    │
                       ├──► ├─ ButtonControl ───────── scenario.activate
    MIDI button       ─┤    │
    Hotkey            ─┘    ├─ CycleControl ────────── scenario.next
                            │
                            └─ DisplayControl ──────── Scribble / WebUI label

Each ControlSurface has named controls.  Each control can be bound to an
action with optional value translation.  The ControlManager routes changes
bidirectionally: physical→action and action→physical (for motorised faders,
LEDs, displays).

Inspired by surfacepresser-run's Linker + virtual device system.
"""

from __future__ import annotations

import asyncio
import logging
import uuid
from dataclasses import dataclass, field
from typing import Any, Callable, TYPE_CHECKING

if TYPE_CHECKING:
    from scenarios import ScenarioManager
    from audio import AudioRouter
    from state import AppState
    from motion import MotionManager

log = logging.getLogger("ozma.controls")


# ── Value types ──────────────────────────────────────────────────────────────

@dataclass
class ControlBinding:
    """Maps a control to an ozma action."""

    action: str          # "scenario.activate", "scenario.next", "audio.volume", "audio.mute"
    target: str = ""     # Scenario ID, PW node name, or "@active" (follows active node)
    value: Any = None    # Fixed value for buttons (e.g. scenario_id, +1/-1)
    to_target: Callable[[Any], Any] | None = None     # e.g. MIDI 0-127 → float 0-1
    from_target: Callable[[Any], Any] | None = None   # e.g. float 0-1 → MIDI 0-127


@dataclass
class Control:
    """A named control on a surface with optional binding."""

    name: str
    surface_id: str
    binding: ControlBinding | None = None
    value: Any = None            # Current value
    lockout: bool = False        # True while user is physically touching (prevents feedback)

    # Called by ControlManager when the bound action's value changes externally.
    # The surface implementation overrides this to update hardware (e.g. move fader).
    on_feedback: Callable[[Any], None] | None = None


@dataclass
class DisplayControl:
    """A named display element on a surface."""

    name: str
    surface_id: str
    binding: str = ""            # What to display: "@active.name", "@active.color", etc.
    value: str = ""

    # Called when display content should update.
    on_update: Callable[[str, str | None], None] | None = None  # (text, color)


@dataclass
class EventTriggerRule:
    """Fires an ozma action when a broadcast event matches type and optional filters.

    Example — switch to Matt's workstation when face recognised at front door:
        EventTriggerRule(
            event_type="frigate.person_recognized",
            filters={"person": "Matt", "camera": "front_door"},
            action="scenario.activate",
            value="matt-workstation",
        )

    Filters are AND-matched: every key in ``filters`` must equal the corresponding
    value in the event dict.  Omit ``filters`` to match all events of that type.
    """

    event_type: str                                  # e.g. "frigate.person_recognized"
    action: str                                      # e.g. "scenario.activate"
    rule_id: str = field(default_factory=lambda: uuid.uuid4().hex[:8])
    filters: dict[str, Any] = field(default_factory=dict)
    target: str = ""
    value: Any = None                                # fixed value; None → pass event data


class ControlSurface:
    """
    A physical or virtual device with named controls and displays.

    Subclassed by MidiSurface, HotkeySurface, etc.
    """

    def __init__(self, surface_id: str) -> None:
        self.id = surface_id
        self.controls: dict[str, Control] = {}
        self.displays: dict[str, DisplayControl] = {}

    async def start(self) -> None:
        """Start the surface (open device, etc.)."""

    async def stop(self) -> None:
        """Stop the surface (close device, etc.)."""

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id,
            "controls": {
                name: {
                    "value": c.value,
                    "binding": c.binding.action if c.binding else None,
                    "target": c.binding.target if c.binding else None,
                }
                for name, c in self.controls.items()
            },
            "displays": {
                name: {"value": d.value, "binding": d.binding}
                for name, d in self.displays.items()
            },
        }


# ── Control Manager ──────────────────────────────────────────────────────────

class ControlManager:
    """
    Central registry for control surfaces.  Routes control changes to ozma
    actions and routes action feedback back to surfaces.
    """

    def __init__(
        self,
        state: "AppState",
        scenarios: "ScenarioManager",
        audio: "AudioRouter | None" = None,
        motion: "MotionManager | None" = None,
        doorbell: "Any | None" = None,
        alerts: "Any | None" = None,
    ) -> None:
        self._state = state
        self._scenarios = scenarios
        self._audio = audio
        self._motion = motion
        self._doorbell = doorbell
        self._alerts = alerts
        self._surfaces: dict[str, ControlSurface] = {}
        self._action_lock = asyncio.Lock()
        self._trigger_rules: list[EventTriggerRule] = []

    def register_surface(self, surface: ControlSurface) -> None:
        self._surfaces[surface.id] = surface
        log.info("Control surface registered: %s (%d controls, %d displays)",
                 surface.id, len(surface.controls), len(surface.displays))

    async def start(self) -> None:
        for surface in self._surfaces.values():
            await surface.start()

    async def stop(self) -> None:
        for surface in self._surfaces.values():
            await surface.stop()

    # ── Inbound: physical control changed ────────────────────────────────────

    async def on_control_changed(
        self, surface_id: str, control_name: str, value: Any
    ) -> None:
        """Called when a physical control changes (button press, fader move, etc.)."""
        surface = self._surfaces.get(surface_id)
        if not surface:
            return
        control = surface.controls.get(control_name)
        if not control or not control.binding:
            return

        control.value = value
        binding = control.binding

        # Transform value if needed
        action_value = binding.to_target(value) if binding.to_target else value
        # For buttons with a fixed value, use that instead
        if binding.value is not None:
            action_value = binding.value

        async with self._action_lock:
            await self._execute_action(binding.action, binding.target, action_value)

    async def _execute_action(self, action: str, target: str, value: Any) -> None:
        """Execute an ozma action."""
        match action:
            case "scenario.activate":
                scenario_id = str(value) if value else target
                try:
                    await self._scenarios.activate(scenario_id)
                except KeyError:
                    log.warning("scenario.activate: unknown scenario %r", scenario_id)

            case "scenario.next":
                delta = int(value) if value else 1
                await self._cycle_scenario(delta)

            case "audio.volume":
                if not self._audio:
                    return
                node_name = self._resolve_target(target)
                if node_name:
                    await self._audio.set_volume(node_name, float(value))

            case "audio.mute":
                if not self._audio:
                    return
                node_name = self._resolve_target(target)
                if node_name:
                    # If value is True (toggle), read current state and flip it
                    pw_node = self._audio.watcher.find_node(node_name) if self._audio else None
                    if value is True and pw_node:
                        await self._audio.set_mute(node_name, not pw_node.mute)
                    else:
                        await self._audio.set_mute(node_name, bool(value))

            case "audio.volume_step":
                # Increment/decrement volume by a step (e.g. +0.05 or -0.05)
                if not self._audio:
                    return
                node_name = self._resolve_target(target)
                if node_name:
                    pw_node = self._audio.watcher.find_node(node_name)
                    if pw_node:
                        new_vol = max(0.0, min(1.5, pw_node.volume + float(value)))
                        await self._audio.set_volume(node_name, new_vol)

            case "motion.move":
                # target format: "device_id:axis" e.g. "crane:pan"
                if not self._motion:
                    return
                parts = target.split(":", 1)
                if len(parts) == 2:
                    await self._motion.move(parts[0], parts[1], float(value))

            case "motion.stop":
                if not self._motion:
                    return
                parts = target.split(":", 1)
                device_id = parts[0] if parts else ""
                axis = parts[1] if len(parts) > 1 else None
                if device_id:
                    await self._motion.stop_axis(device_id, axis)

            case "motion.preset":
                if not self._motion:
                    return
                # target = device_id, value = preset_name
                await self._motion.go_to_preset(target, str(value))

            case "power.on" | "power.off" | "power.reset" | "power.cycle" | "power.force-off":
                power_action = action.split(".", 1)[1]
                node_name = self._resolve_target(target)
                if node_name:
                    await self._proxy_power(node_name, power_action)

            case "alert.acknowledge":
                # Acknowledge an alert (primary action — Answer for doorbell, OK for timer).
                # target or value = alert_id; empty = most recent active alert.
                if self._alerts:
                    await self._alerts.acknowledge(str(value or target or ""))
                elif self._doorbell:
                    # Compat: fall through to doorbell if no alert manager
                    await self._doorbell.answer(str(value or target or ""))

            case "alert.dismiss":
                # Dismiss an alert (secondary action).
                if self._alerts:
                    await self._alerts.dismiss(str(value or target or ""))
                elif self._doorbell:
                    await self._doorbell.dismiss(str(value or target or ""))

            # Kept for backwards compatibility with existing control bindings
            case "doorbell.answer":
                if self._alerts:
                    await self._alerts.acknowledge(str(value or target or ""))
                elif self._doorbell:
                    await self._doorbell.answer(str(value or target or ""))

            case "doorbell.dismiss":
                if self._alerts:
                    await self._alerts.dismiss(str(value or target or ""))
                elif self._doorbell:
                    await self._doorbell.dismiss(str(value or target or ""))

            case _:
                log.debug("Unknown action: %s", action)

    async def _cycle_scenario(self, delta: int) -> None:
        """Cycle through scenarios by delta (+1 = next, -1 = prev)."""
        ids = list(self._scenarios._scenarios.keys())
        if not ids:
            return
        current = self._scenarios.active_id
        if current in ids:
            idx = (ids.index(current) + delta) % len(ids)
        else:
            idx = 0
        next_id = ids[idx]
        try:
            await self._scenarios.activate(next_id)
        except KeyError:
            pass

        # Update all displays bound to @active.*
        await self._update_active_displays()

    async def _proxy_power(self, node_name: str, action: str) -> None:
        """Proxy a power action to a node via its HTTP API."""
        import urllib.request
        # Find node by audio sink name or node ID
        node = None
        for n in self._state.nodes.values():
            if n.audio_sink == node_name or n.id == node_name or n.id.split(".")[0] == node_name:
                node = n
                break
        if not node or not node.api_port:
            log.debug("power.%s: node %r not found or no API port", action, node_name)
            return
        url = f"http://{node.host}:{node.api_port}/power/{action}"
        try:
            loop = asyncio.get_running_loop()
            req = urllib.request.Request(url, data=b"{}", headers={"Content-Type": "application/json"}, method="POST")
            await loop.run_in_executor(None, lambda: urllib.request.urlopen(req, timeout=10))
            log.info("Power %s → %s", action, node.id)
        except Exception as e:
            log.warning("Power %s → %s failed: %s", action, node.id, e)

    def _resolve_target(self, target: str) -> str | None:
        """Resolve @active to the active node's audio source name."""
        if target == "@active":
            active = self._state.get_active_node()
            if active and active.audio_sink:
                return active.audio_sink
            return None
        return target

    # ── Outbound: action state changed → update surfaces ─────────────────────

    async def on_scenario_changed(self, scenario_id: str) -> None:
        """Called when the active scenario changes (from any source)."""
        await self._update_active_displays()

        # Send feedback to controls bound to scenario.activate
        for surface in self._surfaces.values():
            for control in surface.controls.values():
                if not control.binding:
                    continue
                if control.binding.action == "scenario.activate":
                    is_active = (control.binding.target == scenario_id or
                                 control.binding.value == scenario_id)
                    if control.on_feedback:
                        control.on_feedback(is_active)

    async def on_volume_changed(self, node_name: str, volume: float) -> None:
        """Called when a PW node's volume changes (from PipeWireWatcher)."""
        for surface in self._surfaces.values():
            for control in surface.controls.values():
                if not control.binding or control.lockout:
                    continue
                if control.binding.action != "audio.volume":
                    continue
                resolved = self._resolve_target(control.binding.target)
                if resolved == node_name:
                    feedback_val = (control.binding.from_target(volume)
                                    if control.binding.from_target else volume)
                    if control.on_feedback:
                        control.on_feedback(feedback_val)

    async def _update_active_displays(self) -> None:
        """Update all displays bound to @active.* with current scenario info."""
        scenario = self._scenarios.get(self._scenarios.active_id or "")
        for surface in self._surfaces.values():
            for display in surface.displays.values():
                if not display.binding.startswith("@active"):
                    continue
                if not scenario:
                    continue
                text = ""
                color = None
                match display.binding:
                    case "@active.name":
                        text = scenario.name
                        color = scenario.color
                    case "@active.id":
                        text = scenario.id
                    case "@active.color":
                        text = scenario.color
                    case "@active.node":
                        text = (scenario.node_id or "unbound").split(".")[0]
                display.value = text
                if display.on_update:
                    display.on_update(text, color)

    # ── Event trigger rules ──────────────────────────────────────────────────

    def add_trigger_rule(self, rule: EventTriggerRule) -> str:
        """Register a trigger rule. Returns the rule_id."""
        self._trigger_rules.append(rule)
        log.info("Trigger rule added: %s → %s (filters=%s)", rule.event_type, rule.action, rule.filters)
        return rule.rule_id

    def remove_trigger_rule(self, rule_id: str) -> bool:
        before = len(self._trigger_rules)
        self._trigger_rules = [r for r in self._trigger_rules if r.rule_id != rule_id]
        return len(self._trigger_rules) < before

    def list_trigger_rules(self) -> list[dict[str, Any]]:
        return [
            {
                "rule_id": r.rule_id,
                "event_type": r.event_type,
                "filters": r.filters,
                "action": r.action,
                "target": r.target,
                "value": r.value,
            }
            for r in self._trigger_rules
        ]

    async def on_event(self, event_type: str, data: dict) -> None:
        """Called for every event broadcast; fires matching trigger rules."""
        for rule in self._trigger_rules:
            if rule.event_type != event_type:
                continue
            if not all(data.get(k) == v for k, v in rule.filters.items()):
                continue
            action_value = rule.value if rule.value is not None else data
            log.debug("Trigger rule %s fired: %s → %s", rule.rule_id, event_type, rule.action)
            async with self._action_lock:
                await self._execute_action(rule.action, rule.target, action_value)

    # ── Surface enumeration for API ──────────────────────────────────────────

    def list_surfaces(self) -> list[dict[str, Any]]:
        return [s.to_dict() for s in self._surfaces.values()]
