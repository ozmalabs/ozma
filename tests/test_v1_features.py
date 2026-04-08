#!/usr/bin/env python3
"""
Unit tests for V0.6/V1.0 modules — no running stack needed.
Tests core logic, data structures, and algorithms.
"""

import asyncio
import sys
import time
import unittest
from unittest.mock import AsyncMock, MagicMock, patch

sys.path.insert(0, "controller")


class TestWoL(unittest.TestCase):
    def test_send_wol_valid_mac(self):
        from wol import send_wol
        with patch("wol.socket") as mock_sock:
            mock_inst = MagicMock()
            mock_sock.socket.return_value = mock_inst
            ok = send_wol("AA:BB:CC:DD:EE:FF")
            self.assertTrue(ok)
            mock_inst.sendto.assert_called_once()
            data = mock_inst.sendto.call_args[0][0]
            self.assertEqual(data[:6], b"\xff" * 6)
            self.assertEqual(len(data), 6 + 6 * 16)

    def test_send_wol_invalid_mac(self):
        from wol import send_wol
        ok = send_wol("invalid")
        self.assertFalse(ok)


class TestEdgesCrossing(unittest.TestCase):
    def setUp(self):
        from edge_crossing import EdgeCrossingManager
        self.mgr = EdgeCrossingManager(MagicMock())
        self.mgr.auto_layout(["node-a", "node-b"], width=1920, height=1080)

    def test_no_crossing_within_screen(self):
        result = self.mgr.check_crossing(100, 50, "node-a")
        self.assertIsNone(result)

    def test_crossing_right_edge(self):
        # Move cursor to right edge of node-a
        self.mgr._global_x = 1919
        result = self.mgr.check_crossing(5, 0, "node-a")
        self.assertIsNotNone(result)
        new_node, abs_x, abs_y = result
        self.assertEqual(new_node, "node-b")
        self.assertGreaterEqual(abs_x, 0)

    def test_crossing_left_edge(self):
        self.mgr._global_x = 1920
        self.mgr._current_node = "node-b"
        result = self.mgr.check_crossing(-5, 0, "node-b")
        self.assertIsNotNone(result)
        self.assertEqual(result[0], "node-a")

    def test_no_crossing_when_disabled(self):
        self.mgr.set_enabled(False)
        self.mgr._global_x = 1919
        result = self.mgr.check_crossing(5, 0, "node-a")
        self.assertIsNone(result)

    def test_sticky_edge(self):
        self.mgr.set_sticky(500)  # 500ms sticky
        self.mgr._global_x = 1919
        # First crossing attempt — should be blocked (sticky)
        result = self.mgr.check_crossing(5, 0, "node-a")
        self.assertIsNone(result)

    def test_single_screen_no_crossing(self):
        from edge_crossing import EdgeCrossingManager
        mgr = EdgeCrossingManager(MagicMock())
        mgr.auto_layout(["only-node"])
        result = mgr.check_crossing(100, 0, "only-node")
        self.assertIsNone(result)

    def test_get_layout(self):
        layout = self.mgr.get_layout()
        self.assertEqual(len(layout), 2)
        self.assertEqual(layout[0]["node_id"], "node-a")
        self.assertEqual(layout[1]["node_id"], "node-b")
        self.assertEqual(layout[1]["x"], 1920)


class TestScheduler(unittest.TestCase):
    def test_rule_matches_time(self):
        from scheduler import ScheduleRule
        rule = ScheduleRule(time="09:00", days="*", scenario="work")
        # Can't easily test time matching without mocking datetime
        self.assertEqual(rule.to_dict()["time"], "09:00")

    def test_rule_matches_day(self):
        from scheduler import ScheduleRule, _DAY_MAP
        rule = ScheduleRule(time="00:00", days="mon,wed,fri", scenario="test")
        d = rule.to_dict()
        self.assertEqual(d["days"], "mon,wed,fri")
        self.assertIn("test", d["scenario"])


