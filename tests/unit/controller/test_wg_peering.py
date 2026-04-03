"""Unit tests for controller/wg_peering.py.

Covers: key generation, overlay IP allocation, WGPeer model, WGPeeringManager
add/remove peers, peer_with exchange, wg-quick config generation, status,
refresh_handshakes parsing. All subprocess and network calls are mocked.
"""

from __future__ import annotations

import asyncio
import base64
import json
import time
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from wg_peering import (
    CTRL_WG_INTERFACE, CTRL_WG_PORT,
    WGKeys, WGPeer, WGPeeringManager,
    _py_genkey, _public_from_private,
)


# ── Helpers ───────────────────────────────────────────────────────────────────

def _mgr(tmp_path: Path, ctrl_id: str = "ctrl-test") -> WGPeeringManager:
    return WGPeeringManager(
        controller_id=ctrl_id,
        api_port=7380,
        keys_path=tmp_path / "wg_keys.json",
        state_path=tmp_path / "wg_state.json",
    )


def _mock_run(return_map: dict | None = None) -> callable:
    """Patch WGPeeringManager._run to return (0, '', '') for all commands,
    or specific values from return_map keyed by joined args string."""
    async def _run(self, *args):
        key = " ".join(args)
        if return_map:
            for pattern, val in return_map.items():
                if pattern in key:
                    return val
        return (0, "", "")
    return _run


# ── Key generation ────────────────────────────────────────────────────────────

class TestKeyGeneration:
    def test_py_genkey_returns_two_base64_strings(self):
        priv, pub = _py_genkey()
        assert len(priv) > 0
        assert len(pub) > 0
        # Should be valid base64
        base64.b64decode(priv)
        base64.b64decode(pub)

    def test_py_genkey_different_each_call(self):
        k1 = _py_genkey()
        k2 = _py_genkey()
        assert k1[0] != k2[0]   # different private keys

    def test_wg_keys_roundtrip(self):
        keys = WGKeys(private_key="abc=", public_key="xyz=")
        k2 = WGKeys.from_dict(keys.to_dict())
        assert k2.private_key == "abc="
        assert k2.public_key == "xyz="


# ── WGPeer model ──────────────────────────────────────────────────────────────

class TestWGPeer:
    def test_defaults(self):
        peer = WGPeer(controller_id="ctrl-a", public_key="pk=",
                      endpoint="1.2.3.4:51820", overlay_ip="10.201.0.5")
        assert peer.online is False
        assert peer.last_handshake == 0.0

    def test_roundtrip(self):
        peer = WGPeer(
            controller_id="ctrl-b", public_key="pub=",
            endpoint="5.6.7.8:51820", overlay_ip="10.201.0.10",
            allowed_ips="10.201.0.10/32", online=True, last_handshake=9999.0,
        )
        p2 = WGPeer.from_dict(peer.to_dict())
        assert p2.controller_id == "ctrl-b"
        assert p2.online is True
        assert p2.last_handshake == 9999.0

    def test_allowed_ips_defaults_to_overlay_slash32(self):
        peer = WGPeer(controller_id="c", public_key="k=",
                      endpoint="1.1.1.1:51820", overlay_ip="10.201.0.3")
        d = peer.to_dict()
        assert d["allowed_ips"] == "10.201.0.3/32"


# ── WGPeeringManager ──────────────────────────────────────────────────────────

