"""Unit tests for the AI agent engine — dispatch, keycodes, SoM, ActionResult."""

import pytest
from unittest.mock import MagicMock, AsyncMock

pytestmark = pytest.mark.unit


class TestNamedKeys:
    def test_common_keys_present(self):
        from agent_engine import _NAMED_KEYS
        for key in ("enter", "return", "esc", "escape", "tab", "space",
                     "backspace", "delete", "up", "down", "left", "right",
                     "home", "end", "pageup", "pagedown"):
            assert key in _NAMED_KEYS, f"Missing named key: {key}"

    def test_function_keys(self):
        from agent_engine import _NAMED_KEYS
        for i in range(1, 13):
            assert f"f{i}" in _NAMED_KEYS

    def test_values_are_ints(self):
        from agent_engine import _NAMED_KEYS
        for key, code in _NAMED_KEYS.items():
            assert isinstance(code, int), f"{key} maps to {type(code)}"


class TestCharToHID:
    def test_lowercase_letters(self):
        from agent_engine import _CHAR_TO_HID
        for c in "abcdefghijklmnopqrstuvwxyz":
            assert c in _CHAR_TO_HID, f"Missing char: {c}"
            kc, mod = _CHAR_TO_HID[c]
            assert mod == 0, f"'{c}' should not require modifier"

    def test_digits(self):
        from agent_engine import _CHAR_TO_HID
        for c in "1234567890":
            assert c in _CHAR_TO_HID

    def test_common_punctuation(self):
        from agent_engine import _CHAR_TO_HID
        for c in " -=[]\\;',./":
            assert c in _CHAR_TO_HID, f"Missing punctuation: {repr(c)}"

    def test_newline_and_tab(self):
        from agent_engine import _CHAR_TO_HID
        assert "\n" in _CHAR_TO_HID
        assert "\t" in _CHAR_TO_HID


class TestShiftChars:
    def test_all_shifted_mapped(self):
        from agent_engine import _SHIFT_CHARS
        expected = set('!@#$%^&*()_+{}|:"~<>?')
        assert expected == set(_SHIFT_CHARS.keys())

    def test_values_are_keycodes(self):
        from agent_engine import _SHIFT_CHARS
        for ch, kc in _SHIFT_CHARS.items():
            assert isinstance(kc, int), f"Shift char {repr(ch)} maps to non-int"


class TestReadOnlyActions:
    def test_read_actions_are_read_only(self):
        from agent_engine import _READ_ONLY_ACTIONS
        for action in ("screenshot", "read_screen", "find_elements",
                        "assert_text", "assert_element"):
            assert action in _READ_ONLY_ACTIONS

    def test_write_actions_not_read_only(self):
        from agent_engine import _READ_ONLY_ACTIONS
        for action in ("click", "type", "key", "hotkey", "mouse_move", "scroll"):
            assert action not in _READ_ONLY_ACTIONS


class TestActionResult:
    def test_defaults(self):
        from agent_engine import ActionResult
        r = ActionResult()
        assert r.success is True
        assert r.error is None
        assert r.screenshot_base64 == ""
        assert r.elements == []

    def test_to_dict_minimal(self):
        from agent_engine import ActionResult
        r = ActionResult(action="screenshot", node_id="test")
        d = r.to_dict()
        assert d["success"] is True
        assert d["action"] == "screenshot"
        assert d["node_id"] == "test"
        assert "error" not in d  # None excluded
        assert "screenshot_base64" not in d  # empty excluded

    def test_to_dict_with_error(self):
        from agent_engine import ActionResult
        r = ActionResult(success=False, error="Failed")
        d = r.to_dict()
        assert d["success"] is False
        assert d["error"] == "Failed"

    def test_to_dict_with_verification(self):
        from agent_engine import ActionResult
        r = ActionResult(screen_changed=True, diff_regions=[{"x": 0, "y": 0}])
        d = r.to_dict()
        assert d["verification"]["screen_changed"] is True
        assert len(d["verification"]["diff_regions"]) == 1

    def test_to_dict_with_som(self):
        from agent_engine import ActionResult
        r = ActionResult(som_elements={1: {"type": "button"}})
        d = r.to_dict()
        assert d["som_elements"][1]["type"] == "button"


