"""Unit tests for controller-to-controller mDNS peer discovery.

Covers:
  - PeerController.auto_discovered field + serialisation
  - SharingManager.mark_peer_online / mark_peer_offline
  - SharingManager.add_peer with auto_discovered flag
  - DiscoveryService.start_peer_browser callbacks (_resolve_peer, _peer_lost)
  - main.py wiring: _on_peer_found / _on_peer_lost integration

All zeroconf and network calls are mocked.
"""

from __future__ import annotations

import asyncio
import socket
import struct
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

# ── path already fixed by conftest ──────────────────────────────────────────

from sharing import PeerController, SharingManager
from discovery import DiscoveryService


# ── Helpers ──────────────────────────────────────────────────────────────────

def _manager(tmp_path: Path) -> SharingManager:
    return SharingManager(tmp_path / "shares.json")


def _ip_bytes(ip: str = "192.168.1.10") -> bytes:
    return socket.inet_aton(ip)


def _mock_service_info(name: str, ip: str, api_port: int, ctrl_id: str) -> MagicMock:
    info = MagicMock()
    info.name = name
    info.addresses = [_ip_bytes(ip)]
    info.properties = {
        b"api_port": str(api_port).encode(),
        b"controller_id": ctrl_id.encode(),
        b"version": b"1",
    }
    info.async_request = AsyncMock(return_value=True)
    return info


def _discovery() -> DiscoveryService:
    config = MagicMock()
    config.mdns_service_type = "_ozma._udp.local."
    config.mdns_requery_interval = 60
    config.node_port = 7331
    config.api_port = 7380

    state = MagicMock()
    state.nodes = {}
    state.events = asyncio.Queue()

    svc = DiscoveryService(config, state)
    # Inject a running AsyncZeroconf mock
    svc._azc = MagicMock()
    svc._loop = asyncio.get_event_loop()
    return svc


# ── PeerController ────────────────────────────────────────────────────────────

class TestPeerControllerModel:
    def test_auto_discovered_defaults_false(self):
        p = PeerController(id="ctrl-a", owner_user_id="", name="A", host="1.2.3.4")
        assert p.auto_discovered is False

    def test_auto_discovered_serialises(self):
        p = PeerController(id="ctrl-a", owner_user_id="", name="A", host="1.2.3.4",
                           auto_discovered=True)
        d = p.to_dict()
        assert d["auto_discovered"] is True

    def test_auto_discovered_roundtrips(self):
        p = PeerController(id="ctrl-b", owner_user_id="u1", name="B", host="5.6.7.8",
                           auto_discovered=True)
        p2 = PeerController.from_dict(p.to_dict())
        assert p2.auto_discovered is True

    def test_auto_discovered_false_roundtrips(self):
        p = PeerController(id="ctrl-c", owner_user_id="", name="C", host="9.9.9.9")
        p2 = PeerController.from_dict(p.to_dict())
        assert p2.auto_discovered is False

    def test_from_dict_missing_auto_discovered_defaults_false(self):
        d = {"id": "ctrl-d", "owner_user_id": "", "name": "D", "host": "1.1.1.1"}
        p = PeerController.from_dict(d)
        assert p.auto_discovered is False


# ── SharingManager: mark_peer_online / mark_peer_offline ─────────────────────