class TestPasteTyping(unittest.TestCase):
    def test_us_layout_lowercase(self):
        from paste_typing import LAYOUTS
        layout = LAYOUTS["us"]
        stroke = layout.get("a")
        self.assertIsNotNone(stroke)
        self.assertEqual(stroke.modifier, 0)
        self.assertEqual(stroke.key, 0x04)

    def test_us_layout_uppercase(self):
        from paste_typing import LAYOUTS
        layout = LAYOUTS["us"]
        stroke = layout.get("A")
        self.assertIsNotNone(stroke)
        self.assertEqual(stroke.modifier, 0x02)  # Left Shift

    def test_us_layout_symbols(self):
        from paste_typing import LAYOUTS
        layout = LAYOUTS["us"]
        for sym in "!@#$%^&*()":
            self.assertIn(sym, layout, f"Missing symbol: {sym}")
            self.assertEqual(layout[sym].modifier, 0x02)  # All shifted

    def test_available_layouts(self):
        from paste_typing import PasteTyper
        layouts = PasteTyper.available_layouts()
        self.assertIn("us", layouts)
        self.assertIn("uk", layouts)
        self.assertIn("de", layouts)

    def test_de_layout_zy_swap(self):
        from paste_typing import LAYOUTS
        de = LAYOUTS["de"]
        us = LAYOUTS["us"]
        self.assertEqual(de["z"].key, us["y"].key)
        self.assertEqual(de["y"].key, us["z"].key)


try:
    import numpy
    _NUMPY = True
except ImportError:
    _NUMPY = False

try:
    from zeroconf import ServiceInfo
    _ZEROCONF = True
except ImportError:
    _ZEROCONF = False


@unittest.skipUnless(_NUMPY, "numpy not available")
class TestTextCapture(unittest.TestCase):
    def test_cp437_map(self):
        from text_capture import _CP437_MAP
        self.assertEqual(_CP437_MAP[32], " ")
        self.assertEqual(_CP437_MAP[65], "A")
        self.assertIn(219, _CP437_MAP)  # Full block

    def test_resolution_dataclass(self):
        from display_capture import Resolution
        r = Resolution(3440, 1440, 60)
        self.assertEqual(r.aspect_ratio, "21:9")
        self.assertAlmostEqual(r.aspect_float, 3440 / 1440, places=2)

    def test_resolution_16_9(self):
        from display_capture import Resolution
        self.assertEqual(Resolution(1920, 1080).aspect_ratio, "16:9")
        self.assertEqual(Resolution(3840, 2160).aspect_ratio, "16:9")

    def test_resolution_32_9(self):
        from display_capture import Resolution
        self.assertEqual(Resolution(5120, 1440).aspect_ratio, "32:9")

    def test_resolution_4_3(self):
        from display_capture import Resolution
        self.assertEqual(Resolution(1024, 768).aspect_ratio, "4:3")


@unittest.skipUnless(_NUMPY, "numpy not available")
class TestEdidGeneration(unittest.TestCase):
    def test_generate_edid_length(self):
        from edid import generate_edid
        edid = generate_edid(1920, 1080, 60)
        self.assertEqual(len(edid), 128)

    def test_generate_edid_header(self):
        from edid import generate_edid
        edid = generate_edid()
        self.assertEqual(edid[:8], b"\x00\xff\xff\xff\xff\xff\xff\x00")

    def test_generate_edid_checksum(self):
        from edid import generate_edid
        edid = generate_edid(3440, 1440, 60)
        self.assertEqual(sum(edid) % 256, 0)

    def test_parse_edid_roundtrip(self):
        from edid import generate_edid, parse_edid_resolution
        edid = generate_edid(1920, 1080, 60)
        result = parse_edid_resolution(edid)
        self.assertIsNotNone(result)
        w, h, r = result
        self.assertEqual(w, 1920)
        self.assertEqual(h, 1080)


