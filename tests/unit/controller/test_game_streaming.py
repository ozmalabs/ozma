# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Unit tests for controller/game_streaming.py."""

from __future__ import annotations

import asyncio
import json
import sys
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

sys.path.insert(0, str(Path(__file__).parents[3] / "controller"))

from game_streaming import (
    SunshineConfig,
    SunshineInstance,
    SunshineManager,
    _build_sunshine_conf,
    _safe_id,
    _SUNSHINE_BASE_PORT,
    _PORT_STRIDE,
)


def run(coro):
    return asyncio.run(coro)


# ---------------------------------------------------------------------------
# SunshineConfig
# ---------------------------------------------------------------------------

class TestSunshineConfig:
    def test_defaults(self):
        cfg = SunshineConfig(node_id="vm1")
        assert cfg.enabled is False
        assert cfg.encoder == "auto"
        assert cfg.codec == "h264"
        assert cfg.fps == 60
        assert cfg.bitrate_kbps == 10_000
        assert cfg.port_offset == 0

    def test_roundtrip(self):
        cfg = SunshineConfig(
            node_id="test-node",
            enabled=True,
            encoder="nvenc",
            codec="h265",
            fps=30,
            bitrate_kbps=20_000,
            port_offset=100,
            resolutions=["1920x1080"],
        )
        d = cfg.to_dict()
        cfg2 = SunshineConfig.from_dict(d)
        assert cfg2.node_id == "test-node"
        assert cfg2.enabled is True
        assert cfg2.encoder == "nvenc"
        assert cfg2.codec == "h265"
        assert cfg2.fps == 30
        assert cfg2.bitrate_kbps == 20_000
        assert cfg2.port_offset == 100

    def test_from_dict_defaults(self):
        cfg = SunshineConfig.from_dict({"node_id": "x"})
        assert cfg.enabled is False
        assert cfg.encoder == "auto"
        assert cfg.fps == 60


# ---------------------------------------------------------------------------
# SunshineInstance
# ---------------------------------------------------------------------------

class TestSunshineInstance:
    def _make(self, port_offset=0, remote=False) -> SunshineInstance:
        cfg = SunshineConfig(node_id="n1", port_offset=port_offset)
        return SunshineInstance(
            node_id="n1", config=cfg,
            config_dir=Path("/tmp/sunshine/n1"),
            remote=remote,
            remote_host="192.168.1.50" if remote else "",
            remote_api_port=47990 if remote else 47990,
        )

    def test_api_port_local(self):
        inst = self._make(port_offset=0)
        assert inst.api_port == _SUNSHINE_BASE_PORT + 6

    def test_api_port_offset(self):
        inst = self._make(port_offset=100)
        assert inst.api_port == _SUNSHINE_BASE_PORT + 100 + 6

    def test_api_port_remote(self):
        inst = self._make(remote=True)
        assert inst.api_port == 47990

    def test_stream_base_port_offset(self):
        inst = self._make(port_offset=200)
        assert inst.stream_base_port == _SUNSHINE_BASE_PORT + 200

    def test_to_dict_keys(self):
        inst = self._make()
        d = inst.to_dict()
        for key in ("node_id", "state", "error", "pid", "started_at",
                    "paired_clients", "restarts", "remote",
                    "api_port", "stream_base_port", "config"):
            assert key in d, f"Missing key: {key}"


# ---------------------------------------------------------------------------
# Config file generation
# ---------------------------------------------------------------------------