class TestWGPeeringManagerInit:
    @pytest.mark.asyncio
    async def test_start_generates_keypair(self, tmp_path):
        mgr = _mgr(tmp_path)
        with patch.object(WGPeeringManager, "_run", new=_mock_run()):
            with patch("wg_peering._wg_genkey", return_value=("priv=", "pub=")):
                await mgr.start()
        assert mgr.public_key == "pub="
        assert mgr._keys_path.exists()

    @pytest.mark.asyncio
    async def test_start_reuses_existing_keys(self, tmp_path):
        keys = WGKeys(private_key="existing-priv=", public_key="existing-pub=")
        (tmp_path / "wg_keys.json").write_text(json.dumps(keys.to_dict()))

        mgr = _mgr(tmp_path)
        with patch.object(WGPeeringManager, "_run", new=_mock_run()):
            await mgr.start()
        assert mgr.public_key == "existing-pub="

    @pytest.mark.asyncio
    async def test_start_allocates_overlay_ip(self, tmp_path):
        mgr = _mgr(tmp_path, ctrl_id="test-controller")
        with patch.object(WGPeeringManager, "_run", new=_mock_run()):
            with patch("wg_peering._wg_genkey", return_value=("p=", "k=")):
                await mgr.start()
        assert mgr.overlay_ip.startswith("10.201.0.")

    def test_overlay_ip_is_stable_for_same_ctrl_id(self, tmp_path):
        mgr1 = _mgr(tmp_path / "a", ctrl_id="stable-id")
        mgr2 = _mgr(tmp_path / "b", ctrl_id="stable-id")
        ip1 = mgr1._allocate_overlay_ip("stable-id")
        ip2 = mgr2._allocate_overlay_ip("stable-id")
        assert ip1 == ip2

    def test_overlay_ip_differs_for_different_ctrl_ids(self, tmp_path):
        mgr = _mgr(tmp_path)
        ip1 = mgr._allocate_overlay_ip("controller-alpha")
        ip2 = mgr._allocate_overlay_ip("controller-beta")
        assert ip1 != ip2

    def test_overlay_ip_octet_in_range(self, tmp_path):
        mgr = _mgr(tmp_path)
        for name in ["a", "b", "c", "really-long-name-that-wraps", "x" * 50]:
            ip = mgr._allocate_overlay_ip(name)
            octet = int(ip.split(".")[-1])
            assert 1 <= octet <= 253


# ── add_peer / remove_peer ────────────────────────────────────────────────────

class TestPeerManagement:
    @pytest.mark.asyncio
    async def test_add_peer_stores_peer(self, tmp_path):
        mgr = _mgr(tmp_path)
        mgr._keys = WGKeys(private_key="p=", public_key="k=")
        with patch.object(WGPeeringManager, "_run", new=_mock_run()):
            peer = await mgr.add_peer(
                controller_id="ctrl-remote",
                public_key="remote-pub=",
                endpoint="10.0.0.2:51820",
                overlay_ip="10.201.0.7",
            )
        assert mgr.get_peer("ctrl-remote") is not None
        assert peer.overlay_ip == "10.201.0.7"

    @pytest.mark.asyncio
    async def test_add_peer_persists(self, tmp_path):
        mgr = _mgr(tmp_path)
        mgr._keys = WGKeys(private_key="p=", public_key="k=")
        with patch.object(WGPeeringManager, "_run", new=_mock_run()):
            await mgr.add_peer("ctrl-r2", "pub=", "10.0.0.3:51820", "10.201.0.8")

        mgr2 = _mgr(tmp_path)
        assert mgr2.get_peer("ctrl-r2") is not None

    @pytest.mark.asyncio
    async def test_add_peer_derives_overlay_ip_when_empty(self, tmp_path):
        mgr = _mgr(tmp_path)
        mgr._keys = WGKeys(private_key="p=", public_key="k=")
        with patch.object(WGPeeringManager, "_run", new=_mock_run()):
            peer = await mgr.add_peer("ctrl-r3", "pub=", "10.0.0.4:51820", overlay_ip="")
        assert peer.overlay_ip.startswith("10.201.0.")

    @pytest.mark.asyncio
    async def test_remove_peer(self, tmp_path):
        mgr = _mgr(tmp_path)
        mgr._keys = WGKeys(private_key="p=", public_key="k=")
        with patch.object(WGPeeringManager, "_run", new=_mock_run()):
            await mgr.add_peer("ctrl-del", "pub=", "10.0.0.5:51820", "10.201.0.9")
            ok = await mgr.remove_peer("ctrl-del")
        assert ok is True
        assert mgr.get_peer("ctrl-del") is None

    @pytest.mark.asyncio
    async def test_remove_unknown_peer_returns_false(self, tmp_path):
        mgr = _mgr(tmp_path)
        mgr._keys = WGKeys(private_key="p=", public_key="k=")
        with patch.object(WGPeeringManager, "_run", new=_mock_run()):
            ok = await mgr.remove_peer("ghost-id")
        assert ok is False

    def test_list_peers(self, tmp_path):
        mgr = _mgr(tmp_path)
        mgr._peers = {
            "a": WGPeer(controller_id="a", public_key="pa=",
                        endpoint="1.1.1.1:51820", overlay_ip="10.201.0.1"),
            "b": WGPeer(controller_id="b", public_key="pb=",
                        endpoint="2.2.2.2:51820", overlay_ip="10.201.0.2"),
        }
        assert len(mgr.list_peers()) == 2


