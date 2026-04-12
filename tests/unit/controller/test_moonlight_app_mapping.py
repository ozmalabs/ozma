# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Unit tests for controller/gaming/moonlight_app_mapping.py."""

from __future__ import annotations

import asyncio
import json
import sys
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

sys.path.insert(0, str(Path(__file__).parents[3] / "controller"))
sys.path.insert(0, str(Path(__file__).parents[2]))

from gaming.moonlight_app_mapping import (
    MoonlightApp,
    MoonlightAppMapper,
    create_app_mapper,
)

sys.path.pop(0)
sys.path.pop(0)


def run(coro):
    return asyncio.run(coro)


# ---------------------------------------------------------------------------
# MoonlightApp
# ---------------------------------------------------------------------------

class TestMoonlightApp:
    def test_minimal_app(self):
        app = MoonlightApp(app_id="work", name="Work")
        d = app.to_dict()
        assert d["appId"] == "work"
        assert d["name"] == "Work"
        assert "icon" not in d
        assert "node_id" not in d

    def test_app_with_all_fields(self):
        app = MoonlightApp(
            app_id="gaming",
            name="Gaming PC",
            icon_url="http://example.com/icon.png",
            node_id="vm1._ozma._udp.local.",
            capture_source="hdmi-0",
            vnc_host="192.168.1.50",
            vnc_port=5901,
        )
        d = app.to_dict()
        assert d["appId"] == "gaming"
        assert d["name"] == "Gaming PC"
        assert d["icon"] == "http://example.com/icon.png"
        assert d["node_id"] == "vm1._ozma._udp.local."
        assert d["captureSource"] == "hdmi-0"
        assert d["vncHost"] == "192.168.1.50"
        assert d["vncPort"] == 5901

    def test_app_without_optional_fields(self):
        app = MoonlightApp(
            app_id="media",
            name="Media Center",
        )
        d = app.to_dict()
        assert len(d) == 2  # Only appId and name
        assert d == {"appId": "media", "name": "Media Center"}


# ---------------------------------------------------------------------------
# MoonlightAppMapper
# ---------------------------------------------------------------------------