class TestBuildSunshineConf:
    def test_port_in_conf(self, tmp_path):
        cfg = SunshineConfig(node_id="n1", port_offset=0)
        conf = _build_sunshine_conf(cfg, tmp_path)
        assert f"port = {_SUNSHINE_BASE_PORT}" in conf

    def test_port_offset(self, tmp_path):
        cfg = SunshineConfig(node_id="n1", port_offset=200)
        conf = _build_sunshine_conf(cfg, tmp_path)
        assert f"port = {_SUNSHINE_BASE_PORT + 200}" in conf

    def test_encoder_in_conf(self, tmp_path):
        cfg = SunshineConfig(node_id="n1", encoder="nvenc")
        conf = _build_sunshine_conf(cfg, tmp_path)
        assert "encoder = nvenc" in conf

    def test_codec_in_conf(self, tmp_path):
        cfg = SunshineConfig(node_id="n1", codec="h265")
        conf = _build_sunshine_conf(cfg, tmp_path)
        assert "codec = h265" in conf

    def test_bitrate_in_conf(self, tmp_path):
        cfg = SunshineConfig(node_id="n1", bitrate_kbps=15_000)
        conf = _build_sunshine_conf(cfg, tmp_path)
        assert "bitrate = 15000" in conf

    def test_fps_in_conf(self, tmp_path):
        cfg = SunshineConfig(node_id="n1", fps=30)
        conf = _build_sunshine_conf(cfg, tmp_path)
        assert "fps = 30" in conf

    def test_resolutions_in_conf(self, tmp_path):
        cfg = SunshineConfig(node_id="n1", resolutions=["1920x1080", "2560x1440"])
        conf = _build_sunshine_conf(cfg, tmp_path)
        assert "1920x1080" in conf
        assert "2560x1440" in conf

    def test_v4l2_device_in_conf(self, tmp_path):
        cfg = SunshineConfig(node_id="n1", v4l2_device="/dev/video0")
        conf = _build_sunshine_conf(cfg, tmp_path)
        assert "capture = v4l2" in conf
        assert "/dev/video0" in conf

    def test_capture_backend_explicit(self, tmp_path):
        cfg = SunshineConfig(node_id="n1", capture="kms")
        conf = _build_sunshine_conf(cfg, tmp_path)
        assert "capture = kms" in conf

    def test_audio_sink_in_conf(self, tmp_path):
        cfg = SunshineConfig(node_id="n1", audio_sink="ozma-node-sink")
        conf = _build_sunshine_conf(cfg, tmp_path)
        assert "audio_sink = ozma-node-sink" in conf

    def test_paths_include_config_dir(self, tmp_path):
        cfg = SunshineConfig(node_id="n1")
        conf = _build_sunshine_conf(cfg, tmp_path)
        assert str(tmp_path) in conf

    def test_node_id_in_comment(self, tmp_path):
        cfg = SunshineConfig(node_id="mynode")
        conf = _build_sunshine_conf(cfg, tmp_path)
        assert "mynode" in conf

    def test_lan_origin_restriction(self, tmp_path):
        cfg = SunshineConfig(node_id="n1")
        conf = _build_sunshine_conf(cfg, tmp_path)
        assert "lan" in conf   # origin_web_ui_allowed = lan


# ---------------------------------------------------------------------------
# SunshineManager — persistence
# ---------------------------------------------------------------------------

class TestSunshineManagerPersistence:
    def _mgr(self, tmp_path) -> SunshineManager:
        return SunshineManager(data_dir=tmp_path / "sunshine")

    def test_save_and_load_config(self, tmp_path):
        mgr = self._mgr(tmp_path)
        mgr._configs["n1"] = SunshineConfig(
            node_id="n1", enabled=True, encoder="vaapi", port_offset=0
        )
        mgr._next_port_offset = 1
        mgr._save()

        mgr2 = self._mgr(tmp_path)
        assert "n1" in mgr2._configs
        assert mgr2._configs["n1"].encoder == "vaapi"
        assert mgr2._next_port_offset == 1

    def test_file_permissions(self, tmp_path):
        mgr = self._mgr(tmp_path)
        mgr._save()
        state_path = tmp_path / "sunshine" / "sunshine_state.json"
        assert oct(state_path.stat().st_mode)[-3:] == "600"

    def test_load_missing_no_error(self, tmp_path):
        mgr = self._mgr(tmp_path)
        # _load is called in __init__ — should not raise
        assert mgr._configs == {}
        assert mgr._next_port_offset == 0


# ---------------------------------------------------------------------------
# SunshineManager — port allocation
# ---------------------------------------------------------------------------

class TestPortAllocation:
    def _mgr(self, tmp_path) -> SunshineManager:
        return SunshineManager(data_dir=tmp_path / "sunshine")

    def test_unique_offsets(self, tmp_path):
        mgr = self._mgr(tmp_path)
        o1 = mgr._allocate_port_offset("node-a")
        o2 = mgr._allocate_port_offset("node-b")
        assert o1 != o2

    def test_offset_stride(self, tmp_path):
        mgr = self._mgr(tmp_path)
        o1 = mgr._allocate_port_offset("node-a")
        o2 = mgr._allocate_port_offset("node-b")
        assert abs(o2 - o1) == _PORT_STRIDE

    def test_same_node_same_offset(self, tmp_path):
        mgr = self._mgr(tmp_path)
        mgr._configs["n1"] = SunshineConfig(node_id="n1", port_offset=500)
        o = mgr._allocate_port_offset("n1")
        assert o == 500


# ---------------------------------------------------------------------------
# SunshineManager — remote registration
# ---------------------------------------------------------------------------