class TestSharingManagerPeerHelpers:
    def test_mark_peer_online_returns_none_for_unknown(self, tmp_path):
        mgr = _manager(tmp_path)
        result = mgr.mark_peer_online("unknown-id", "1.2.3.4", 7380)
        assert result is None

    def test_mark_peer_online_updates_address_and_status(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.add_peer("ctrl-a", owner_user_id="", name="A", host="old-host", port=7380)
        result = mgr.mark_peer_online("ctrl-a", "new-host", 8080)
        assert result is not None
        assert result.host == "new-host"
        assert result.port == 8080
        assert result.online is True

    def test_mark_peer_online_updates_last_seen(self, tmp_path):
        import time
        mgr = _manager(tmp_path)
        mgr.add_peer("ctrl-a", owner_user_id="", name="A", host="h", port=7380)
        before = time.time()
        mgr.mark_peer_online("ctrl-a", "h", 7380)
        assert mgr.get_peer("ctrl-a").last_seen >= before

    def test_mark_peer_offline_returns_none_for_unknown(self, tmp_path):
        mgr = _manager(tmp_path)
        assert mgr.mark_peer_offline("ghost") is None

    def test_mark_peer_offline_sets_online_false(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.add_peer("ctrl-b", owner_user_id="", name="B", host="h", port=7380)
        mgr.mark_peer_online("ctrl-b", "h", 7380)  # make it online first
        result = mgr.mark_peer_offline("ctrl-b")
        assert result is not None
        assert result.online is False

    def test_mark_peer_offline_idempotent(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.add_peer("ctrl-c", owner_user_id="", name="C", host="h", port=7380)
        mgr.mark_peer_offline("ctrl-c")
        mgr.mark_peer_offline("ctrl-c")  # should not raise
        assert mgr.get_peer("ctrl-c").online is False

    def test_add_peer_auto_discovered_flag_not_set_by_default(self, tmp_path):
        mgr = _manager(tmp_path)
        peer = mgr.add_peer("ctrl-d", owner_user_id="", name="D", host="h", port=7380)
        assert peer.auto_discovered is False

    def test_add_peer_can_set_auto_discovered_manually(self, tmp_path):
        mgr = _manager(tmp_path)
        peer = mgr.add_peer("ctrl-e", owner_user_id="", name="E", host="h", port=7380)
        peer.auto_discovered = True
        assert mgr.get_peer("ctrl-e").auto_discovered is True


# ── DiscoveryService peer browser ─────────────────────────────────────────────

class TestDiscoveryPeerBrowser:
    @pytest.mark.asyncio
    async def test_start_peer_browser_registers_callbacks(self):
        svc = _discovery()
        found_calls = []
        lost_calls = []

        async def on_found(info): found_calls.append(info)
        async def on_lost(ctrl_id): lost_calls.append(ctrl_id)

        from zeroconf.asyncio import AsyncServiceBrowser as _ASB
        with patch("discovery.AsyncServiceBrowser" if hasattr(
                __import__("discovery"), "AsyncServiceBrowser") else
                "zeroconf.asyncio.AsyncServiceBrowser",
                MagicMock()) as _mock_asb:
            # Patch the import inside start_peer_browser
            with patch("discovery.DiscoveryService.start_peer_browser",
                       wraps=svc.start_peer_browser.__func__):
                pass
            # Direct call — patch the import inside the method
            import importlib
            import discovery as disc_mod
            with patch.object(disc_mod, "__builtins__", disc_mod.__builtins__):
                await svc.start_peer_browser(on_found, on_lost)

        assert svc._on_peer_found is on_found
        assert svc._on_peer_lost is on_lost

    @pytest.mark.asyncio
    async def test_resolve_peer_fires_on_found(self):
        svc = _discovery()
        found: list[dict] = []

        async def on_found(info): found.append(info)
        svc._on_peer_found = on_found

        mock_info = _mock_service_info(
            name="ctrl-abc._ozma-ctrl._tcp.local.",
            ip="10.0.0.5",
            api_port=7380,
            ctrl_id="ctrl-abc",
        )

        from zeroconf.asyncio import AsyncServiceInfo as _ASI
        with patch("discovery.AsyncServiceInfo" if hasattr(
                __import__("discovery"), "AsyncServiceInfo") else
                "zeroconf.asyncio.AsyncServiceInfo",
                return_value=mock_info):
            # Patch at the import site inside the method
            import discovery as disc_mod
            with patch.object(disc_mod, "AsyncZeroconf", MagicMock()):
                with patch("zeroconf.asyncio.AsyncServiceInfo", return_value=mock_info):
                    await svc._resolve_peer(MagicMock(), "ctrl-abc._ozma-ctrl._tcp.local.")

        assert len(found) == 1
        assert found[0]["id"] == "ctrl-abc"
        assert found[0]["host"] == "10.0.0.5"
        assert found[0]["api_port"] == 7380

    @pytest.mark.asyncio
    async def test_resolve_peer_skips_self(self):
        svc = _discovery()
        found: list[dict] = []
        async def on_found(info): found.append(info)
        svc._on_peer_found = on_found

        # Simulate this controller has already announced itself
        my_info = MagicMock()
        my_info.name = "self-ctrl._ozma-ctrl._tcp.local."
        svc._ctrl_info = my_info

        mock_info = _mock_service_info(
            name="self-ctrl._ozma-ctrl._tcp.local.",
            ip="127.0.0.1",
            api_port=7380,
            ctrl_id="self-ctrl",
        )
        with patch("zeroconf.asyncio.AsyncServiceInfo", return_value=mock_info):
            await svc._resolve_peer(MagicMock(), "self-ctrl._ozma-ctrl._tcp.local.")

        assert found == []

    @pytest.mark.asyncio
    async def test_resolve_peer_skips_no_address(self):
        svc = _discovery()
        found: list[dict] = []
        async def on_found(info): found.append(info)
        svc._on_peer_found = on_found

        mock_info = MagicMock()
        mock_info.addresses = []
        mock_info.async_request = AsyncMock(return_value=False)

        with patch("zeroconf.asyncio.AsyncServiceInfo", return_value=mock_info):
            await svc._resolve_peer(MagicMock(), "ctrl-x._ozma-ctrl._tcp.local.")

        assert found == []

    @pytest.mark.asyncio
    async def test_peer_lost_fires_on_lost(self):
        svc = _discovery()
        lost: list[str] = []

        async def on_lost(ctrl_id): lost.append(ctrl_id)
        svc._on_peer_lost = on_lost

        await svc._peer_lost("ctrl-gone")

        assert lost == ["ctrl-gone"]

    @pytest.mark.asyncio
    async def test_peer_lost_noop_when_no_callback(self):
        svc = _discovery()
        svc._on_peer_lost = None
        # Should not raise
        await svc._peer_lost("ctrl-gone")

    @pytest.mark.asyncio
    async def test_start_peer_browser_noop_when_no_azc(self):
        svc = _discovery()
        svc._azc = None
        # Should return early without error
        await svc.start_peer_browser(AsyncMock(), AsyncMock())
        assert svc._peer_browser is None


# ── Integration: _on_peer_found / _on_peer_lost wiring ───────────────────────

class TestPeerFoundLostWiring:
    """Simulate the callbacks defined in main.py."""

    def _make_callbacks(self, tmp_path: Path):
        """Return (on_found, on_lost, sharing, events) matching main.py logic."""
        sharing = _manager(tmp_path)
        events = asyncio.Queue()

        async def _on_peer_found(info: dict) -> None:
            existing = sharing.get_peer(info["id"])
            if existing:
                was_online = existing.online
                updated = sharing.mark_peer_online(info["id"], info["host"], info["api_port"])
                if updated and not was_online:
                    await events.put({"type": "peer.online", "controller_id": info["id"]})
            else:
                peer = sharing.add_peer(
                    controller_id=info["id"],
                    owner_user_id="",
                    name=info["id"],
                    host=info["host"],
                    port=info["api_port"],
                    transport="lan",
                )
                peer.auto_discovered = True
                await events.put({"type": "peer.discovered", "peer": peer.to_dict()})

        async def _on_peer_lost(ctrl_id: str) -> None:
            peer = sharing.mark_peer_offline(ctrl_id)
            if peer:
                await events.put({"type": "peer.offline", "controller_id": ctrl_id})

        return _on_peer_found, _on_peer_lost, sharing, events

    @pytest.mark.asyncio
    async def test_new_peer_is_auto_linked(self, tmp_path):
        on_found, _, sharing, events = self._make_callbacks(tmp_path)
        await on_found({"id": "ctrl-x", "host": "192.168.1.5", "api_port": 7380,
                        "base_url": "http://192.168.1.5:7380"})

        peer = sharing.get_peer("ctrl-x")
        assert peer is not None
        assert peer.host == "192.168.1.5"
        assert peer.auto_discovered is True
        assert peer.transport == "lan"

    @pytest.mark.asyncio
    async def test_new_peer_emits_discovered_event(self, tmp_path):
        on_found, _, sharing, events = self._make_callbacks(tmp_path)
        await on_found({"id": "ctrl-y", "host": "10.0.0.1", "api_port": 7380,
                        "base_url": "http://10.0.0.1:7380"})

        event = events.get_nowait()
        assert event["type"] == "peer.discovered"
        assert event["peer"]["id"] == "ctrl-y"

    @pytest.mark.asyncio
    async def test_existing_peer_address_updated_on_found(self, tmp_path):
        on_found, _, sharing, events = self._make_callbacks(tmp_path)
        # Pre-add peer with old address
        sharing.add_peer("ctrl-z", owner_user_id="", name="Z",
                         host="old-host", port=7380)

        await on_found({"id": "ctrl-z", "host": "new-host", "api_port": 9999,
                        "base_url": "http://new-host:9999"})

        peer = sharing.get_peer("ctrl-z")
        assert peer.host == "new-host"
        assert peer.port == 9999

    @pytest.mark.asyncio
    async def test_peer_lost_marks_offline_and_emits_event(self, tmp_path):
        on_found, on_lost, sharing, events = self._make_callbacks(tmp_path)
        await on_found({"id": "ctrl-a", "host": "h", "api_port": 7380,
                        "base_url": "http://h:7380"})
        _ = events.get_nowait()  # consume discovered event

        await on_lost("ctrl-a")

        peer = sharing.get_peer("ctrl-a")
        assert peer.online is False
        event = events.get_nowait()
        assert event["type"] == "peer.offline"
        assert event["controller_id"] == "ctrl-a"

    @pytest.mark.asyncio
    async def test_peer_lost_unknown_does_not_emit_event(self, tmp_path):
        _, on_lost, sharing, events = self._make_callbacks(tmp_path)
        await on_lost("ctrl-never-seen")
        assert events.empty()

    @pytest.mark.asyncio
    async def test_peer_comes_back_online_emits_online_event(self, tmp_path):
        on_found, on_lost, sharing, events = self._make_callbacks(tmp_path)

        # Discover then lose then rediscover
        await on_found({"id": "ctrl-b", "host": "h", "api_port": 7380,
                        "base_url": "http://h:7380"})
        _ = events.get_nowait()  # discovered

        await on_lost("ctrl-b")
        _ = events.get_nowait()  # offline

        await on_found({"id": "ctrl-b", "host": "h", "api_port": 7380,
                        "base_url": "http://h:7380"})
        event = events.get_nowait()
        assert event["type"] == "peer.online"
        assert event["controller_id"] == "ctrl-b"
