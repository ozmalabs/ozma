# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Unit tests for UPSMonitor, UPSConfig, and UPSStatus."""

import asyncio
import json
import sys
import time
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

sys.path.insert(0, str(Path(__file__).parents[3] / "controller"))

from ups_monitor import UPSConfig, UPSMonitor, UPSStatus


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _monitor(tmp_path: Path, event_queue=None) -> UPSMonitor:
    return UPSMonitor(
        state_path=tmp_path / "ups_state.json",
        event_queue=event_queue,
    )


_SAMPLE_UPSC = (
    "battery.charge: 85\n"
    "battery.runtime: 3600\n"
    "ups.status: OL\n"
    "ups.load: 23\n"
    "battery.voltage: 13.5\n"
    "input.voltage: 230.0\n"
    "output.voltage: 230.0\n"
    "device.model: SmartUPS 1500\n"
)

_SAMPLE_UPSC_ON_BATTERY = (
    "battery.charge: 60\n"
    "battery.runtime: 1800\n"
    "ups.status: OB\n"
    "ups.load: 30\n"
)


# ---------------------------------------------------------------------------
# TestUPSConfig
# ---------------------------------------------------------------------------

class TestUPSConfig:
    def test_defaults(self):
        cfg = UPSConfig()
        assert cfg.enabled is False
        assert cfg.nut_host == "localhost"
        assert cfg.nut_port == 3493
        assert cfg.ups_name == "ups"
        assert cfg.poll_interval_seconds == 30
        assert cfg.battery_warn_pct == 50
        assert cfg.battery_critical_pct == 25
        assert cfg.battery_shutdown_pct == 10
        assert cfg.runtime_warn_minutes == 10
        assert cfg.auto_shutdown is True

    def test_roundtrip(self):
        cfg = UPSConfig(
            enabled=True,
            nut_host="192.168.1.5",
            nut_port=3493,
            ups_name="myups",
            poll_interval_seconds=60,
            battery_warn_pct=40,
            battery_critical_pct=20,
            battery_shutdown_pct=5,
            runtime_warn_minutes=5,
            auto_shutdown=False,
        )
        restored = UPSConfig.from_dict(cfg.to_dict())
        assert restored.enabled is True
        assert restored.nut_host == "192.168.1.5"
        assert restored.ups_name == "myups"
        assert restored.poll_interval_seconds == 60
        assert restored.battery_warn_pct == 40
        assert restored.auto_shutdown is False

    def test_from_dict_ignores_unknown_keys(self):
        data = {"enabled": True, "unknown_field": "value", "ups_name": "test"}
        cfg = UPSConfig.from_dict(data)
        assert cfg.enabled is True
        assert cfg.ups_name == "test"
        assert not hasattr(cfg, "unknown_field")

    def test_to_dict_has_all_fields(self):
        cfg = UPSConfig()
        d = cfg.to_dict()
        for key in ("enabled", "nut_host", "nut_port", "ups_name",
                    "poll_interval_seconds", "battery_warn_pct",
                    "battery_critical_pct", "battery_shutdown_pct",
                    "runtime_warn_minutes", "auto_shutdown"):
            assert key in d


# ---------------------------------------------------------------------------
# TestUPSStatus
# ---------------------------------------------------------------------------

class TestUPSStatus:
    def test_to_dict_has_all_expected_keys(self):
        status = UPSStatus(ups_name="ups")
        d = status.to_dict()
        for key in ("ups_name", "model", "status", "on_battery", "battery_pct",
                    "battery_voltage", "runtime_seconds", "load_pct",
                    "input_voltage", "output_voltage", "temperature",
                    "last_polled", "reachable"):
            assert key in d

    def test_on_battery_field(self):
        status = UPSStatus(ups_name="ups", on_battery=True)
        assert status.to_dict()["on_battery"] is True

    def test_on_battery_false_by_default(self):
        status = UPSStatus(ups_name="ups")
        assert status.on_battery is False

    def test_reachable_false(self):
        status = UPSStatus(ups_name="ups", reachable=False)
        assert status.to_dict()["reachable"] is False


# ---------------------------------------------------------------------------
# TestParseUpscOutput
# ---------------------------------------------------------------------------

