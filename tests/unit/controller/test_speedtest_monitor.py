# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Unit tests for SpeedtestMonitor, SpeedtestConfig, and SpeedtestResult."""

import asyncio
import json
import sys
import time
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

sys.path.insert(0, str(Path(__file__).parents[3] / "controller"))

from speedtest_monitor import SpeedtestConfig, SpeedtestMonitor, SpeedtestResult


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _monitor(tmp_path: Path, event_queue=None) -> SpeedtestMonitor:
    return SpeedtestMonitor(
        state_path=tmp_path / "speedtest_state.json",
        event_queue=event_queue,
    )


_SPEEDTEST_CLI_JSON = json.dumps({
    "download": 100_000_000,   # 100 Mbps in bps
    "upload":    20_000_000,   # 20 Mbps in bps
    "ping":      12.34,
    "server": {"name": "Test Server", "host": "speedtest.example.com"},
    "client": {"isp": "Example ISP"},
    "share": "https://www.speedtest.net/result/123.png",
})

_LIBRESPEED_JSON = json.dumps([{
    "download": "95.12",   # already Mbps
    "upload":   "18.75",
    "ping":     "8.5",
    "server": {"name": "Libre Server"},
}])


# ---------------------------------------------------------------------------
# TestSpeedtestResult
# ---------------------------------------------------------------------------

class TestSpeedtestResult:
    def test_to_dict_has_all_fields(self):
        result = SpeedtestResult(
            timestamp=1700000000.0,
            download_mbps=100.0,
            upload_mbps=20.0,
            ping_ms=12.0,
            server_name="Test",
            server_host="test.example.com",
            isp="ISP",
            result_url="https://speedtest.net/result/1",
            tool="speedtest-cli",
        )
        d = result.to_dict()
        for key in ("timestamp", "download_mbps", "upload_mbps", "ping_ms",
                    "server_name", "server_host", "isp", "result_url", "tool"):
            assert key in d

    def test_roundtrip(self):
        result = SpeedtestResult(
            timestamp=1700000000.0,
            download_mbps=95.5,
            upload_mbps=18.2,
            ping_ms=9.1,
            server_name="London Server",
            server_host="lon.example.com",
            isp="BT",
            result_url="",
            tool="librespeed-cli",
        )
        restored = SpeedtestResult.from_dict(result.to_dict())
        assert restored.download_mbps == 95.5
        assert restored.upload_mbps == 18.2
        assert restored.ping_ms == 9.1
        assert restored.server_name == "London Server"
        assert restored.tool == "librespeed-cli"

    def test_defaults(self):
        result = SpeedtestResult.from_dict({
            "timestamp": 1700000000.0,
            "download_mbps": 50.0,
            "upload_mbps": 10.0,
            "ping_ms": 20.0,
        })
        assert result.server_name == ""
        assert result.server_host == ""
        assert result.isp == ""
        assert result.result_url == ""
        assert result.tool == "speedtest-cli"


# ---------------------------------------------------------------------------
# TestSpeedtestConfig
# ---------------------------------------------------------------------------

class TestSpeedtestConfig:
    def test_defaults(self):
        cfg = SpeedtestConfig()
        assert cfg.enabled is False
        assert cfg.interval_hours == 6.0
        assert cfg.tool == "auto"
        assert cfg.iperf3_server == ""
        assert cfg.min_download_mbps == 0.0
        assert cfg.min_upload_mbps == 0.0
        assert cfg.max_ping_ms == 0.0
        assert cfg.history_max == 168

    def test_roundtrip(self):
        cfg = SpeedtestConfig(
            enabled=True,
            interval_hours=12.0,
            tool="speedtest-cli",
            iperf3_server="iperf.example.com",
            min_download_mbps=50.0,
            min_upload_mbps=10.0,
            max_ping_ms=100.0,
            history_max=48,
        )
        restored = SpeedtestConfig.from_dict(cfg.to_dict())
        assert restored.enabled is True
        assert restored.interval_hours == 12.0
        assert restored.tool == "speedtest-cli"
        assert restored.iperf3_server == "iperf.example.com"
        assert restored.min_download_mbps == 50.0
        assert restored.max_ping_ms == 100.0
        assert restored.history_max == 48

    def test_from_dict_defaults_for_missing(self):
        cfg = SpeedtestConfig.from_dict({})
        assert cfg.enabled is False
        assert cfg.interval_hours == 6.0


# ---------------------------------------------------------------------------
# TestParseSpeedtestCli
# ---------------------------------------------------------------------------