class TestMoonlightAppMapper:
    def _make_mock_scenarios(self, scenario_list):
        """Create a mock ScenarioManager."""
        mock_mgr = MagicMock()

        class MockScenario:
            def __init__(self, data):
                self.__dict__.update(data)

        mock_scenarios_dict = {s["id"]: MockScenario(s) for s in scenario_list}

        mock_mgr.list = MagicMock(return_value=scenario_list)
        mock_mgr.get = MagicMock(side_effect=lambda sid: mock_scenarios_dict.get(sid))
        mock_mgr._state = MagicMock()
        mock_mgr._state.nodes = {}
        mock_mgr.activate = AsyncMock()
        return mock_mgr

    def _make_mock_sunshine(self):
        """Create a mock SunshineManager."""
        mock_mgr = MagicMock()
        mock_mgr.enable_node = AsyncMock()
        mock_mgr.get_config = MagicMock(return_value=None)
        return mock_mgr

    def test_empty_scenario_list(self):
        mock_scenarios = self._make_mock_scenarios([])
        mapper = MoonlightAppMapper(mock_scenarios)

        apps = mapper.get_app_list()
        assert apps == []

    def test_single_scenario_conversion(self):
        scenario_list = [
            {
                "id": "work",
                "name": "Work",
                "node_id": "node1._ozma._udp.local.",
                "color": "#4A90D9",
            }
        ]
        mock_scenarios = self._make_mock_scenarios(scenario_list)
        mapper = MoonlightAppMapper(mock_scenarios)

        apps = mapper.get_app_list()
        assert len(apps) == 1
        assert apps[0]["appId"] == "work"
        assert apps[0]["name"] == "Work"

    def test_multiple_scenarios(self):
        scenario_list = [
            {"id": "work", "name": "Work", "color": "#4A90D9"},
            {"id": "gaming", "name": "Gaming", "color": "#E63946"},
            {"id": "media", "name": "Media", "color": "#2A9D8F"},
        ]
        mock_scenarios = self._make_mock_scenarios(scenario_list)
        mapper = MoonlightAppMapper(mock_scenarios)

        apps = mapper.get_app_list()
        assert len(apps) == 3
        assert apps[0]["appId"] == "work"
        assert apps[1]["appId"] == "gaming"
        assert apps[2]["appId"] == "media"

    def test_scenario_without_node(self):
        scenario_list = [
            {"id": "placeholder", "name": "Placeholder", "color": "#888888"},
        ]
        mock_scenarios = self._make_mock_scenarios(scenario_list)
        mapper = MoonlightAppMapper(mock_scenarios)

        apps = mapper.get_app_list()
        assert apps[0]["appId"] == "placeholder"
        assert "node_id" not in apps[0]

    def test_scenario_with_vnc_node(self):
        scenario_list = [
            {
                "id": "vm1",
                "name": "VM 1",
                "node_id": "vm1._ozma._udp.local.",
                "color": "#3366CC",
            }
        ]
        mock_scenarios = self._make_mock_scenarios(scenario_list)
        mock_scenarios._state.nodes = {
            "vm1._ozma._udp.local.": MagicMock(
                vnc_host="192.168.1.100",
                vnc_port=5900,
                host="192.168.1.50"
            )
        }
        mapper = MoonlightAppMapper(mock_scenarios)

        apps = mapper.get_app_list()
        assert apps[0]["vncHost"] == "192.168.1.100"
        assert apps[0]["vncPort"] == 5900

    def test_scenario_with_hdpi_capture(self):
        scenario_list = [
            {
                "id": "physical",
                "name": "Physical Machine",
                "node_id": "node1._ozma._udp.local.",
                "capture_source": "hdmi-0",
                "color": "#FF5733",
            }
        ]
        mock_scenarios = self._make_mock_scenarios(scenario_list)
        mapper = MoonlightAppMapper(mock_scenarios)

        apps = mapper.get_app_list()
        assert apps[0]["captureSource"] == "hdmi-0"

    def test_scenario_icon_url_generation(self):
        scenario_list = [
            {"id": "test", "name": "Test", "color": "#FF0000"},
        ]
        mock_scenarios = self._make_mock_scenarios(scenario_list)
        mapper = MoonlightAppMapper(mock_scenarios)

        apps = mapper.get_app_list()
        assert "icon" in apps[0]
        assert apps[0]["icon"].startswith("data:image/svg+xml,")

    def test_launch_app_success(self):
        scenario_list = [
            {"id": "work", "name": "Work", "node_id": "node1._ozma._udp.local."},
        ]
        mock_scenarios = self._make_mock_scenarios(scenario_list)
        mapper = MoonlightAppMapper(mock_scenarios)

        result = run(mapper.launch_app("work"))

        assert result["ok"] is True
        assert result["scenario_id"] == "work"
        mock_scenarios.activate.assert_awaited_once_with("work")

    def test_launch_app_not_found(self):
        scenario_list = [
            {"id": "work", "name": "Work"},
        ]
        mock_scenarios = self._make_mock_scenarios(scenario_list)
        mapper = MoonlightAppMapper(mock_scenarios)

        result = run(mapper.launch_app("nonexistent"))

        assert result["ok"] is False
        assert "not found" in result["error"].lower()

    def test_launch_app_sunshine_enabled(self):
        scenario_list = [
            {"id": "gaming", "name": "Gaming", "node_id": "vm1._ozma._udp.local."},
        ]
        mock_scenarios = self._make_mock_scenarios(scenario_list)

        mock_sunshine = self._make_mock_sunshine()
        mock_sunshine.get_config = MagicMock(return_value=None)

        mapper = MoonlightAppMapper(mock_scenarios, mock_sunshine)

        result = run(mapper.launch_app("gaming"))

        assert result["ok"] is True
        assert result["streaming_enabled"] is True
        mock_sunshine.enable_node.assert_awaited_once_with("vm1._ozma._udp.local.")

    def test_launch_app_sunshine_already_enabled(self):
        scenario_list = [
            {"id": "gaming", "name": "Gaming", "node_id": "vm1._ozma._udp.local."},
        ]
        mock_scenarios = self._make_mock_scenarios(scenario_list)

        mock_config = MagicMock()
        mock_config.enabled = True
        mock_sunshine = self._make_mock_sunshine()
        mock_sunshine.get_config = MagicMock(return_value=mock_config)

        mapper = MoonlightAppMapper(mock_scenarios, mock_sunshine)

        result = run(mapper.launch_app("gaming"))

        assert result["ok"] is True
        assert result["streaming_enabled"] is False
        assert result["reason"] == "Streaming already enabled"
        mock_sunshine.enable_node.assert_not_called()

    def test_cache_invalidation(self):
        scenario_list = [
            {"id": "work", "name": "Work"},
        ]
        mock_scenarios = self._make_mock_scenarios(scenario_list)
        mapper = MoonlightAppMapper(mock_scenarios)

        # First call builds cache
        apps1 = mapper.get_app_list()
        assert len(apps1) == 1

        # Add more scenarios
        mock_scenarios.list.return_value = [
            {"id": "work", "name": "Work"},
            {"id": "gaming", "name": "Gaming"},
        ]

        # Cache should still return old value
        apps2 = mapper.get_app_list()
        assert len(apps2) == 1

        # Invalidate cache
        mapper.invalidate_cache()

        # Now should get new value
        apps3 = mapper.get_app_list()
        assert len(apps3) == 2

    def test_get_app_by_id(self):
        scenario_list = [
            {"id": "work", "name": "Work"},
            {"id": "gaming", "name": "Gaming"},
        ]
        mock_scenarios = self._make_mock_scenarios(scenario_list)
        mapper = MoonlightAppMapper(mock_scenarios)

        # Force cache build
        _ = mapper.get_app_list()

        app = mapper.get_app_by_id("gaming")
        assert app is not None
        assert app.app_id == "gaming"
        assert app.name == "Gaming"

        app = mapper.get_app_by_id("nonexistent")
        assert app is None