class TestParseUpscOutput:
    def test_battery_charge_parsed(self, tmp_path):
        mon = _monitor(tmp_path)
        raw = mon._parse_upsc_output(_SAMPLE_UPSC)
        assert raw["battery.charge"] == "85"

    def test_battery_runtime_parsed(self, tmp_path):
        mon = _monitor(tmp_path)
        raw = mon._parse_upsc_output(_SAMPLE_UPSC)
        assert raw["battery.runtime"] == "3600"

    def test_ups_status_parsed(self, tmp_path):
        mon = _monitor(tmp_path)
        raw = mon._parse_upsc_output(_SAMPLE_UPSC)
        assert raw["ups.status"] == "OL"

    def test_ups_load_parsed(self, tmp_path):
        mon = _monitor(tmp_path)
        raw = mon._parse_upsc_output(_SAMPLE_UPSC)
        assert raw["ups.load"] == "23"

    def test_build_status_from_ol(self, tmp_path):
        mon = _monitor(tmp_path)
        raw = mon._parse_upsc_output(_SAMPLE_UPSC)
        status = mon._build_status(raw)
        assert status.battery_pct == 85.0
        assert status.runtime_seconds == 3600
        assert status.status == "OL"
        assert status.on_battery is False
        assert status.reachable is True

    def test_build_status_on_battery(self, tmp_path):
        mon = _monitor(tmp_path)
        raw = mon._parse_upsc_output(_SAMPLE_UPSC_ON_BATTERY)
        status = mon._build_status(raw)
        assert status.on_battery is True
        assert status.battery_pct == 60.0

    def test_parse_empty_output(self, tmp_path):
        mon = _monitor(tmp_path)
        raw = mon._parse_upsc_output("")
        assert raw == {}

    def test_parse_ignores_lines_without_colon_space(self, tmp_path):
        mon = _monitor(tmp_path)
        raw = mon._parse_upsc_output("no-colon-here\nbattery.charge: 90\n")
        assert "battery.charge" in raw
        assert "no-colon-here" not in raw


# ---------------------------------------------------------------------------
# TestThresholds
# ---------------------------------------------------------------------------

class TestThresholds:
    @pytest.mark.asyncio
    async def test_on_battery_fires_event(self, tmp_path):
        q = asyncio.Queue()
        mon = _monitor(tmp_path, event_queue=q)
        mon._config.enabled = True
        mon._first_poll = False
        mon._prev_on_battery = False

        status = UPSStatus(ups_name="ups", on_battery=True, battery_pct=80.0,
                           runtime_seconds=3600, reachable=True)
        mon._check_thresholds(status)
        # drain tasks
        await asyncio.gather(*asyncio.all_tasks() - {asyncio.current_task()},
                             return_exceptions=True)
        events = []
        while not q.empty():
            events.append(await q.get())
        types = {e["type"] for e in events}
        assert "ups.on_battery" in types

    @pytest.mark.asyncio
    async def test_ac_restored_fires_event(self, tmp_path):
        q = asyncio.Queue()
        mon = _monitor(tmp_path, event_queue=q)
        mon._first_poll = False
        mon._prev_on_battery = True

        status = UPSStatus(ups_name="ups", on_battery=False, battery_pct=90.0,
                           reachable=True)
        mon._check_thresholds(status)
        await asyncio.gather(*asyncio.all_tasks() - {asyncio.current_task()},
                             return_exceptions=True)
        events = []
        while not q.empty():
            events.append(await q.get())
        types = {e["type"] for e in events}
        assert "ups.restored" in types

    @pytest.mark.asyncio
    async def test_battery_warn_fires_event(self, tmp_path):
        q = asyncio.Queue()
        mon = _monitor(tmp_path, event_queue=q)
        mon._first_poll = False
        mon._prev_on_battery = True
        mon._config.battery_warn_pct = 50

        status = UPSStatus(ups_name="ups", on_battery=True, battery_pct=45.0,
                           runtime_seconds=600, reachable=True)
        mon._check_thresholds(status)
        await asyncio.gather(*asyncio.all_tasks() - {asyncio.current_task()},
                             return_exceptions=True)
        events = []
        while not q.empty():
            events.append(await q.get())
        types = {e["type"] for e in events}
        assert "ups.battery_low" in types

    @pytest.mark.asyncio
    async def test_no_duplicate_warn_events(self, tmp_path):
        q = asyncio.Queue()
        mon = _monitor(tmp_path, event_queue=q)
        mon._first_poll = False
        mon._prev_on_battery = True
        mon._config.battery_warn_pct = 50
        mon._last_alert_level = "warn"  # already warned

        status = UPSStatus(ups_name="ups", on_battery=True, battery_pct=45.0,
                           runtime_seconds=600, reachable=True)
        mon._check_thresholds(status)
        await asyncio.gather(*asyncio.all_tasks() - {asyncio.current_task()},
                             return_exceptions=True)
        events = []
        while not q.empty():
            events.append(await q.get())
        # No new battery_low event because already at warn level
        low_events = [e for e in events if e["type"] == "ups.battery_low"]
        assert len(low_events) == 0

    @pytest.mark.asyncio
    async def test_unreachable_fires_unreachable_event(self, tmp_path):
        q = asyncio.Queue()
        mon = _monitor(tmp_path, event_queue=q)
        mon._prev_reachable = True

        status = UPSStatus(ups_name="ups", reachable=False)
        mon._check_thresholds(status)
        await asyncio.gather(*asyncio.all_tasks() - {asyncio.current_task()},
                             return_exceptions=True)
        events = []
        while not q.empty():
            events.append(await q.get())
        types = {e["type"] for e in events}
        assert "ups.unreachable" in types


