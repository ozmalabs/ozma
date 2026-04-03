#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Unit tests for dynamic seat config push (controller -> agent).

Tests the full flow: controller REST API, WebSocket config push,
agent-side config handling, scaling, and persistence.
"""

import asyncio
import json
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

# conftest.py already sets up sys.path for agent/ and controller/


# ── Controller: state.py ─────────────────────────────────────────────────

class TestNodeInfoSeatFields(unittest.TestCase):
    """NodeInfo seat_count and seat_config fields."""

    def test_default_seat_count(self):
        from state import NodeInfo
        node = NodeInfo(id="test", host="1.2.3.4", port=7331,
                        role="compute", hw="test", fw_version="1.0",
                        proto_version=1)
        self.assertEqual(node.seat_count, 1)
        self.assertEqual(node.seat_config, {})

    def test_seat_config_in_to_dict(self):
        from state import NodeInfo
        node = NodeInfo(id="test", host="1.2.3.4", port=7331,
                        role="compute", hw="test", fw_version="1.0",
                        proto_version=1, seat_count=3,
                        seat_config={"seats": 3, "profiles": ["gaming", "gaming", "workstation"]})
        d = node.to_dict()
        self.assertEqual(d["seat_count"], 3)
        self.assertEqual(d["seat_config"]["seats"], 3)
        self.assertEqual(len(d["seat_config"]["profiles"]), 3)

    def test_seat_config_not_in_dict_when_default(self):
        from state import NodeInfo
        node = NodeInfo(id="test", host="1.2.3.4", port=7331,
                        role="compute", hw="test", fw_version="1.0",
                        proto_version=1)
        d = node.to_dict()
        self.assertNotIn("seat_count", d)
        self.assertNotIn("seat_config", d)

    def test_seat_config_preserved_on_re_register(self):
        """When a node re-registers, seat config set by controller is preserved."""
        from state import AppState, NodeInfo

        loop = asyncio.new_event_loop()
        state = AppState()

        # First registration
        node1 = NodeInfo(id="n1", host="1.2.3.4", port=7331,
                         role="compute", hw="test", fw_version="1.0",
                         proto_version=1)
        loop.run_until_complete(state.add_node(node1))

        # Controller sets seat config
        state.nodes["n1"].seat_count = 4
        state.nodes["n1"].seat_config = {"seats": 4, "profiles": []}

        # Re-registration (agent reconnects)
        node2 = NodeInfo(id="n1", host="1.2.3.4", port=7331,
                         role="compute", hw="test", fw_version="1.0",
                         proto_version=1)
        loop.run_until_complete(state.add_node(node2))

        # Seat config should be preserved
        self.assertEqual(state.nodes["n1"].seat_count, 4)
        self.assertEqual(state.nodes["n1"].seat_config["seats"], 4)
        loop.close()


# ── Controller: API endpoints ─────────────────────────────────────────────

try:
    from api import build_app
    _API_AVAILABLE = True
except ImportError:
    _API_AVAILABLE = False


@unittest.skipUnless(_API_AVAILABLE, "controller API deps not installed")
class TestSeatConfigAPI(unittest.TestCase):
    """Test seat config REST endpoints via TestClient."""

    @classmethod
    def setUpClass(cls):
        from state import AppState, NodeInfo
        from scenarios import ScenarioManager
        from api import build_app

        cls.state = AppState()
        from pathlib import Path
        scenarios = ScenarioManager(Path("/tmp/ozma-test-scenarios.json"), cls.state)
        cls.app = build_app(cls.state, scenarios)

        # Add a test node
        loop = asyncio.new_event_loop()
        node = NodeInfo(id="test-node", host="1.2.3.4", port=7331,
                        role="compute", hw="test", fw_version="1.0",
                        proto_version=1)
        loop.run_until_complete(cls.state.add_node(node))
        loop.close()

    def test_get_seat_config_default(self):
        from fastapi.testclient import TestClient
        with TestClient(self.app) as client:
            resp = client.get("/api/v1/nodes/test-node/seats")
            self.assertEqual(resp.status_code, 200)
            data = resp.json()
            self.assertEqual(data["seat_count"], 1)
            self.assertEqual(data["seat_config"], {})

    def test_get_seat_config_not_found(self):
        from fastapi.testclient import TestClient
        with TestClient(self.app) as client:
            resp = client.get("/api/v1/nodes/nonexistent/seats")
            self.assertEqual(resp.status_code, 404)

    def test_put_seat_config(self):
        from fastapi.testclient import TestClient
        with TestClient(self.app) as client:
            resp = client.put("/api/v1/nodes/test-node/seats",
                              json={"seats": 3, "profiles": ["gaming", "gaming", "workstation"]})
            self.assertEqual(resp.status_code, 200)
            data = resp.json()
            self.assertTrue(data["ok"])
            self.assertEqual(data["seat_count"], 3)

            # Verify state was updated
            node = self.state.nodes["test-node"]
            self.assertEqual(node.seat_count, 3)
            self.assertEqual(node.seat_config["seats"], 3)

    def test_put_seat_config_invalid_count(self):
        from fastapi.testclient import TestClient
        with TestClient(self.app) as client:
            resp = client.put("/api/v1/nodes/test-node/seats", json={"seats": 0})
            self.assertEqual(resp.status_code, 400)

            resp = client.put("/api/v1/nodes/test-node/seats", json={"seats": 9})
            self.assertEqual(resp.status_code, 400)

            resp = client.put("/api/v1/nodes/test-node/seats", json={"seats": "abc"})
            self.assertEqual(resp.status_code, 400)

    def test_put_seat_config_profiles_mismatch(self):
        from fastapi.testclient import TestClient
        with TestClient(self.app) as client:
            resp = client.put("/api/v1/nodes/test-node/seats",
                              json={"seats": 2, "profiles": ["gaming"]})
            self.assertEqual(resp.status_code, 400)

    def test_put_seat_config_no_profiles(self):
        """profiles is optional — empty list is fine."""
        from fastapi.testclient import TestClient
        with TestClient(self.app) as client:
            resp = client.put("/api/v1/nodes/test-node/seats", json={"seats": 2})
            self.assertEqual(resp.status_code, 200)
            data = resp.json()
            self.assertEqual(data["seat_count"], 2)
            self.assertEqual(data["seat_config"]["profiles"], [])


# ── Agent: SeatManager config handling ────────────────────────────────────

class TestSeatManagerConfigPersistence(unittest.TestCase):
    """Test config persistence to disk."""

    def test_persist_and_load(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            config_file = Path(tmpdir) / "seat_config.json"
            config_dir = Path(tmpdir)

            with patch("agent.multiseat.seat_manager._DEFAULT_CONFIG_DIR", config_dir), \
                 patch("agent.multiseat.seat_manager._DEFAULT_CONFIG_FILE", config_file):
                from agent.multiseat.seat_manager import SeatManager

                mgr = SeatManager(controller_url="http://localhost:7380",
                                  machine_name="test-host")
                mgr._persist_config({"seats": 3, "profiles": ["gaming", "gaming", "workstation"]})

                self.assertTrue(config_file.exists())
                loaded = SeatManager.load_persisted_config()
                self.assertIsNotNone(loaded)
                self.assertEqual(loaded["seats"], 3)

    def test_load_missing_file(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            config_file = Path(tmpdir) / "nonexistent.json"
            with patch("agent.multiseat.seat_manager._DEFAULT_CONFIG_FILE", config_file):
                from agent.multiseat.seat_manager import SeatManager
                self.assertIsNone(SeatManager.load_persisted_config())


class TestSeatManagerConfigMessages(unittest.TestCase):
    """Test _handle_config_message dispatching."""

    def setUp(self):
        self.mgr = self._create_manager()

    def _create_manager(self):
        with patch("agent.multiseat.seat_manager._DEFAULT_CONFIG_DIR"), \
             patch("agent.multiseat.seat_manager._DEFAULT_CONFIG_FILE"):
            from agent.multiseat.seat_manager import SeatManager
            return SeatManager(controller_url="http://localhost:7380",
                               machine_name="test-host")

    def test_handle_seat_config(self):
        loop = asyncio.new_event_loop()
        self.mgr._apply_seat_config = AsyncMock()
        self.mgr._persist_config = MagicMock()

        msg = json.dumps({"type": "seat_config", "seats": 3, "profiles": ["gaming", "gaming", "workstation"]})
        loop.run_until_complete(self.mgr._handle_config_message(msg))

        self.mgr._apply_seat_config.assert_called_once_with(3, ["gaming", "gaming", "workstation"])
        self.mgr._persist_config.assert_called_once()
        loop.close()

    def test_handle_encoder_hint(self):
        loop = asyncio.new_event_loop()
        self.mgr.rebalance_encoders = AsyncMock(return_value=[])

        msg = json.dumps({"type": "encoder_hint", "seat": "seat-0", "gaming_gpu": 0})
        loop.run_until_complete(self.mgr._handle_config_message(msg))

        self.mgr.rebalance_encoders.assert_called_once_with("seat-0", 0)
        loop.close()

    def test_handle_invalid_json(self):
        loop = asyncio.new_event_loop()
        # Should not raise
        loop.run_until_complete(self.mgr._handle_config_message("not json"))
        loop.close()


class TestSeatManagerScaling(unittest.TestCase):
    """Test _apply_seat_config scale up/down logic."""

    def _make_manager_with_seats(self, n: int):
        """Create a SeatManager with n mock seats."""
        from agent.multiseat.seat_manager import SeatManager
        mgr = SeatManager(controller_url="http://localhost:7380",
                          machine_name="test-host")
        mgr._display_backend = MagicMock()
        mgr._audio_backend = AsyncMock()
        mgr._encoder_allocator = MagicMock()
        mgr._game_launcher = MagicMock()
        mgr._game_launcher.stop = AsyncMock()
        mgr._profile = MagicMock()
        mgr._profile.name = "workstation"
        mgr._profile.capture_fps = 15
        mgr._profile.capture_width = 1920
        mgr._profile.capture_height = 1080

        # Mock encoder allocation
        mock_session = MagicMock()
        mock_session.ffmpeg_args = ["-c:v", "libx264"]
        mgr._encoder_allocator.allocate.return_value = mock_session

        for i in range(n):
            seat = MagicMock()
            seat.name = f"test-host-seat-{i}"
            seat.seat_index = i
            seat.display = MagicMock()
            seat.display.virtual = True
            seat.stop = AsyncMock()
            seat.start = AsyncMock()
            mgr._seats.append(seat)

            task = AsyncMock()
            task.get_name.return_value = f"seat-test-host-seat-{i}"
            task.cancel = MagicMock()
            mgr._seat_tasks.append(task)

        return mgr

    def test_scale_up(self):
        loop = asyncio.new_event_loop()
        mgr = self._make_manager_with_seats(1)

        # Mock display creation
        mock_display = MagicMock()
        mock_display.index = 1
        mgr._display_backend.create_virtual.return_value = mock_display

        # Mock seat start (must be a coroutine)
        with patch("agent.multiseat.seat_manager.Seat") as MockSeat:
            mock_seat_instance = MagicMock()
            mock_seat_instance.start = AsyncMock()
            mock_seat_instance.name = "test-host-seat-1"
            MockSeat.return_value = mock_seat_instance

            loop.run_until_complete(mgr._apply_seat_config(3, []))

        # Should have 3 seats now (1 original + 2 new)
        self.assertEqual(len(mgr._seats), 3)
        loop.close()

    def test_scale_down(self):
        loop = asyncio.new_event_loop()
        mgr = self._make_manager_with_seats(3)

        loop.run_until_complete(mgr._apply_seat_config(1, []))

        # Should have 1 seat remaining
        self.assertEqual(len(mgr._seats), 1)
        # Seats 2 and 1 should have been stopped
        self.assertEqual(mgr._seats[0].name, "test-host-seat-0")
        loop.close()

    def test_same_count_noop(self):
        loop = asyncio.new_event_loop()
        mgr = self._make_manager_with_seats(2)

        loop.run_until_complete(mgr._apply_seat_config(2, []))

        # No seats created or destroyed
        self.assertEqual(len(mgr._seats), 2)
        loop.close()

    def test_clamp_range(self):
        loop = asyncio.new_event_loop()
        mgr = self._make_manager_with_seats(2)

        # 0 is clamped to 1
        loop.run_until_complete(mgr._apply_seat_config(0, []))
        self.assertEqual(len(mgr._seats), 1)
        loop.close()


if __name__ == "__main__":
    unittest.main()