class TestParseSpeedtestCli:
    @pytest.mark.asyncio
    async def test_parse_download_mbps(self, tmp_path):
        mon = _monitor(tmp_path)
        mock_proc = MagicMock()
        mock_proc.returncode = 0
        mock_proc.communicate = AsyncMock(return_value=(
            _SPEEDTEST_CLI_JSON.encode(), b""
        ))
        with patch("asyncio.create_subprocess_exec", AsyncMock(return_value=mock_proc)):
            result = await mon._run_speedtest_cli()
        assert result is not None
        assert result.download_mbps == 100.0

    @pytest.mark.asyncio
    async def test_parse_upload_mbps(self, tmp_path):
        mon = _monitor(tmp_path)
        mock_proc = MagicMock()
        mock_proc.returncode = 0
        mock_proc.communicate = AsyncMock(return_value=(
            _SPEEDTEST_CLI_JSON.encode(), b""
        ))
        with patch("asyncio.create_subprocess_exec", AsyncMock(return_value=mock_proc)):
            result = await mon._run_speedtest_cli()
        assert result is not None
        assert result.upload_mbps == 20.0

    @pytest.mark.asyncio
    async def test_parse_ping(self, tmp_path):
        mon = _monitor(tmp_path)
        mock_proc = MagicMock()
        mock_proc.returncode = 0
        mock_proc.communicate = AsyncMock(return_value=(
            _SPEEDTEST_CLI_JSON.encode(), b""
        ))
        with patch("asyncio.create_subprocess_exec", AsyncMock(return_value=mock_proc)):
            result = await mon._run_speedtest_cli()
        assert result is not None
        assert result.ping_ms == 12.34

    @pytest.mark.asyncio
    async def test_parse_server_name(self, tmp_path):
        mon = _monitor(tmp_path)
        mock_proc = MagicMock()
        mock_proc.returncode = 0
        mock_proc.communicate = AsyncMock(return_value=(
            _SPEEDTEST_CLI_JSON.encode(), b""
        ))
        with patch("asyncio.create_subprocess_exec", AsyncMock(return_value=mock_proc)):
            result = await mon._run_speedtest_cli()
        assert result is not None
        assert result.server_name == "Test Server"

    @pytest.mark.asyncio
    async def test_returns_none_on_nonzero_exit(self, tmp_path):
        mon = _monitor(tmp_path)
        mock_proc = MagicMock()
        mock_proc.returncode = 1
        mock_proc.communicate = AsyncMock(return_value=(b"", b"error"))
        with patch("asyncio.create_subprocess_exec", AsyncMock(return_value=mock_proc)):
            result = await mon._run_speedtest_cli()
        assert result is None


# ---------------------------------------------------------------------------
# TestParseLibrespeed
# ---------------------------------------------------------------------------

class TestParseLibrespeed:
    @pytest.mark.asyncio
    async def test_parse_download_already_mbps(self, tmp_path):
        mon = _monitor(tmp_path)
        mock_proc = MagicMock()
        mock_proc.returncode = 0
        mock_proc.communicate = AsyncMock(return_value=(
            _LIBRESPEED_JSON.encode(), b""
        ))
        with patch("asyncio.create_subprocess_exec", AsyncMock(return_value=mock_proc)):
            result = await mon._run_librespeed()
        assert result is not None
        assert result.download_mbps == 95.12

    @pytest.mark.asyncio
    async def test_parse_upload_librespeed(self, tmp_path):
        mon = _monitor(tmp_path)
        mock_proc = MagicMock()
        mock_proc.returncode = 0
        mock_proc.communicate = AsyncMock(return_value=(
            _LIBRESPEED_JSON.encode(), b""
        ))
        with patch("asyncio.create_subprocess_exec", AsyncMock(return_value=mock_proc)):
            result = await mon._run_librespeed()
        assert result is not None
        assert result.upload_mbps == 18.75

    @pytest.mark.asyncio
    async def test_parse_ping_librespeed(self, tmp_path):
        mon = _monitor(tmp_path)
        mock_proc = MagicMock()
        mock_proc.returncode = 0
        mock_proc.communicate = AsyncMock(return_value=(
            _LIBRESPEED_JSON.encode(), b""
        ))
        with patch("asyncio.create_subprocess_exec", AsyncMock(return_value=mock_proc)):
            result = await mon._run_librespeed()
        assert result is not None
        assert result.ping_ms == 8.5

    @pytest.mark.asyncio
    async def test_librespeed_tool_label(self, tmp_path):
        mon = _monitor(tmp_path)
        mock_proc = MagicMock()
        mock_proc.returncode = 0
        mock_proc.communicate = AsyncMock(return_value=(
            _LIBRESPEED_JSON.encode(), b""
        ))
        with patch("asyncio.create_subprocess_exec", AsyncMock(return_value=mock_proc)):
            result = await mon._run_librespeed()
        assert result is not None
        assert result.tool == "librespeed-cli"


# ---------------------------------------------------------------------------
# TestHistory
# ---------------------------------------------------------------------------