# ---------------------------------------------------------------------------
# TestConfig
# ---------------------------------------------------------------------------

class TestConfig:
    @pytest.mark.asyncio
    async def test_set_config_updates_fields(self, tmp_path):
        mon = _monitor(tmp_path)
        await mon.set_config(nut_host="10.0.0.5", poll_interval_seconds=15)
        assert mon.get_config().nut_host == "10.0.0.5"
        assert mon.get_config().poll_interval_seconds == 15

    @pytest.mark.asyncio
    async def test_set_config_raises_for_unknown_field(self, tmp_path):
        mon = _monitor(tmp_path)
        with pytest.raises(ValueError, match="Unknown UPSConfig field"):
            await mon.set_config(nonexistent_field=True)

    def test_get_status_has_expected_fields(self, tmp_path):
        mon = _monitor(tmp_path)
        status = mon.get_status()
        assert "config" in status

    def test_get_status_includes_config(self, tmp_path):
        mon = _monitor(tmp_path)
        status = mon.get_status()
        assert "enabled" in status["config"]


# ---------------------------------------------------------------------------
# TestPersistence
# ---------------------------------------------------------------------------

class TestPersistence:
    @pytest.mark.asyncio
    async def test_config_survives_reload(self, tmp_path):
        mon = _monitor(tmp_path)
        await mon.set_config(nut_host="192.168.1.99", ups_name="powerwall", enabled=False)

        mon2 = _monitor(tmp_path)
        assert mon2.get_config().nut_host == "192.168.1.99"
        assert mon2.get_config().ups_name == "powerwall"

    def test_state_file_mode_600(self, tmp_path):
        mon = _monitor(tmp_path)
        mon._save()
        state_file = tmp_path / "ups_state.json"
        assert state_file.exists()
        mode = oct(state_file.stat().st_mode)[-3:]
        assert mode == "600"

    def test_state_file_valid_json(self, tmp_path):
        mon = _monitor(tmp_path)
        mon._save()
        data = json.loads((tmp_path / "ups_state.json").read_text())
        assert "config" in data


# ---------------------------------------------------------------------------
# TestLifecycle
# ---------------------------------------------------------------------------

class TestLifecycle:
    @pytest.mark.asyncio
    async def test_start_creates_task_when_enabled(self, tmp_path):
        mon = _monitor(tmp_path)
        mon._config.enabled = True
        with patch.object(mon, "poll_now", AsyncMock(return_value=None)):
            await mon.start()
            assert mon._poll_task is not None
            assert not mon._poll_task.done()
            await mon.stop()

    @pytest.mark.asyncio
    async def test_start_does_not_create_task_when_disabled(self, tmp_path):
        mon = _monitor(tmp_path)
        mon._config.enabled = False
        await mon.start()
        assert mon._poll_task is None

    @pytest.mark.asyncio
    async def test_stop_cancels_task(self, tmp_path):
        mon = _monitor(tmp_path)
        mon._config.enabled = True
        with patch.object(mon, "poll_now", AsyncMock(return_value=None)):
            await mon.start()
            task = mon._poll_task
            await mon.stop()
        assert task.done()
        assert mon._poll_task is None