class TestRemoteRegistration:
    def _mgr(self, tmp_path) -> SunshineManager:
        return SunshineManager(data_dir=tmp_path / "sunshine")

    def test_register_remote(self, tmp_path):
        mgr = self._mgr(tmp_path)
        mgr.register_remote("desktop-1", "192.168.1.50", 47990)
        inst = mgr._instances.get("desktop-1")
        assert inst is not None
        assert inst.remote is True
        assert inst.remote_host == "192.168.1.50"
        assert inst.state == "running"

    def test_register_remote_creates_config(self, tmp_path):
        mgr = self._mgr(tmp_path)
        mgr.register_remote("desktop-2", "10.0.0.1", 47990)
        assert "desktop-2" in mgr._configs

    def test_get_status_remote(self, tmp_path):
        mgr = self._mgr(tmp_path)
        mgr.register_remote("d1", "1.2.3.4", 47990)
        status = mgr.get_status("d1")
        assert status is not None
        assert status["remote"] is True

    def test_get_status_unknown_node(self, tmp_path):
        mgr = self._mgr(tmp_path)
        assert mgr.get_status("unknown") is None

    def test_unregister_node(self, tmp_path):
        mgr = self._mgr(tmp_path)
        mgr.register_remote("d1", "1.2.3.4", 47990)
        mgr.unregister_node("d1")
        assert "d1" not in mgr._instances


# ---------------------------------------------------------------------------
# SunshineManager — is_available / moonlight_address
# ---------------------------------------------------------------------------

class TestAvailability:
    def _mgr(self, tmp_path) -> SunshineManager:
        return SunshineManager(data_dir=tmp_path / "sunshine")

    def test_not_available_without_binary(self, tmp_path):
        mgr = self._mgr(tmp_path)
        mgr._sunshine_binary = None
        assert mgr.is_available() is False

    def test_available_with_binary(self, tmp_path):
        mgr = self._mgr(tmp_path)
        mgr._sunshine_binary = "/usr/bin/sunshine"
        assert mgr.is_available() is True

    def test_moonlight_address_not_running(self, tmp_path):
        mgr = self._mgr(tmp_path)
        assert mgr.moonlight_address("no-node") is None

    def test_moonlight_address_local(self, tmp_path):
        mgr = self._mgr(tmp_path)
        mgr.register_remote("d1", "192.168.1.100", 47990)
        # Remote instance always has a state=running
        addr = mgr.moonlight_address("d1")
        assert "192.168.1.100" in addr

    def test_moonlight_address_remote_host(self, tmp_path):
        mgr = self._mgr(tmp_path)
        mgr.register_remote("d1", "my.host.local", 47990)
        addr = mgr.moonlight_address("d1")
        assert "my.host.local" in addr
        assert str(_SUNSHINE_BASE_PORT) in addr


# ---------------------------------------------------------------------------
# SunshineManager — enable_node (no Sunshine binary)
# ---------------------------------------------------------------------------

class TestEnableNode:
    def _mgr(self, tmp_path) -> SunshineManager:
        mgr = SunshineManager(data_dir=tmp_path / "sunshine")
        mgr._sunshine_binary = None  # no binary — error path
        return mgr

    def test_enable_without_binary_returns_error(self, tmp_path):
        mgr = self._mgr(tmp_path)
        result = run(mgr.enable_node("vm1"))
        assert result.get("state") == "error"
        assert "not found" in result.get("error", "").lower()

    def test_enable_creates_config(self, tmp_path):
        mgr = self._mgr(tmp_path)
        run(mgr.enable_node("vm1", encoder="software", fps=30))
        assert "vm1" in mgr._configs
        assert mgr._configs["vm1"].fps == 30
        assert mgr._configs["vm1"].encoder == "software"

    def test_enable_saves_state(self, tmp_path):
        mgr = self._mgr(tmp_path)
        run(mgr.enable_node("vm1"))
        state_path = tmp_path / "sunshine" / "sunshine_state.json"
        assert state_path.exists()

    def test_disable_node(self, tmp_path):
        mgr = self._mgr(tmp_path)
        mgr._configs["vm1"] = SunshineConfig(node_id="vm1", enabled=True)
        run(mgr.disable_node("vm1"))
        assert mgr._configs["vm1"].enabled is False


# ---------------------------------------------------------------------------
# SunshineManager — enable_node (with mock binary)
# ---------------------------------------------------------------------------