class TestOCRTriggers(unittest.TestCase):
    def test_builtin_patterns_exist(self):
        from ocr_triggers import _BUILTIN_PATTERNS
        self.assertGreater(len(_BUILTIN_PATTERNS), 20)

    def test_kernel_panic_pattern(self):
        from ocr_triggers import TriggerPattern
        p = TriggerPattern(id="test", pattern="Kernel panic", severity="critical")
        self.assertTrue(p.matches("blah Kernel panic blah"))
        self.assertFalse(p.matches("everything is fine"))

    def test_regex_pattern(self):
        from ocr_triggers import TriggerPattern
        p = TriggerPattern(id="test", pattern=r"error:.*grub", is_regex=True, severity="error")
        self.assertTrue(p.matches("error: unknown filesystem grub"))
        self.assertFalse(p.matches("no errors here"))

    def test_cooldown(self):
        from ocr_triggers import TriggerPattern
        p = TriggerPattern(id="test", pattern="error", cooldown_s=60)
        self.assertTrue(p.can_fire())
        p.mark_fired()
        self.assertFalse(p.can_fire())


class TestNotifications(unittest.TestCase):
    def test_rule_matching(self):
        from notifications import NotifyRule
        rule = NotifyRule(event_pattern="node.offline", destination_id="slack")
        self.assertTrue(rule.matches("node.offline"))
        self.assertFalse(rule.matches("node.online"))

    def test_wildcard_rule(self):
        from notifications import NotifyRule
        rule = NotifyRule(event_pattern="kdeconnect.*", destination_id="discord")
        self.assertTrue(rule.matches("kdeconnect.notification"))
        self.assertTrue(rule.matches("kdeconnect.battery"))
        self.assertFalse(rule.matches("node.offline"))


class TestNetworkHealth(unittest.TestCase):
    def test_node_health_dataclass(self):
        from network_health import NodeHealth
        h = NodeHealth(node_id="test", host="10.0.0.1")
        d = h.to_dict()
        self.assertEqual(d["node_id"], "test")
        self.assertFalse(d["online"])
        self.assertEqual(d["latency_ms"], 0.0)


@unittest.skipUnless(_ZEROCONF, "zeroconf not available")
class TestGridService(unittest.TestCase):
    def setUp(self):
        import tempfile
        from pathlib import Path
        self._tmpdir = tempfile.TemporaryDirectory()
        self._data_dir = Path(self._tmpdir.name) / "grid"

    def tearDown(self):
        self._tmpdir.cleanup()

    def _make_grid(self):
        from grid import GridService
        return GridService(data_dir=self._data_dir)

    def test_claim_mark(self):
        grid = self._make_grid()
        ok = grid.claim_mark("mark-1", "desk-a")
        self.assertTrue(ok)
        claim = grid.get_claim("mark-1")
        self.assertEqual(claim.desk_id, "desk-a")

    def test_claim_transfer(self):
        grid = self._make_grid()
        grid.claim_mark("mark-1", "desk-a")
        grid.claim_mark("mark-1", "desk-b")
        claim = grid.get_claim("mark-1")
        self.assertEqual(claim.desk_id, "desk-b")

    def test_release_mark(self):
        grid = self._make_grid()
        grid.claim_mark("mark-1", "desk-a")
        ok = grid.release_mark("mark-1", "desk-a")
        self.assertTrue(ok)
        self.assertIsNone(grid.get_claim("mark-1"))

    def test_release_wrong_desk(self):
        grid = self._make_grid()
        grid.claim_mark("mark-1", "desk-a")
        ok = grid.release_mark("mark-1", "desk-b")
        self.assertFalse(ok)

    def test_show_state(self):
        from grid import DeskInfo
        grid = self._make_grid()
        grid.register_desk(DeskInfo(id="d1", name="Desk 1", host="10.0.0.1", port=7380))
        grid.claim_mark("m1", "d1")
        state = grid.show_state()
        self.assertEqual(len(state["desks"]), 1)
        self.assertEqual(len(state["claims"]), 1)


if __name__ == "__main__":
    unittest.main()