# ---------------------------------------------------------------------------
# Factory function
# ---------------------------------------------------------------------------

class TestCreateAppMapper:
    def test_create_app_mapper(self):
        mock_scenarios = MagicMock()
        mock_sunshine = MagicMock()

        mapper = create_app_mapper(mock_scenarios, mock_sunshine)

        assert isinstance(mapper, MoonlightAppMapper)
        assert mapper._scenarios is mock_scenarios
        assert mapper._sunshine is mock_sunshine


# ---------------------------------------------------------------------------
# Integration tests with mocked dependencies
# ---------------------------------------------------------------------------

class TestIntegration:
    def test_full_scenario_to_app_flow(self):
        """Test complete flow: scenarios -> apps -> launch."""
        scenario_list = [
            {
                "id": "work",
                "name": "Work",
                "node_id": "workstation1._ozma._udp.local.",
                "color": "#4A90D9",
                "capture_source": "hdmi-0",
            },
            {
                "id": "vm1",
                "name": "Ubuntu VM",
                "node_id": "vm1._ozma._udp.local.",
                "color": "#3366CC",
                "vnc_host": "192.168.1.100",
                "vnc_port": 5900,
            },
            {
                "id": "media",
                "name": "Media Server",
                "node_id": None,  # No node bound
                "color": "#2A9D8F",
            },
        ]

        mock_scenarios = self._make_mock_scenarios(scenario_list)
        mock_scenarios._state.nodes = {
            "vm1._ozma._udp.local.": MagicMock(
                vnc_host="192.168.1.100",
                vnc_port=5900,
            )
        }

        mock_sunshine = self._make_mock_sunshine()
        mapper = MoonlightAppMapper(mock_scenarios, mock_sunshine)

        # 1. Get app list
        apps = mapper.get_app_list()
        assert len(apps) == 3

        # 2. Verify app properties
        work_app = next(a for a in apps if a["appId"] == "work")
        assert work_app["name"] == "Work"
        assert work_app["captureSource"] == "hdmi-0"
        assert work_app["node_id"] == "workstation1._ozma._udp.local."

        vm_app = next(a for a in apps if a["appId"] == "vm1")
        assert vm_app["name"] == "Ubuntu VM"
        assert vm_app["vncHost"] == "192.168.1.100"
        assert vm_app["vncPort"] == 5900

        media_app = next(a for a in apps if a["appId"] == "media")
        assert media_app["name"] == "Media Server"
        assert "node_id" not in media_app  # No node bound

    def _make_mock_scenarios(self, scenario_list):
        """Create a mock ScenarioManager."""
        mock_mgr = MagicMock()
        mock_mgr.list = MagicMock(return_value=scenario_list)
        mock_mgr.get = MagicMock(side_effect=lambda sid: next(
            (s for s in scenario_list if s.get("id") == sid), None
        ))
        mock_mgr._state = MagicMock()
        mock_mgr._state.nodes = {}
        mock_mgr.activate = AsyncMock()
        return mock_mgr

    def _make_mock_sunshine(self):
        """Create a mock SunshineManager."""
        mock_mgr = MagicMock()
        mock_mgr.enable_node = AsyncMock()
        mock_mgr.get_config = MagicMock(return_value=None)
        return mock_mgr