class TestEnableNodeWithBinary:
    def _mgr(self, tmp_path) -> SunshineManager:
        mgr = SunshineManager(data_dir=tmp_path / "sunshine")
        mgr._sunshine_binary = "/usr/bin/sunshine"
        mgr._default_encoder = "software"
        mgr._default_capture = "kms"
        return mgr

    def test_enable_starts_task(self, tmp_path):
        mgr = self._mgr(tmp_path)
        initial_tasks = len(mgr._tasks)

        async def _run():
            with patch("asyncio.create_subprocess_exec", new_callable=AsyncMock) as mock_exec:
                mock_proc = MagicMock()
                mock_proc.pid = 12345
                mock_proc.returncode = None
                mock_proc.wait = AsyncMock(return_value=0)
                mock_exec.return_value = mock_proc
                await mgr.enable_node("vm1")
                await asyncio.sleep(0)  # let task schedule

        run(_run())
        assert len(mgr._tasks) > initial_tasks

    def test_conf_file_written(self, tmp_path):
        mgr = self._mgr(tmp_path)

        async def _run():
            with patch("asyncio.create_subprocess_exec", new_callable=AsyncMock) as mock_exec:
                mock_proc = MagicMock()
                mock_proc.pid = 1
                mock_proc.returncode = None
                mock_proc.wait = AsyncMock(return_value=0)
                mock_exec.return_value = mock_proc
                await mgr.enable_node("vm1", v4l2_device="/dev/video0")

        run(_run())
        conf_file = tmp_path / "sunshine" / "vm1._ozma._udp_local_" / "sunshine.conf"
        # config_dir name depends on _safe_id("vm1")
        found = list((tmp_path / "sunshine").rglob("sunshine.conf"))
        assert len(found) == 1
        content = found[0].read_text()
        assert "v4l2" in content
        assert "/dev/video0" in content


# ---------------------------------------------------------------------------
# SunshineManager — pair (mocked HTTP)
# ---------------------------------------------------------------------------

class TestPairing:
    def _mgr(self, tmp_path) -> SunshineManager:
        mgr = SunshineManager(data_dir=tmp_path / "sunshine")
        mgr.register_remote("d1", "127.0.0.1", 47990)
        return mgr

    def test_pair_success(self, tmp_path):
        mgr = self._mgr(tmp_path)

        class FakeResponse:
            status = 200
            def read(self): return json.dumps({"status": "true"}).encode()
            def __enter__(self): return self
            def __exit__(self, *a): pass

        with patch("urllib.request.urlopen", return_value=FakeResponse()):
            result = run(mgr.pair("d1", "1234"))
        assert result["ok"] is True

    def test_pair_network_error(self, tmp_path):
        mgr = self._mgr(tmp_path)
        with patch("urllib.request.urlopen", side_effect=Exception("connection refused")):
            result = run(mgr.pair("d1", "1234"))
        assert result["ok"] is False
        assert "connection refused" in result["error"]

    def test_pair_unknown_node(self, tmp_path):
        mgr = self._mgr(tmp_path)
        result = run(mgr.pair("ghost", "1234"))
        assert result["ok"] is False

    def test_pair_stopped_node(self, tmp_path):
        mgr = self._mgr(tmp_path)
        mgr._instances["d1"].state = "stopped"
        result = run(mgr.pair("d1", "1234"))
        assert result["ok"] is False


# ---------------------------------------------------------------------------
# SunshineManager — list_clients (mocked HTTP)
# ---------------------------------------------------------------------------

class TestListClients:
    def _mgr(self, tmp_path) -> SunshineManager:
        mgr = SunshineManager(data_dir=tmp_path / "sunshine")
        mgr.register_remote("d1", "127.0.0.1", 47990)
        return mgr

    def test_list_clients_success(self, tmp_path):
        mgr = self._mgr(tmp_path)
        clients = [{"name": "Moonlight", "cert": "abc123", "last_seen": 0}]

        class FakeResponse:
            def read(self): return json.dumps({"named_certs": clients}).encode()
            def __enter__(self): return self
            def __exit__(self, *a): pass

        with patch("urllib.request.urlopen", return_value=FakeResponse()):
            result = run(mgr.list_clients("d1"))
        assert len(result) == 1
        assert result[0]["name"] == "Moonlight"

    def test_list_clients_fallback_on_error(self, tmp_path):
        mgr = self._mgr(tmp_path)
        mgr._instances["d1"].paired_clients = [{"name": "cached"}]
        with patch("urllib.request.urlopen", side_effect=Exception("network error")):
            result = run(mgr.list_clients("d1"))
        assert result[0]["name"] == "cached"

    def test_list_clients_unknown_node(self, tmp_path):
        mgr = self._mgr(tmp_path)
        result = run(mgr.list_clients("nobody"))
        assert result == []


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

class TestHelpers:
    def test_safe_id_strips_dots_slashes(self):
        node_id = "vm1._ozma._udp.local."
        s = _safe_id(node_id)
        assert "/" not in s
        assert " " not in s

    def test_safe_id_max_length(self):
        assert len(_safe_id("x" * 200)) <= 64

    def test_safe_id_preserves_alnum(self):
        assert _safe_id("hello-world") == "hello-world"

    def test_all_status_empty(self, tmp_path):
        mgr = SunshineManager(data_dir=tmp_path / "sunshine")
        assert mgr.get_all_status() == []

    def test_all_status_includes_registered(self, tmp_path):
        mgr = SunshineManager(data_dir=tmp_path / "sunshine")
        mgr.register_remote("d1", "1.2.3.4", 47990)
        mgr.register_remote("d2", "5.6.7.8", 47990)
        assert len(mgr.get_all_status()) == 2
