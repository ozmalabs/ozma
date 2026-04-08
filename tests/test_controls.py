#!/usr/bin/env python3
"""
Unit tests for the control surface abstraction — ControlManager,
action routing, scenario cycling, @active resolution, feedback.

Uses mock ScenarioManager and AudioRouter.
"""

import asyncio
import sys
import unittest
from unittest.mock import AsyncMock, MagicMock, patch

sys.path.insert(0, "controller")

from controls import (
    ControlManager, ControlSurface, Control, ControlBinding, DisplayControl,
)


# ── Mock services ────────────────────────────────────────────────────────────

class MockScenario:
    def __init__(self, sid, name, node_id=None, color="#888"):
        self.id = sid
        self.name = name
        self.node_id = node_id
        self.color = color

    def to_dict(self):
        return {"id": self.id, "name": self.name, "node_id": self.node_id, "color": self.color}


class MockScenarioManager:
    def __init__(self):
        self._scenarios = {
            "s1": MockScenario("s1", "Scenario 1", "node1"),
            "s2": MockScenario("s2", "Scenario 2", "node2"),
            "s3": MockScenario("s3", "Scenario 3"),
        }
        self._active_id = "s1"
        self.activate = AsyncMock()

    @property
    def active_id(self):
        return self._active_id

    def get(self, sid):
        return self._scenarios.get(sid)

    def list(self):
        return [s.to_dict() for s in self._scenarios.values()]


class MockNodeInfo:
    def __init__(self, nid, audio_sink=None):
        self.id = nid
        self.audio_sink = audio_sink


class MockAppState:
    def __init__(self):
        self.nodes = {
            "node1": MockNodeInfo("node1", audio_sink="ozma-vm1"),
            "node2": MockNodeInfo("node2", audio_sink="ozma-vm2"),
        }
        self.active_node_id = "node1"

    def get_active_node(self):
        return self.nodes.get(self.active_node_id)


class MockPWNode:
    def __init__(self, volume=0.5, mute=False):
        self.volume = volume
        self.mute = mute


class MockWatcher:
    def __init__(self):
        self._nodes = {"ozma-vm1": MockPWNode(0.5, False)}

    def find_node(self, name):
        return self._nodes.get(name)


class MockAudioRouter:
    def __init__(self):
        self.set_volume = AsyncMock(return_value=True)
        self.set_mute = AsyncMock(return_value=True)
        self.watcher = MockWatcher()