class TestHistory:
    def _make_result(self, dl: float = 100.0, ts: float | None = None) -> SpeedtestResult:
        return SpeedtestResult(
            timestamp=ts or time.time(),
            download_mbps=dl,
            upload_mbps=20.0,
            ping_ms=10.0,
        )

    def test_store_result_appends(self, tmp_path):
        mon = _monitor(tmp_path)
        mon._store_result(self._make_result(100.0))
        mon._store_result(self._make_result(90.0))
        assert len(mon._history) == 2

    def test_history_trimmed_to_history_max(self, tmp_path):
        mon = _monitor(tmp_path)
        mon._config.history_max = 5
        for i in range(10):
            mon._store_result(self._make_result(float(i)))
        assert len(mon._history) == 5

    def test_history_trimmed_keeps_most_recent(self, tmp_path):
        mon = _monitor(tmp_path)
        mon._config.history_max = 3
        for i in range(5):
            mon._store_result(self._make_result(float(i)))
        # Should keep the last 3
        values = [r.download_mbps for r in mon._history]
        assert values == [2.0, 3.0, 4.0]

    def test_get_history_respects_limit(self, tmp_path):
        mon = _monitor(tmp_path)
        for i in range(10):
            mon._store_result(self._make_result(float(i)))
        history = mon.get_history(limit=3)
        assert len(history) == 3

    def test_get_history_returns_dicts(self, tmp_path):
        mon = _monitor(tmp_path)
        mon._store_result(self._make_result())
        history = mon.get_history()
        assert isinstance(history[0], dict)
        assert "download_mbps" in history[0]


# ---------------------------------------------------------------------------
# TestThresholds
# ---------------------------------------------------------------------------

class TestThresholds:
    @pytest.mark.asyncio
    async def test_fires_event_when_download_below_min(self, tmp_path):
        q = asyncio.Queue()
        mon = _monitor(tmp_path, event_queue=q)
        mon._config.min_download_mbps = 50.0

        result = SpeedtestResult(
            timestamp=time.time(),
            download_mbps=30.0,  # below threshold
            upload_mbps=20.0,
            ping_ms=10.0,
        )
        mon._check_thresholds(result)
        await asyncio.gather(*asyncio.all_tasks() - {asyncio.current_task()},
                             return_exceptions=True)
        events = []
        while not q.empty():
            events.append(await q.get())
        assert any(e["type"] == "speedtest.threshold_breach" and e["metric"] == "download"
                   for e in events)

    @pytest.mark.asyncio
    async def test_no_event_when_threshold_zero(self, tmp_path):
        q = asyncio.Queue()
        mon = _monitor(tmp_path, event_queue=q)
        mon._config.min_download_mbps = 0.0  # disabled

        result = SpeedtestResult(
            timestamp=time.time(),
            download_mbps=1.0,
            upload_mbps=1.0,
            ping_ms=10.0,
        )
        mon._check_thresholds(result)
        await asyncio.gather(*asyncio.all_tasks() - {asyncio.current_task()},
                             return_exceptions=True)
        events = []
        while not q.empty():
            events.append(await q.get())
        assert len(events) == 0

    @pytest.mark.asyncio
    async def test_fires_event_when_ping_too_high(self, tmp_path):
        q = asyncio.Queue()
        mon = _monitor(tmp_path, event_queue=q)
        mon._config.max_ping_ms = 50.0

        result = SpeedtestResult(
            timestamp=time.time(),
            download_mbps=100.0,
            upload_mbps=20.0,
            ping_ms=100.0,  # above threshold
        )
        mon._check_thresholds(result)
        await asyncio.gather(*asyncio.all_tasks() - {asyncio.current_task()},
                             return_exceptions=True)
        events = []
        while not q.empty():
            events.append(await q.get())
        assert any(e["type"] == "speedtest.threshold_breach" and e["metric"] == "ping"
                   for e in events)

    @pytest.mark.asyncio
    async def test_no_ping_event_when_ping_ok(self, tmp_path):
        q = asyncio.Queue()
        mon = _monitor(tmp_path, event_queue=q)
        mon._config.max_ping_ms = 100.0

        result = SpeedtestResult(
            timestamp=time.time(),
            download_mbps=100.0,
            upload_mbps=20.0,
            ping_ms=20.0,  # well below threshold
        )
        mon._check_thresholds(result)
        await asyncio.gather(*asyncio.all_tasks() - {asyncio.current_task()},
                             return_exceptions=True)
        events = []
        while not q.empty():
            events.append(await q.get())
        ping_events = [e for e in events if e.get("metric") == "ping"]
        assert len(ping_events) == 0


# ---------------------------------------------------------------------------
# TestRunNow
# ---------------------------------------------------------------------------