# ── peer_with (exchange flow) ─────────────────────────────────────────────────

class TestPeerWith:
    def _make_client(self, get_response=None, post_response=None,
                      get_side_effect=None):
        """Build a mock httpx.AsyncClient context manager."""
        mock_client = MagicMock()
        mock_client.__aenter__ = AsyncMock(return_value=mock_client)
        mock_client.__aexit__ = AsyncMock(return_value=False)
        if get_side_effect:
            mock_client.get = AsyncMock(side_effect=get_side_effect)
        else:
            mock_client.get = AsyncMock(return_value=get_response)
        mock_client.post = AsyncMock(return_value=post_response or MagicMock(status_code=200))
        return mock_client

    @pytest.mark.asyncio
    async def test_peer_with_success(self, tmp_path):
        mgr = _mgr(tmp_path, ctrl_id="local-ctrl")
        mgr._keys = WGKeys(private_key="local-priv=", public_key="local-pub=")
        mgr._overlay_ip = "10.201.0.1"

        peer_info_response = {
            "controller_id": "remote-ctrl",
            "public_key": "remote-pub=",
            "endpoint": "5.5.5.5:51820",
            "overlay_ip": "10.201.0.50",
            "api_port": 7380,
        }

        mock_get = MagicMock(status_code=200)
        mock_get.json = MagicMock(return_value=peer_info_response)
        mock_post = MagicMock(status_code=200)
        mock_post.json = MagicMock(return_value={"status": "peered"})
        mock_client = self._make_client(mock_get, mock_post)

        import httpx as _httpx
        with patch.object(WGPeeringManager, "_run", new=_mock_run()):
            with patch.object(_httpx, "AsyncClient", return_value=mock_client):
                peer = await mgr.peer_with("5.5.5.5", 7380)

        assert peer is not None
        assert peer.controller_id == "remote-ctrl"
        assert peer.public_key == "remote-pub="

    @pytest.mark.asyncio
    async def test_peer_with_returns_none_on_network_error(self, tmp_path):
        mgr = _mgr(tmp_path)
        mgr._keys = WGKeys(private_key="p=", public_key="k=")
        mgr._overlay_ip = "10.201.0.1"

        mock_client = self._make_client(get_side_effect=Exception("connection refused"))
        import httpx as _httpx
        with patch.object(_httpx, "AsyncClient", return_value=mock_client):
            peer = await mgr.peer_with("bad-host", 7380)

        assert peer is None

    @pytest.mark.asyncio
    async def test_peer_with_returns_none_on_non_200(self, tmp_path):
        mgr = _mgr(tmp_path)
        mgr._keys = WGKeys(private_key="p=", public_key="k=")
        mgr._overlay_ip = "10.201.0.1"

        mock_get = MagicMock(status_code=503)
        mock_client = self._make_client(mock_get)
        import httpx as _httpx
        with patch.object(_httpx, "AsyncClient", return_value=mock_client):
            peer = await mgr.peer_with("host", 7380)

        assert peer is None


# ── wg-quick config ───────────────────────────────────────────────────────────

class TestWGConfig:
    def test_write_wg_config_contains_interface_section(self, tmp_path):
        mgr = _mgr(tmp_path, ctrl_id="local")
        mgr._keys = WGKeys(private_key="mypriv=", public_key="mypub=")
        mgr._overlay_ip = "10.201.0.5"
        conf = mgr.write_wg_config()
        assert "[Interface]" in conf
        assert "mypriv=" in conf
        assert "10.201.0.5" in conf
        assert str(CTRL_WG_PORT) in conf

    def test_write_wg_config_contains_peer_sections(self, tmp_path):
        mgr = _mgr(tmp_path)
        mgr._keys = WGKeys(private_key="p=", public_key="k=")
        mgr._overlay_ip = "10.201.0.1"
        mgr._peers["peer-a"] = WGPeer(
            controller_id="peer-a", public_key="peer-pub=",
            endpoint="9.9.9.9:51820", overlay_ip="10.201.0.20",
        )
        conf = mgr.write_wg_config()
        assert "[Peer]" in conf
        assert "peer-pub=" in conf
        assert "9.9.9.9:51820" in conf
        assert "10.201.0.20/32" in conf

    def test_write_wg_config_no_peers(self, tmp_path):
        mgr = _mgr(tmp_path)
        mgr._keys = WGKeys(private_key="p=", public_key="k=")
        mgr._overlay_ip = "10.201.0.1"
        conf = mgr.write_wg_config()
        assert "[Peer]" not in conf