class TestControlManager(unittest.TestCase):

    def setUp(self):
        self.state = MockAppState()
        self.scenarios = MockScenarioManager()
        self.audio = MockAudioRouter()
        self.mgr = ControlManager(self.state, self.scenarios, self.audio)

    def _run(self, coro):
        return asyncio.run(coro)

    # ── Surface registration ─────────────────────────────────────────────────

    def test_register_surface(self):
        surface = ControlSurface("test")
        self.mgr.register_surface(surface)
        self.assertIn("test", self.mgr._surfaces)

    def test_list_surfaces(self):
        s1 = ControlSurface("s1")
        s2 = ControlSurface("s2")
        self.mgr.register_surface(s1)
        self.mgr.register_surface(s2)
        result = self.mgr.list_surfaces()
        self.assertEqual(len(result), 2)

    # ── scenario.next action ─────────────────────────────────────────────────

    def test_scenario_next(self):
        surface = ControlSurface("hotkeys")
        surface.controls["next"] = Control(
            name="next", surface_id="hotkeys",
            binding=ControlBinding(action="scenario.next", value=1),
        )
        self.mgr.register_surface(surface)

        self._run(self.mgr.on_control_changed("hotkeys", "next", True))
        self.scenarios.activate.assert_called_once()
        # Should cycle from s1 to s2
        call_arg = self.scenarios.activate.call_args[0][0]
        self.assertEqual(call_arg, "s2")

    def test_scenario_prev(self):
        surface = ControlSurface("hotkeys")
        surface.controls["prev"] = Control(
            name="prev", surface_id="hotkeys",
            binding=ControlBinding(action="scenario.next", value=-1),
        )
        self.mgr.register_surface(surface)

        self._run(self.mgr.on_control_changed("hotkeys", "prev", True))
        self.scenarios.activate.assert_called_once()
        # s1 - 1 wraps to s3
        call_arg = self.scenarios.activate.call_args[0][0]
        self.assertEqual(call_arg, "s3")

    # ── scenario.activate action ─────────────────────────────────────────────

    def test_scenario_activate(self):
        surface = ControlSurface("test")
        surface.controls["btn"] = Control(
            name="btn", surface_id="test",
            binding=ControlBinding(action="scenario.activate", value="s2"),
        )
        self.mgr.register_surface(surface)

        self._run(self.mgr.on_control_changed("test", "btn", True))
        self.scenarios.activate.assert_called_once_with("s2")

    # ── audio.volume action ──────────────────────────────────────────────────

    def test_volume_with_active_target(self):
        surface = ControlSurface("test")
        surface.controls["fader"] = Control(
            name="fader", surface_id="test",
            binding=ControlBinding(action="audio.volume", target="@active"),
        )
        self.mgr.register_surface(surface)

        self._run(self.mgr.on_control_changed("test", "fader", 0.8))
        self.audio.set_volume.assert_called_once_with("ozma-vm1", 0.8)

    def test_volume_with_explicit_target(self):
        surface = ControlSurface("test")
        surface.controls["fader"] = Control(
            name="fader", surface_id="test",
            binding=ControlBinding(action="audio.volume", target="ozma-vm2"),
        )
        self.mgr.register_surface(surface)

        self._run(self.mgr.on_control_changed("test", "fader", 0.6))
        self.audio.set_volume.assert_called_once_with("ozma-vm2", 0.6)

    # ── audio.mute action (toggle) ───────────────────────────────────────────

    def test_mute_toggle(self):
        surface = ControlSurface("test")
        surface.controls["mute"] = Control(
            name="mute", surface_id="test",
            binding=ControlBinding(action="audio.mute", target="@active"),
        )
        self.mgr.register_surface(surface)

        # PW node has mute=False, toggle should set True
        self._run(self.mgr.on_control_changed("test", "mute", True))
        self.audio.set_mute.assert_called_once_with("ozma-vm1", True)

    # ── audio.volume_step action ─────────────────────────────────────────────

    def test_volume_step(self):
        surface = ControlSurface("test")
        surface.controls["up"] = Control(
            name="up", surface_id="test",
            binding=ControlBinding(action="audio.volume_step", target="@active", value=0.05),
        )
        self.mgr.register_surface(surface)

        self._run(self.mgr.on_control_changed("test", "up", True))
        # Should set volume to 0.5 + 0.05 = 0.55
        self.audio.set_volume.assert_called_once()
        actual_vol = self.audio.set_volume.call_args[0][1]
        self.assertAlmostEqual(actual_vol, 0.55)

    # ── @active resolution ───────────────────────────────────────────────────

    def test_active_resolves_to_audio_sink(self):
        result = self.mgr._resolve_target("@active")
        self.assertEqual(result, "ozma-vm1")

    def test_active_returns_none_when_no_active(self):
        self.state.active_node_id = None
        result = self.mgr._resolve_target("@active")
        self.assertIsNone(result)

    def test_explicit_target_passthrough(self):
        result = self.mgr._resolve_target("some-node")
        self.assertEqual(result, "some-node")

    # ── Value transforms ─────────────────────────────────────────────────────

    def test_to_target_transform(self):
        surface = ControlSurface("test")
        surface.controls["fader"] = Control(
            name="fader", surface_id="test",
            binding=ControlBinding(
                action="audio.volume", target="ozma-vm1",
                to_target=lambda v: v / 127.0,
            ),
        )
        self.mgr.register_surface(surface)

        self._run(self.mgr.on_control_changed("test", "fader", 127))
        self.audio.set_volume.assert_called_once_with("ozma-vm1", 1.0)

    # ── No binding → no action ───────────────────────────────────────────────

    def test_no_binding_ignored(self):
        surface = ControlSurface("test")
        surface.controls["btn"] = Control(name="btn", surface_id="test")
        self.mgr.register_surface(surface)

        # Should not raise
        self._run(self.mgr.on_control_changed("test", "btn", True))

    # ── Unknown surface ignored ──────────────────────────────────────────────

    def test_unknown_surface_ignored(self):
        self._run(self.mgr.on_control_changed("nonexistent", "btn", True))

    # ── Display updates ──────────────────────────────────────────────────────

    def test_update_active_displays(self):
        surface = ControlSurface("test")
        updated = []
        display = DisplayControl(
            name="lcd", surface_id="test",
            binding="@active.name",
        )
        display.on_update = lambda text, color: updated.append((text, color))
        surface.displays["lcd"] = display
        self.mgr.register_surface(surface)

        self._run(self.mgr._update_active_displays())
        self.assertEqual(len(updated), 1)
        self.assertEqual(updated[0][0], "Scenario 1")

    # ── Feedback on scenario change ──────────────────────────────────────────

    def test_scenario_feedback_to_buttons(self):
        surface = ControlSurface("test")
        feedback_values = []
        ctrl = Control(
            name="btn_s1", surface_id="test",
            binding=ControlBinding(action="scenario.activate", target="s1"),
        )
        ctrl.on_feedback = lambda v: feedback_values.append(v)
        surface.controls["btn_s1"] = ctrl
        self.mgr.register_surface(surface)

        self._run(self.mgr.on_scenario_changed("s1"))
        self.assertEqual(feedback_values, [True])


if __name__ == "__main__":
    unittest.main()
