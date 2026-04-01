#!/usr/bin/env python3
"""
Unit tests for gamepad controller detection and profile mapping.

Does NOT require a connected gamepad.  Tests the detection logic,
profile selection, button/axis constants, and default bindings.
"""

import sys
import unittest

sys.path.insert(0, "controller")

try:
    from gamepad import (
        detect_profile, ControllerProfile, _PROFILES,
        GAMEPAD_BUTTONS, GAMEPAD_AXES, GamepadSurface,
    )
    _EVDEV_AVAILABLE = True
except ImportError:
    _EVDEV_AVAILABLE = False


@unittest.skipUnless(_EVDEV_AVAILABLE, "evdev not available")


class TestProfileDetection(unittest.TestCase):

    def test_xbox_360(self):
        p = detect_profile("Microsoft X-Box 360 pad")
        self.assertEqual(p.family, "xbox")
        self.assertEqual(p.south_label, "A")

    def test_xbox_one(self):
        p = detect_profile("Xbox One Controller")
        self.assertEqual(p.family, "xbox")

    def test_xbox_series(self):
        p = detect_profile("Xbox Series X Controller")
        self.assertEqual(p.family, "xbox")
        self.assertEqual(p.variant, "series")

    def test_xbox_generic(self):
        p = detect_profile("Xbox Wireless Controller")
        self.assertEqual(p.family, "xbox")

    def test_ps5_dualsense(self):
        p = detect_profile("Sony Interactive Entertainment DualSense Wireless Controller")
        self.assertEqual(p.family, "playstation")
        self.assertEqual(p.variant, "dualsense")
        self.assertEqual(p.south_label, "Cross")
        self.assertEqual(p.east_label, "Circle")

    def test_ps4_dualshock4(self):
        p = detect_profile("Sony Interactive Entertainment DualShock 4")
        self.assertEqual(p.family, "playstation")
        self.assertEqual(p.variant, "ds4")

    def test_ps3_dualshock3(self):
        p = detect_profile("Sony PLAYSTATION(R)3 DualShock 3 Controller")
        self.assertEqual(p.family, "playstation")

    def test_8bitdo(self):
        p = detect_profile("8BitDo SN30 Pro+")
        self.assertEqual(p.family, "generic")

    def test_switch_pro(self):
        p = detect_profile("Nintendo Switch Pro Controller")
        self.assertEqual(p.family, "generic")

    def test_unknown_fallback(self):
        p = detect_profile("Random USB Gamepad 3000")
        self.assertEqual(p.family, "generic")
        self.assertEqual(p.south_label, "South")

    def test_case_insensitive(self):
        p = detect_profile("XBOX ONE CONTROLLER")
        self.assertEqual(p.family, "xbox")

    def test_microsoft_keyword(self):
        p = detect_profile("Microsoft Corporation Controller")
        self.assertEqual(p.family, "xbox")

    def test_sony_keyword(self):
        p = detect_profile("Sony Corp. Wireless Controller")
        self.assertEqual(p.family, "playstation")


@unittest.skipUnless(_EVDEV_AVAILABLE, "evdev not available")
class TestButtonConstants(unittest.TestCase):

    def test_all_buttons_are_ints(self):
        for name, code in GAMEPAD_BUTTONS.items():
            self.assertIsInstance(code, int, f"{name} should be int")

    def test_all_axes_are_ints(self):
        for name, code in GAMEPAD_AXES.items():
            self.assertIsInstance(code, int, f"{name} should be int")

    def test_south_is_btn_gamepad(self):
        """BTN_SOUTH and BTN_GAMEPAD are the same code on Linux."""
        from evdev import ecodes
        self.assertEqual(GAMEPAD_BUTTONS["south"], ecodes.BTN_SOUTH)

    def test_standard_button_set(self):
        expected = {"south", "east", "north", "west", "lb", "rb",
                    "lt_btn", "rt_btn", "select", "start", "guide",
                    "lstick", "rstick"}
        self.assertEqual(set(GAMEPAD_BUTTONS.keys()), expected)

    def test_standard_axis_set(self):
        expected = {"lx", "ly", "rx", "ry", "lt", "rt", "hat_x", "hat_y"}
        self.assertEqual(set(GAMEPAD_AXES.keys()), expected)


@unittest.skipUnless(_EVDEV_AVAILABLE, "evdev not available")
class TestProfiles(unittest.TestCase):

    def test_all_profiles_have_labels(self):
        for key, p in _PROFILES.items():
            self.assertTrue(p.south_label, f"{key} missing south_label")
            self.assertTrue(p.east_label, f"{key} missing east_label")
            self.assertTrue(p.north_label, f"{key} missing north_label")
            self.assertTrue(p.west_label, f"{key} missing west_label")
            self.assertTrue(p.family, f"{key} missing family")

    def test_xbox_labels(self):
        p = _PROFILES["xbox"]
        self.assertEqual(p.south_label, "A")
        self.assertEqual(p.east_label, "B")
        self.assertEqual(p.north_label, "X")
        self.assertEqual(p.west_label, "Y")

    def test_playstation_labels(self):
        p = _PROFILES["playstation"]
        self.assertEqual(p.south_label, "Cross")
        self.assertEqual(p.east_label, "Circle")
        self.assertEqual(p.north_label, "Triangle")
        self.assertEqual(p.west_label, "Square")


@unittest.skipUnless(_EVDEV_AVAILABLE, "evdev not available")
class TestGamepadSurfaceDefaults(unittest.TestCase):

    def test_default_controls_created(self):
        """GamepadSurface builds default controls without a real device."""
        from unittest.mock import MagicMock
        mock_dev = MagicMock()
        mock_dev.name = "Xbox Wireless Controller"
        mock_dev.path = "/dev/input/event99"
        mock_dev.capabilities.return_value = {}

        gpad = GamepadSurface(mock_dev)
        self.assertIn("dpad_right", gpad.controls)
        self.assertIn("dpad_left", gpad.controls)
        self.assertIn("rb", gpad.controls)
        self.assertIn("lb", gpad.controls)
        self.assertIn("south", gpad.controls)
        self.assertIn("guide", gpad.controls)
        self.assertIn("rt_volume", gpad.controls)
        self.assertIn("dpad_up", gpad.controls)
        self.assertIn("dpad_down", gpad.controls)

    def test_surface_id_format(self):
        from unittest.mock import MagicMock
        mock_dev = MagicMock()
        mock_dev.name = "Sony Interactive Entertainment DualSense Wireless Controller"
        mock_dev.path = "/dev/input/event5"
        mock_dev.capabilities.return_value = {}

        gpad = GamepadSurface(mock_dev)
        self.assertEqual(gpad.id, "gamepad-playstation-dualsense")

    def test_to_dict(self):
        from unittest.mock import MagicMock
        mock_dev = MagicMock()
        mock_dev.name = "Xbox 360 Controller"
        mock_dev.path = "/dev/input/event3"
        mock_dev.capabilities.return_value = {}

        gpad = GamepadSurface(mock_dev)
        d = gpad.to_dict()
        self.assertEqual(d["device"], "Xbox 360 Controller")
        self.assertEqual(d["profile"]["family"], "xbox")


if __name__ == "__main__":
    unittest.main()