class TestRunNow:
    @pytest.mark.asyncio
    async def test_returns_none_when_already_running(self, tmp_path):
        mon = _monitor(tmp_path)
        mon._running = True
        result = await mon.run_now()
        assert result is None

    @pytest.mark.asyncio
    async def test_runs_tool_when_not_running(self, tmp_path):
        mon = _monitor(tmp_path)
        mon._running = False
        mock_result = SpeedtestResult(
            timestamp=time.time(),
            download_mbps=100.0,
            upload_mbps=20.0,
            ping_ms=10.0,
        )
        with patch.object(mon, "_execute", AsyncMock(return_value=mock_result)):
            result = await mon.run_now()
        assert result is mock_result


# ---------------------------------------------------------------------------
# TestStatus
# ---------------------------------------------------------------------------

class TestStatus:
    def test_get_status_has_expected_fields(self, tmp_path):
        mon = _monitor(tmp_path)
        status = mon.get_status()
        for key in ("enabled", "tool", "running", "latest",
                    "avg_download_mbps", "avg_upload_mbps", "avg_ping_ms",
                    "result_count"):
            assert key in status

    def test_status_latest_none_when_empty(self, tmp_path):
        mon = _monitor(tmp_path)
        assert mon.get_status()["latest"] is None

    def test_status_result_count(self, tmp_path):
        mon = _monitor(tmp_path)
        for i in range(3):
            mon._history.append(SpeedtestResult(
                timestamp=time.time(),
                download_mbps=100.0,
                upload_mbps=20.0,
                ping_ms=10.0,
            ))
        assert mon.get_status()["result_count"] == 3

    def test_avg_download_calculated_from_recent_24h(self, tmp_path):
        mon = _monitor(tmp_path)
        now = time.time()
        # Two results in last 24h
        mon._history.append(SpeedtestResult(
            timestamp=now - 3600, download_mbps=80.0, upload_mbps=10.0, ping_ms=10.0))
        mon._history.append(SpeedtestResult(
            timestamp=now - 7200, download_mbps=100.0, upload_mbps=20.0, ping_ms=20.0))
        # One old result (outside 24h)
        mon._history.append(SpeedtestResult(
            timestamp=now - 90000, download_mbps=50.0, upload_mbps=5.0, ping_ms=5.0))
        status = mon.get_status()
        assert status["avg_download_mbps"] == 90.0  # (80+100)/2


# ---------------------------------------------------------------------------
# TestPersistence
# ---------------------------------------------------------------------------

class TestPersistence:
    def test_history_survives_reload(self, tmp_path):
        mon = _monitor(tmp_path)
        for dl in (100.0, 90.0, 80.0):
            mon._store_result(SpeedtestResult(
                timestamp=time.time(),
                download_mbps=dl,
                upload_mbps=20.0,
                ping_ms=10.0,
            ))

        mon2 = _monitor(tmp_path)
        mon2._load()
        assert len(mon2._history) == 3

    def test_config_survives_reload(self, tmp_path):
        mon = _monitor(tmp_path)
        mon._config.enabled = True
        mon._config.min_download_mbps = 25.0
        mon._save()

        mon2 = _monitor(tmp_path)
        mon2._load()
        assert mon2.get_config().enabled is True
        assert mon2.get_config().min_download_mbps == 25.0

    def test_state_file_mode_600(self, tmp_path):
        mon = _monitor(tmp_path)
        mon._save()
        state_file = tmp_path / "speedtest_state.json"
        assert state_file.exists()
        mode = oct(state_file.stat().st_mode)[-3:]
        assert mode == "600"


# ---------------------------------------------------------------------------
# TestLifecycle
# ---------------------------------------------------------------------------

class TestLifecycle:
    @pytest.mark.asyncio
    async def test_start_creates_task(self, tmp_path):
        mon = _monitor(tmp_path)
        mon._config.enabled = False  # don't actually run
        with patch.object(mon, "_load", MagicMock()):
            await mon.start()
        assert mon._task is not None
        mon._task.cancel()
        try:
            await mon._task
        except asyncio.CancelledError:
            pass

    @pytest.mark.asyncio
    async def test_stop_cancels_task(self, tmp_path):
        mon = _monitor(tmp_path)
        mon._config.enabled = False
        with patch.object(mon, "_load", MagicMock()):
            await mon.start()
        task = mon._task
        await mon.stop()
        assert task.done()
        assert mon._task is None

    @pytest.mark.asyncio
    async def test_set_config_updates_field(self, tmp_path):
        mon = _monitor(tmp_path)
        await mon.set_config(min_download_mbps=50.0, interval_hours=3.0)
        assert mon.get_config().min_download_mbps == 50.0
        assert mon.get_config().interval_hours == 3.0

    @pytest.mark.asyncio
    async def test_set_config_raises_for_unknown_key(self, tmp_path):
        mon = _monitor(tmp_path)
        with pytest.raises(ValueError, match="Unknown config key"):
            await mon.set_config(nonexistent=True)
