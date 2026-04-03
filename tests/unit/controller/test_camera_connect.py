# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Unit tests for controller/camera_connect.py — V1.7 camera Connect registration.
"""

from __future__ import annotations

import asyncio
import json
import sys
import time
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

sys.path.insert(0, str(Path(__file__).parents[3] / "controller"))

from camera_connect import CameraConnectManager, CameraRegistration


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def make_node(
    node_id: str = "cam01._ozma._udp.local.",
    machine_class: str = "camera",
    host: str = "192.168.100.5",
    last_seen_offset: float = 0.0,
):
    node = MagicMock()
    node.id = node_id
    node.host = host
    node.machine_class = machine_class
    node.hw = "hikvision"
    node.camera_streams = [{"name": "main", "rtsp_inbound": f"rtsp://{host}/"}]
    node.capabilities = ["rtsp", "onvif"]
    node.frigate_host = None
    node.frigate_port = None
    node.last_seen = time.monotonic() - last_seen_offset
    return node


def make_state(nodes: dict | None = None):
    state = MagicMock()
    state.nodes = nodes or {}
    return state


def make_connect(authenticated: bool = True, api_base: str = "https://connect.test"):
    conn = MagicMock()
    conn.authenticated = authenticated
    conn._api_base = api_base
    conn._token = "tok-test"
    conn._api_post = AsyncMock(return_value={
        "camera_id": "cam-abc123",
        "relay": {
            "endpoint": "relay.connect.test:51820",
            "pubkey": "PUBKEY=",
            "allowed_ips": "10.100.0.0/24",
        },
    })
    return conn


def make_manager(tmp_path, state=None, connect=None, nodes=None):
    st = state or make_state(nodes=nodes or {})
    mgr = CameraConnectManager(
        state=st,
        connect=connect,
        data_dir=tmp_path / "cc_data",
    )
    return mgr, st


# ---------------------------------------------------------------------------
# CameraRegistration dataclass
# ---------------------------------------------------------------------------

class TestCameraRegistration:
    def test_to_dict_roundtrip(self):
        reg = CameraRegistration(
            node_id="cam01._ozma._udp.local.",
            camera_id="cam-abc123",
            relay_endpoint="relay.test:51820",
            relay_pubkey="PUB=",
            relay_allowed_ips="10.100.0.0/24",
            registered_at=1700000000.0,
            last_heartbeat=1700000060.0,
            online=True,
        )
        d = reg.to_dict()
        reg2 = CameraRegistration.from_dict(d)
        assert reg2.node_id == reg.node_id
        assert reg2.camera_id == reg.camera_id
        assert reg2.relay_endpoint == reg.relay_endpoint
        assert reg2.online is True

    def test_from_dict_defaults(self):
        reg = CameraRegistration.from_dict({"node_id": "cam01"})
        assert reg.camera_id == ""
        assert reg.relay_endpoint == ""
        assert reg.online is True
        assert reg.error == ""


# ---------------------------------------------------------------------------
# Persistence
# ---------------------------------------------------------------------------

class TestPersistence:
    def test_save_and_load(self, tmp_path):
        mgr, _ = make_manager(tmp_path)
        mgr._registrations["cam01"] = CameraRegistration(
            node_id="cam01", camera_id="cam-abc123",
            relay_endpoint="relay.test:51820",
        )
        mgr._save()

        mgr2, _ = make_manager(tmp_path)
        assert "cam01" in mgr2._registrations
        assert mgr2._registrations["cam01"].camera_id == "cam-abc123"

    def test_save_file_permissions(self, tmp_path):
        mgr, _ = make_manager(tmp_path)
        mgr._registrations["cam01"] = CameraRegistration(node_id="cam01")
        mgr._save()
        p = tmp_path / "cc_data" / "camera_registrations.json"
        assert p.exists()
        assert oct(p.stat().st_mode)[-3:] == "600"

    def test_load_missing_no_error(self, tmp_path):
        mgr, _ = make_manager(tmp_path)
        assert mgr._registrations == {}

    def test_load_corrupt_no_error(self, tmp_path):
        data_dir = tmp_path / "cc_data"
        data_dir.mkdir()
        (data_dir / "camera_registrations.json").write_text("{bad json{{")
        mgr, _ = make_manager(tmp_path)
        assert mgr._registrations == {}


# ---------------------------------------------------------------------------
# Registration
# ---------------------------------------------------------------------------

class TestRegistration:
    @pytest.mark.asyncio
    async def test_register_camera_success(self, tmp_path):
        conn = make_connect()
        node = make_node()
        mgr, state = make_manager(tmp_path, connect=conn, nodes={node.id: node})

        result = await mgr._register_camera(node)

        assert result["ok"] is True
        assert result["camera_id"] == "cam-abc123"
        assert "relay_endpoint" in result
        reg = mgr._registrations[node.id]
        assert reg.camera_id == "cam-abc123"
        assert reg.relay_endpoint == "relay.connect.test:51820"

    @pytest.mark.asyncio
    async def test_register_camera_api_failure(self, tmp_path):
        conn = make_connect()
        conn._api_post = AsyncMock(return_value=None)
        node = make_node()
        mgr, state = make_manager(tmp_path, connect=conn, nodes={node.id: node})

        result = await mgr._register_camera(node)

        assert result["ok"] is False
        assert "error" in result
        reg = mgr._registrations[node.id]
        assert reg.error != ""

    @pytest.mark.asyncio
    async def test_register_camera_no_relay_field(self, tmp_path):
        conn = make_connect()
        conn._api_post = AsyncMock(return_value={"camera_id": "cam-xyz"})
        node = make_node()
        mgr, _ = make_manager(tmp_path, connect=conn, nodes={node.id: node})

        result = await mgr._register_camera(node)

        assert result["ok"] is True
        assert result["camera_id"] == "cam-xyz"
        reg = mgr._registrations[node.id]
        assert reg.relay_endpoint == ""

    @pytest.mark.asyncio
    async def test_force_register_not_camera(self, tmp_path):
        node = make_node(machine_class="compute")
        mgr, state = make_manager(tmp_path, nodes={node.id: node})

        result = await mgr.force_register(node.id)
        assert result["ok"] is False

    @pytest.mark.asyncio
    async def test_force_register_unknown_node(self, tmp_path):
        mgr, _ = make_manager(tmp_path)
        result = await mgr.force_register("nonexistent")
        assert result["ok"] is False

    @pytest.mark.asyncio
    async def test_force_register_calls_save(self, tmp_path):
        conn = make_connect()
        node = make_node()
        mgr, _ = make_manager(tmp_path, connect=conn, nodes={node.id: node})

        with patch.object(mgr, "_save") as mock_save:
            await mgr.force_register(node.id)
            mock_save.assert_called_once()

    @pytest.mark.asyncio
    async def test_check_and_register_all_skips_non_cameras(self, tmp_path):
        conn = make_connect()
        node = make_node(machine_class="compute")
        mgr, _ = make_manager(tmp_path, connect=conn, nodes={node.id: node})

        await mgr._check_and_register_all()
        assert node.id not in mgr._registrations

    @pytest.mark.asyncio
    async def test_check_and_register_all_skips_already_registered(self, tmp_path):
        conn = make_connect()
        node = make_node()
        mgr, _ = make_manager(tmp_path, connect=conn, nodes={node.id: node})
        mgr._registrations[node.id] = CameraRegistration(
            node_id=node.id, camera_id="already-registered"
        )

        await mgr._check_and_register_all()
        # _api_post should not be called since it's already registered
        conn._api_post.assert_not_called()

    @pytest.mark.asyncio
    async def test_check_and_register_all_skips_when_not_authenticated(self, tmp_path):
        conn = make_connect(authenticated=False)
        node = make_node()
        mgr, _ = make_manager(tmp_path, connect=conn, nodes={node.id: node})

        await mgr._check_and_register_all()
        assert node.id not in mgr._registrations


# ---------------------------------------------------------------------------
# Deregistration
# ---------------------------------------------------------------------------

class TestDeregistration:
    @pytest.mark.asyncio
    async def test_deregister_removes_record(self, tmp_path):
        conn = make_connect()
        mgr, _ = make_manager(tmp_path, connect=conn)
        mgr._registrations["cam01"] = CameraRegistration(
            node_id="cam01", camera_id="cam-abc123"
        )
        with patch.object(mgr, "_connect_delete", AsyncMock()):
            result = await mgr.deregister("cam01")

        assert result["ok"] is True
        assert "cam01" not in mgr._registrations

    @pytest.mark.asyncio
    async def test_deregister_unknown_node(self, tmp_path):
        mgr, _ = make_manager(tmp_path)
        result = await mgr.deregister("nonexistent")
        assert result["ok"] is False

    @pytest.mark.asyncio
    async def test_deregister_calls_connect_api(self, tmp_path):
        conn = make_connect()
        mgr, _ = make_manager(tmp_path, connect=conn)
        mgr._registrations["cam01"] = CameraRegistration(
            node_id="cam01", camera_id="cam-abc123"
        )
        delete_mock = AsyncMock()
        with patch.object(mgr, "_connect_delete", delete_mock):
            await mgr.deregister("cam01")
        delete_mock.assert_called_once_with("/cameras/cam-abc123")

    @pytest.mark.asyncio
    async def test_deregister_no_camera_id_skips_api(self, tmp_path):
        conn = make_connect()
        mgr, _ = make_manager(tmp_path, connect=conn)
        mgr._registrations["cam01"] = CameraRegistration(node_id="cam01", camera_id="")
        delete_mock = AsyncMock()
        with patch.object(mgr, "_connect_delete", delete_mock):
            await mgr.deregister("cam01")
        delete_mock.assert_not_called()


# ---------------------------------------------------------------------------
# Heartbeat
# ---------------------------------------------------------------------------

class TestHeartbeat:
    @pytest.mark.asyncio
    async def test_heartbeat_marks_online_nodes(self, tmp_path):
        conn = make_connect()
        conn._api_post = AsyncMock(return_value={"ok": True})
        node = make_node(last_seen_offset=5.0)  # 5s ago — online
        state = make_state(nodes={node.id: node})
        mgr, _ = make_manager(tmp_path, connect=conn)
        mgr._state = state
        mgr._registrations[node.id] = CameraRegistration(
            node_id=node.id, camera_id="cam-abc123"
        )

        await mgr._send_heartbeats()

        conn._api_post.assert_called_once()
        call_args = conn._api_post.call_args[0]
        assert call_args[0] == "/cameras/heartbeat"
        payload = call_args[1]
        assert "cam-abc123" in payload["online"]
        assert payload["offline"] == []

    @pytest.mark.asyncio
    async def test_heartbeat_marks_offline_nodes(self, tmp_path):
        conn = make_connect()
        conn._api_post = AsyncMock(return_value={"ok": True})
        node = make_node(last_seen_offset=200.0)  # 200s ago — offline
        state = make_state(nodes={node.id: node})
        mgr, _ = make_manager(tmp_path, connect=conn)
        mgr._state = state
        mgr._registrations[node.id] = CameraRegistration(
            node_id=node.id, camera_id="cam-abc123"
        )

        await mgr._send_heartbeats()

        payload = conn._api_post.call_args[0][1]
        assert "cam-abc123" in payload["offline"]
        assert payload["online"] == []

    @pytest.mark.asyncio
    async def test_heartbeat_skips_when_no_registrations(self, tmp_path):
        conn = make_connect()
        mgr, _ = make_manager(tmp_path, connect=conn)
        await mgr._send_heartbeats()
        conn._api_post.assert_not_called()

    @pytest.mark.asyncio
    async def test_heartbeat_skips_when_not_authenticated(self, tmp_path):
        conn = make_connect(authenticated=False)
        mgr, _ = make_manager(tmp_path, connect=conn)
        mgr._registrations["cam01"] = CameraRegistration(
            node_id="cam01", camera_id="cam-abc123"
        )
        await mgr._send_heartbeats()
        conn._api_post.assert_not_called()

    @pytest.mark.asyncio
    async def test_heartbeat_updates_last_heartbeat(self, tmp_path):
        conn = make_connect()
        conn._api_post = AsyncMock(return_value={"ok": True})
        node = make_node(last_seen_offset=0.0)
        state = make_state(nodes={node.id: node})
        mgr, _ = make_manager(tmp_path, connect=conn)
        mgr._state = state
        mgr._registrations[node.id] = CameraRegistration(
            node_id=node.id, camera_id="cam-abc123", last_heartbeat=0.0
        )
        before = time.time()
        await mgr._send_heartbeats()
        assert mgr._registrations[node.id].last_heartbeat >= before


# ---------------------------------------------------------------------------
# Public API
# ---------------------------------------------------------------------------

class TestPublicAPI:
    def test_list_registrations_returns_dicts(self, tmp_path):
        mgr, _ = make_manager(tmp_path)
        mgr._registrations["cam01"] = CameraRegistration(
            node_id="cam01", camera_id="cam-abc123"
        )
        result = mgr.list_registrations()
        assert isinstance(result, list)
        assert result[0]["node_id"] == "cam01"
        assert result[0]["camera_id"] == "cam-abc123"

    def test_get_registration_found(self, tmp_path):
        mgr, _ = make_manager(tmp_path)
        mgr._registrations["cam01"] = CameraRegistration(node_id="cam01")
        reg = mgr.get_registration("cam01")
        assert reg is not None

    def test_get_registration_not_found(self, tmp_path):
        mgr, _ = make_manager(tmp_path)
        assert mgr.get_registration("nonexistent") is None

    def test_relay_rtsp_url_with_relay(self, tmp_path):
        mgr, _ = make_manager(tmp_path)
        mgr._registrations["cam01"] = CameraRegistration(
            node_id="cam01",
            relay_endpoint="relay.test:51820",
        )
        url = mgr.relay_rtsp_url("cam01", "/live/ch00_0")
        assert url is not None
        assert "relay.test" in url
        assert "/live/ch00_0" in url

    def test_relay_rtsp_url_no_relay(self, tmp_path):
        mgr, _ = make_manager(tmp_path)
        mgr._registrations["cam01"] = CameraRegistration(
            node_id="cam01", relay_endpoint=""
        )
        assert mgr.relay_rtsp_url("cam01") is None

    def test_relay_rtsp_url_unknown_node(self, tmp_path):
        mgr, _ = make_manager(tmp_path)
        assert mgr.relay_rtsp_url("nonexistent") is None


# ---------------------------------------------------------------------------
# Lifecycle
# ---------------------------------------------------------------------------

class TestLifecycle:
    @pytest.mark.asyncio
    async def test_start_creates_tasks(self, tmp_path):
        mgr, _ = make_manager(tmp_path)
        with patch.object(mgr, "_registration_loop", AsyncMock()), \
             patch.object(mgr, "_heartbeat_loop", AsyncMock()):
            await mgr.start()
            assert len(mgr._tasks) == 2
            await mgr.stop()

    @pytest.mark.asyncio
    async def test_stop_cancels_tasks(self, tmp_path):
        mgr, _ = make_manager(tmp_path)
        cancelled = []

        async def slow():
            try:
                await asyncio.sleep(9999)
            except asyncio.CancelledError:
                cancelled.append(True)
                raise

        task = asyncio.create_task(slow())
        mgr._tasks.append(task)
        await asyncio.sleep(0)
        await mgr.stop()
        assert cancelled

    @pytest.mark.asyncio
    async def test_start_creates_data_dir(self, tmp_path):
        data_dir = tmp_path / "nested" / "cc_data"
        node = make_node()
        state = make_state(nodes={node.id: node})
        mgr = CameraConnectManager(state=state, connect=None, data_dir=data_dir)
        with patch.object(mgr, "_registration_loop", AsyncMock()), \
             patch.object(mgr, "_heartbeat_loop", AsyncMock()):
            await mgr.start()
            await mgr.stop()
        assert data_dir.exists()