class TestPendingAction:
    def test_construction(self):
        import time
        from agent_engine import PendingAction
        p = PendingAction(
            action_id="abc123",
            action="click",
            node_id="test",
            kwargs={"x": 100, "y": 200},
            created_at=time.time(),
        )
        assert p.action_id == "abc123"
        assert p.action == "click"
        assert p.kwargs["x"] == 100


class TestAgentEngineInit:
    def test_default_init(self):
        from agent_engine import AgentEngine
        state = MagicMock()
        state.nodes = {}
        state.active_node_id = None
        engine = AgentEngine(state)
        assert engine._last_frames == {}
        assert engine._som_registry == {}

    def test_evdev_paths(self):
        from agent_engine import AgentEngine
        state = MagicMock()
        engine = AgentEngine(state, evdev_kbd_path="/dev/input/event4",
                              evdev_mouse_path="/dev/input/event5")
        assert engine._evdev_kbd_path == "/dev/input/event4"
        assert engine._evdev_mouse_path == "/dev/input/event5"


class TestAgentEngineDispatch:
    """Test that execute() routes to the correct handler."""

    def _engine(self):
        from agent_engine import AgentEngine
        state = MagicMock()
        state.nodes = {}
        state.get_active_node = MagicMock(return_value=None)
        return AgentEngine(state)

    @pytest.mark.asyncio
    async def test_unknown_action(self):
        engine = self._engine()
        result = await engine.execute("nonexistent_action", node_id="test")
        assert not result.success
        assert "Unknown action" in result.error or "No node" in result.error

    @pytest.mark.asyncio
    async def test_no_node(self):
        engine = self._engine()
        result = await engine.execute("screenshot")
        assert not result.success
        assert "node" in result.error.lower()


class TestSoMRegistry:
    def test_resolve_element(self):
        from agent_engine import AgentEngine
        state = MagicMock()
        engine = AgentEngine(state)
        engine._som_registry["node1"] = {
            1: {"id": 1, "center": [500, 300]},
            2: {"id": 2, "center": [100, 100]},
        }
        assert engine._resolve_element("node1", 1) == (500, 300)
        assert engine._resolve_element("node1", 2) == (100, 100)
        assert engine._resolve_element("node1", 99) is None
        assert engine._resolve_element("nonexistent", 1) is None


class TestHIDToEvdevMapping:
    def test_letters_mapped(self):
        from agent_engine import AgentEngine
        mapping = AgentEngine._HID_TO_EVDEV
        # HID 0x04 = 'a' → evdev 30
        assert mapping[0x04] == 30
        # HID 0x1D = 'z' → evdev 44
        assert mapping[0x1D] == 44

    def test_modifiers_mapped(self):
        from agent_engine import AgentEngine
        mods = AgentEngine._HID_MOD_TO_EVDEV
        assert mods[0x01] == 29  # Left Ctrl
        assert mods[0x02] == 42  # Left Shift
        assert mods[0x04] == 56  # Left Alt
        assert mods[0x08] == 125 # Left Meta/Win


class TestMCPToolSchema:
    def test_schema_structure(self):
        from agent_engine import OZMA_CONTROL_TOOL
        assert OZMA_CONTROL_TOOL["name"] == "ozma_control"
        assert "input_schema" in OZMA_CONTROL_TOOL
        schema = OZMA_CONTROL_TOOL["input_schema"]
        assert schema["type"] == "object"
        assert "action" in schema["properties"]
        assert schema["required"] == ["action"]

    def test_all_actions_listed(self):
        from agent_engine import OZMA_CONTROL_TOOL
        actions = OZMA_CONTROL_TOOL["input_schema"]["properties"]["action"]["enum"]
        expected = {"screenshot", "read_screen", "click", "double_click",
                    "right_click", "type", "key", "hotkey", "mouse_move",
                    "mouse_drag", "scroll", "wait_for_text", "wait_for_element",
                    "find_elements", "assert_text", "assert_element"}
        assert expected.issubset(set(actions))