# ── refresh_handshakes ────────────────────────────────────────────────────────

class TestRefreshHandshakes:
    @pytest.mark.asyncio
    async def test_refresh_marks_recent_peer_online(self, tmp_path):
        mgr = _mgr(tmp_path)
        mgr._keys = WGKeys(private_key="p=", public_key="k=")
        mgr._peers["ctrl-x"] = WGPeer(
            controller_id="ctrl-x", public_key="xpub=",
            endpoint="1.1.1.1:51820", overlay_ip="10.201.0.5",
        )
        recent = int(time.time()) - 30  # 30 seconds ago

        wg_dump = (
            f"{CTRL_WG_INTERFACE}\t\t\t\t\t\t\t\n"   # interface line (skip)
            f"xpub=\t(none)\t1.1.1.1:51820\t10.201.0.5/32\t{recent}\t1024\t512\t25\n"
        )

        async def mock_run(self, *args):
            if "dump" in args:
                return (0, wg_dump, "")
            return (0, "", "")

        with patch.object(WGPeeringManager, "_run", new=mock_run):
            await mgr._refresh_handshakes()

        assert mgr._peers["ctrl-x"].last_handshake == float(recent)
        assert mgr._peers["ctrl-x"].online is True

    @pytest.mark.asyncio
    async def test_refresh_marks_stale_peer_offline(self, tmp_path):
        mgr = _mgr(tmp_path)
        mgr._keys = WGKeys(private_key="p=", public_key="k=")
        mgr._peers["ctrl-y"] = WGPeer(
            controller_id="ctrl-y", public_key="ypub=",
            endpoint="2.2.2.2:51820", overlay_ip="10.201.0.6",
            online=True,
        )
        stale = int(time.time()) - 300   # 5 minutes ago — > 3 min threshold

        wg_dump = (
            f"iface\t\t\t\t\t\t\t\n"
            f"ypub=\t(none)\t2.2.2.2:51820\t10.201.0.6/32\t{stale}\t0\t0\t0\n"
        )

        async def mock_run(self, *args):
            if "dump" in args:
                return (0, wg_dump, "")
            return (0, "", "")

        with patch.object(WGPeeringManager, "_run", new=mock_run):
            await mgr._refresh_handshakes()

        assert mgr._peers["ctrl-y"].online is False

    @pytest.mark.asyncio
    async def test_refresh_noop_on_wg_failure(self, tmp_path):
        mgr = _mgr(tmp_path)
        mgr._keys = WGKeys(private_key="p=", public_key="k=")

        async def mock_run(self, *args):
            return (1, "", "wg: not running")

        with patch.object(WGPeeringManager, "_run", new=mock_run):
            await mgr._refresh_handshakes()  # should not raise


# ── Status ────────────────────────────────────────────────────────────────────

class TestStatus:
    def test_status_shape(self, tmp_path):
        mgr = _mgr(tmp_path, ctrl_id="ctrl-test")
        mgr._keys = WGKeys(private_key="p=", public_key="k=")
        mgr._overlay_ip = "10.201.0.3"
        mgr._peers["p1"] = WGPeer(controller_id="p1", public_key="pub=",
                                   endpoint="1.1.1.1:51820", overlay_ip="10.201.0.4",
                                   online=True)
        s = mgr.status()
        assert s["controller_id"] == "ctrl-test"
        assert s["overlay_ip"] == "10.201.0.3"
        assert s["public_key"] == "k="
        assert s["peer_count"] == 1
        assert s["peers_online"] == 1

    def test_get_info_returns_correct_fields(self, tmp_path):
        mgr = _mgr(tmp_path, ctrl_id="ctrl-info")
        mgr._keys = WGKeys(private_key="p=", public_key="infopub=")
        mgr._overlay_ip = "10.201.0.11"
        info = mgr.get_info()
        assert info["controller_id"] == "ctrl-info"
        assert info["public_key"] == "infopub="
        assert "endpoint" in info
        assert info["api_port"] == 7380
