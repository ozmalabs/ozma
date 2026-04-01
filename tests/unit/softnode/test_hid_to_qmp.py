"""Unit tests for HID → QMP translation."""

import pytest

pytestmark = pytest.mark.unit


class TestHIDToQcodeMapping:
    """Verify the HID Usage ID → QMP qcode translation table."""

    def test_letters_mapped(self):
        from hid_to_qmp import HID_TO_QCODE
        # HID 0x04 = 'a', 0x1D = 'z'
        for hid_code in range(0x04, 0x1E):
            assert hid_code in HID_TO_QCODE
            assert isinstance(HID_TO_QCODE[hid_code], str)

    def test_numbers_mapped(self):
        from hid_to_qmp import HID_TO_QCODE
        # HID 0x1E = '1', 0x27 = '0'
        for hid_code in range(0x1E, 0x28):
            assert hid_code in HID_TO_QCODE

    def test_specific_keys(self):
        from hid_to_qmp import HID_TO_QCODE
        assert HID_TO_QCODE[0x28] == "ret"       # Enter
        assert HID_TO_QCODE[0x29] == "esc"       # Escape
        assert HID_TO_QCODE[0x2A] == "backspace"
        assert HID_TO_QCODE[0x2B] == "tab"
        assert HID_TO_QCODE[0x2C] == "spc"       # Space

    def test_function_keys(self):
        from hid_to_qmp import HID_TO_QCODE
        for i, hid_code in enumerate(range(0x3A, 0x46)):
            assert hid_code in HID_TO_QCODE, f"F{i+1} (0x{hid_code:02X}) missing"

    def test_arrow_keys(self):
        from hid_to_qmp import HID_TO_QCODE
        assert HID_TO_QCODE[0x4F] == "right"
        assert HID_TO_QCODE[0x50] == "left"
        assert HID_TO_QCODE[0x51] == "down"
        assert HID_TO_QCODE[0x52] == "up"

    def test_no_empty_values(self):
        from hid_to_qmp import HID_TO_QCODE
        for code, qcode in HID_TO_QCODE.items():
            assert qcode, f"HID 0x{code:02X} maps to empty string"


class TestKeyboardReportState:
    """Test stateful HID keyboard report diffing."""

    def _state(self):
        from hid_to_qmp import KeyboardReportState
        return KeyboardReportState()

    def test_empty_report_no_events(self):
        state = self._state()
        events = state.diff(bytes(8))
        assert events == []

    def test_press_a(self):
        state = self._state()
        # No modifier, key slot 0 = 0x04 ('a')
        report = bytes([0x00, 0x00, 0x04, 0, 0, 0, 0, 0])
        events = state.diff(report)
        assert len(events) >= 1
        # Should have a key-down for 'a'
        key_events = [e for e in events if e.get("type") == "key"]
        assert any(e["data"]["key"]["data"] == "a" and e["data"]["down"] for e in key_events)

    def test_release_a(self):
        state = self._state()
        # Press
        state.diff(bytes([0x00, 0x00, 0x04, 0, 0, 0, 0, 0]))
        # Release
        events = state.diff(bytes(8))
        key_events = [e for e in events if e.get("type") == "key"]
        assert any(e["data"]["key"]["data"] == "a" and not e["data"]["down"] for e in key_events)

    def test_modifier_left_ctrl(self):
        state = self._state()
        # Left Ctrl = bit 0 of modifier byte
        report = bytes([0x01, 0x00, 0, 0, 0, 0, 0, 0])
        events = state.diff(report)
        key_events = [e for e in events if e.get("type") == "key"]
        assert any(e["data"]["key"]["data"] == "ctrl" and e["data"]["down"] for e in key_events)

    def test_modifier_left_shift(self):
        state = self._state()
        report = bytes([0x02, 0x00, 0, 0, 0, 0, 0, 0])
        events = state.diff(report)
        key_events = [e for e in events if e.get("type") == "key"]
        assert any(e["data"]["key"]["data"] == "shift" and e["data"]["down"] for e in key_events)

    def test_multiple_keys(self):
        state = self._state()
        # Press 'a' and 'b' simultaneously
        report = bytes([0x00, 0x00, 0x04, 0x05, 0, 0, 0, 0])
        events = state.diff(report)
        key_events = [e for e in events if e.get("type") == "key"]
        keys_pressed = {e["data"]["key"]["data"] for e in key_events if e["data"]["down"]}
        assert "a" in keys_pressed
        assert "b" in keys_pressed

    def test_short_report_ignored(self):
        state = self._state()
        events = state.diff(bytes(3))
        assert events == []

    def test_rollover_no_crash(self):
        state = self._state()
        # Phantom key (0x01) in all slots = rollover
        report = bytes([0x00, 0x00, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01])
        events = state.diff(report)
        # Should not crash — may or may not produce events
        assert isinstance(events, list)

    def test_key_then_key_only_new_pressed(self):
        state = self._state()
        state.diff(bytes([0x00, 0x00, 0x04, 0, 0, 0, 0, 0]))  # press 'a'
        events = state.diff(bytes([0x00, 0x00, 0x04, 0x05, 0, 0, 0, 0]))  # add 'b'
        key_events = [e for e in events if e.get("type") == "key"]
        # Should only have 'b' press, not 'a' again
        pressed = [e for e in key_events if e["data"]["down"]]
        assert len(pressed) == 1
        assert pressed[0]["data"]["key"]["data"] == "b"


class TestMouseReportState:
    """Test HID mouse report decoding."""

    def _state(self):
        from hid_to_qmp import MouseReportState
        return MouseReportState()

    def test_position_decoding(self):
        state = self._state()
        # buttons=0, x=16383 (0x3FFF), y=16383, scroll=0
        report = bytes([0, 0xFF, 0x3F, 0xFF, 0x3F, 0])
        events = state.decode(report)
        abs_events = [e for e in events if e.get("type") == "abs"]
        assert len(abs_events) >= 2  # X and Y

    def test_left_button_press(self):
        state = self._state()
        # buttons=1 (left), x=100, y=100, scroll=0
        report = bytes([1, 100, 0, 100, 0, 0])
        events = state.decode(report)
        btn_events = [e for e in events if e.get("type") == "btn"]
        assert any(e["data"]["button"] == "left" and e["data"]["down"] for e in btn_events)

    def test_button_release(self):
        state = self._state()
        state.decode(bytes([1, 100, 0, 100, 0, 0]))  # press
        events = state.decode(bytes([0, 100, 0, 100, 0, 0]))  # release
        btn_events = [e for e in events if e.get("type") == "btn"]
        assert any(e["data"]["button"] == "left" and not e["data"]["down"] for e in btn_events)

    def test_scroll_up(self):
        state = self._state()
        report = bytes([0, 0, 0, 0, 0, 1])  # scroll=1
        events = state.decode(report)
        # Should produce scroll events (button 4/5 in VNC protocol)
        assert len(events) > 0

    def test_short_report(self):
        state = self._state()
        events = state.decode(bytes(2))
        assert events == []

    def test_zero_position(self):
        state = self._state()
        report = bytes([0, 0, 0, 0, 0, 0])
        events = state.decode(report)
        abs_events = [e for e in events if e.get("type") == "abs"]
        # Position 0,0 should still produce abs events
        assert len(abs_events) >= 2
