# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Unit tests for LocalProxyManager, ProxyRoute, LocalProxyConfig, and build_caddyfile."""

import json
import stat
import sys
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

sys.path.insert(0, str(Path(__file__).parents[3] / "controller"))

from local_proxy import (
    LocalProxyConfig,
    LocalProxyManager,
    ProxyRoute,
    build_caddyfile,
)


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _manager(tmp_path: Path) -> LocalProxyManager:
    return LocalProxyManager(state_path=tmp_path / "local_proxy_state.json")


def _route(**kwargs) -> ProxyRoute:
    defaults = dict(
        id="jellyfin",
        name="Jellyfin",
        match_domain="jellyfin.home",
        upstream="http://localhost:8096",
        tls_mode="internal",
        enabled=True,
        strip_prefix="",
        extra_headers={},
    )
    defaults.update(kwargs)
    return ProxyRoute(**defaults)


# ---------------------------------------------------------------------------
# TestProxyRouteModel
# ---------------------------------------------------------------------------

class TestProxyRouteModel:
    def test_to_dict_has_all_keys(self):
        r = _route()
        d = r.to_dict()
        for key in ("id", "name", "match_domain", "upstream", "tls_mode",
                    "enabled", "strip_prefix", "extra_headers"):
            assert key in d

    def test_roundtrip(self):
        r = _route(extra_headers={"X-Foo": "bar"}, strip_prefix="/app")
        restored = ProxyRoute.from_dict(r.to_dict())
        assert restored.id == r.id
        assert restored.name == r.name
        assert restored.match_domain == r.match_domain
        assert restored.upstream == r.upstream
        assert restored.tls_mode == r.tls_mode
        assert restored.enabled == r.enabled
        assert restored.strip_prefix == r.strip_prefix
        assert restored.extra_headers == r.extra_headers

    def test_defaults_from_dict(self):
        d = {"id": "x", "match_domain": "x.home", "upstream": "http://localhost:1234"}
        r = ProxyRoute.from_dict(d)
        assert r.name == "x"        # falls back to id
        assert r.tls_mode == "internal"
        assert r.enabled is True
        assert r.strip_prefix == ""
        assert r.extra_headers == {}

    def test_from_dict_preserves_enabled_false(self):
        r = _route(enabled=False)
        restored = ProxyRoute.from_dict(r.to_dict())
        assert restored.enabled is False


# ---------------------------------------------------------------------------
# TestLocalProxyConfig
# ---------------------------------------------------------------------------

class TestLocalProxyConfig:
    def test_defaults(self):
        cfg = LocalProxyConfig()
        assert cfg.enabled is False
        assert cfg.bind_address == "0.0.0.0"
        assert cfg.http_port == 80
        assert cfg.https_port == 443
        assert cfg.caddy_binary == "caddy"
        assert cfg.admin_api == "localhost:2019"

    def test_roundtrip(self):
        cfg = LocalProxyConfig(
            enabled=True,
            bind_address="192.168.1.1",
            http_port=8080,
            https_port=8443,
            caddy_binary="/usr/local/bin/caddy",
            admin_api="localhost:9999",
        )
        restored = LocalProxyConfig.from_dict(cfg.to_dict())
        assert restored.enabled is True
        assert restored.bind_address == "192.168.1.1"
        assert restored.http_port == 8080
        assert restored.https_port == 8443
        assert restored.caddy_binary == "/usr/local/bin/caddy"
        assert restored.admin_api == "localhost:9999"

    def test_from_dict_defaults_for_missing_keys(self):
        cfg = LocalProxyConfig.from_dict({})
        assert cfg.enabled is False
        assert cfg.http_port == 80


# ---------------------------------------------------------------------------
# TestBuildCaddyfile
# ---------------------------------------------------------------------------

class TestBuildCaddyfile:
    def _cfg(self) -> LocalProxyConfig:
        return LocalProxyConfig()

    def test_empty_routes_returns_global_block_only(self):
        result = build_caddyfile([], self._cfg())
        assert "admin" in result
        # No site blocks — just the global block
        assert "reverse_proxy" not in result

    def test_single_route_contains_match_domain(self):
        route = _route()
        result = build_caddyfile([route], self._cfg())
        assert "jellyfin.home" in result

    def test_single_route_contains_upstream(self):
        route = _route()
        result = build_caddyfile([route], self._cfg())
        assert "http://localhost:8096" in result

    def test_tls_internal_present(self):
        route = _route(tls_mode="internal")
        result = build_caddyfile([route], self._cfg())
        assert "tls internal" in result

    def test_tls_off_uses_http_address(self):
        route = _route(tls_mode="off")
        result = build_caddyfile([route], self._cfg())
        assert "http://jellyfin.home" in result
        assert "tls internal" not in result

    def test_multiple_routes_all_present(self):
        routes = [
            _route(id="jf", name="Jellyfin", match_domain="jellyfin.home", upstream="http://localhost:8096"),
            _route(id="vw", name="Vaultwarden", match_domain="vaultwarden.home", upstream="http://localhost:8080"),
        ]
        result = build_caddyfile(routes, self._cfg())
        assert "jellyfin.home" in result
        assert "vaultwarden.home" in result

    def test_disabled_route_excluded(self):
        routes = [
            _route(id="active", match_domain="active.home", upstream="http://localhost:8096"),
            _route(id="off", match_domain="disabled.home", upstream="http://localhost:9000", enabled=False),
        ]
        result = build_caddyfile(routes, self._cfg())
        assert "active.home" in result
        assert "disabled.home" not in result

    def test_admin_api_in_global_block(self):
        cfg = LocalProxyConfig(admin_api="localhost:5555")
        result = build_caddyfile([], cfg)
        assert "localhost:5555" in result


# ---------------------------------------------------------------------------
# TestLocalProxyManagerCRUD
# ---------------------------------------------------------------------------

class TestLocalProxyManagerCRUD:
    def test_add_route_returns_proxy_route(self, tmp_path):
        mgr = _manager(tmp_path)
        route = mgr.add_route("Jellyfin", "jellyfin.home", "http://localhost:8096")
        assert isinstance(route, ProxyRoute)
        assert route.name == "Jellyfin"
        assert route.match_domain == "jellyfin.home"
        assert route.upstream == "http://localhost:8096"

    def test_add_route_id_slugified(self, tmp_path):
        mgr = _manager(tmp_path)
        route = mgr.add_route("My Service!", "my.home", "http://localhost:1234")
        assert route.id == "my-service-"

    def test_update_route_changes_field(self, tmp_path):
        mgr = _manager(tmp_path)
        route = mgr.add_route("Test", "test.home", "http://localhost:1111")
        updated = mgr.update_route(route.id, upstream="http://localhost:9999")
        assert updated is not None
        assert updated.upstream == "http://localhost:9999"

    def test_update_route_missing_returns_none(self, tmp_path):
        mgr = _manager(tmp_path)
        result = mgr.update_route("nonexistent", upstream="http://localhost:1")
        assert result is None

    def test_remove_route_returns_true(self, tmp_path):
        mgr = _manager(tmp_path)
        route = mgr.add_route("Del", "del.home", "http://localhost:2222")
        assert mgr.remove_route(route.id) is True
        assert mgr.get_route(route.id) is None

    def test_remove_route_missing_returns_false(self, tmp_path):
        mgr = _manager(tmp_path)
        assert mgr.remove_route("ghost") is False

    def test_list_routes_returns_list_of_dicts(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.add_route("A", "a.home", "http://localhost:1")
        mgr.add_route("B", "b.home", "http://localhost:2")
        routes = mgr.list_routes()
        assert isinstance(routes, list)
        assert len(routes) == 2
        for r in routes:
            assert isinstance(r, dict)
            assert "id" in r

    def test_get_route_by_id(self, tmp_path):
        mgr = _manager(tmp_path)
        route = mgr.add_route("NAS", "nas.home", "http://192.168.1.10:5000")
        found = mgr.get_route(route.id)
        assert found is not None
        assert found.match_domain == "nas.home"

    def test_duplicate_name_gets_different_id(self, tmp_path):
        mgr = _manager(tmp_path)
        r1 = mgr.add_route("service", "s1.home", "http://localhost:1")
        r2 = mgr.add_route("service", "s2.home", "http://localhost:2")
        assert r1.id != r2.id


# ---------------------------------------------------------------------------
# TestLocalProxyManagerConfig
# ---------------------------------------------------------------------------

class TestLocalProxyManagerConfig:
    @pytest.mark.asyncio
    async def test_set_config_updates_fields(self, tmp_path):
        mgr = _manager(tmp_path)
        with patch.object(mgr, "_apply", AsyncMock()):
            with patch.object(mgr, "_teardown", AsyncMock()):
                await mgr.set_config(http_port=8080, https_port=8443)
        assert mgr.get_config().http_port == 8080
        assert mgr.get_config().https_port == 8443

    def test_get_status_fields(self, tmp_path):
        mgr = _manager(tmp_path)
        status = mgr.get_status()
        assert "enabled" in status
        assert "active" in status
        assert "routes_total" in status
        assert "routes_enabled" in status
        assert "admin_api" in status
        assert "ca_cert_path" in status

    def test_get_status_routes_total_correct(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.add_route("A", "a.home", "http://localhost:1")
        mgr.add_route("B", "b.home", "http://localhost:2")
        assert mgr.get_status()["routes_total"] == 2

    def test_get_ca_cert_returns_none_when_missing(self, tmp_path):
        mgr = _manager(tmp_path)
        # Override ca_cert_path to something that doesn't exist
        mgr._config.ca_cert_path = str(tmp_path / "nonexistent.crt")
        assert mgr.get_ca_cert() is None

    def test_get_ca_cert_returns_bytes_when_exists(self, tmp_path):
        mgr = _manager(tmp_path)
        cert_file = tmp_path / "root.crt"
        cert_file.write_bytes(b"FAKE CERT DATA")
        mgr._config.ca_cert_path = str(cert_file)
        result = mgr.get_ca_cert()
        assert result == b"FAKE CERT DATA"


# ---------------------------------------------------------------------------
# TestPersistence
# ---------------------------------------------------------------------------

class TestPersistence:
    def test_routes_survive_reload(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.add_route("Jellyfin", "jellyfin.home", "http://localhost:8096", tls_mode="off")
        mgr.add_route("Vaultwarden", "vaultwarden.home", "http://localhost:8080")

        mgr2 = _manager(tmp_path)
        routes = mgr2.list_routes()
        assert len(routes) == 2
        names = {r["name"] for r in routes}
        assert "Jellyfin" in names
        assert "Vaultwarden" in names

    def test_config_survives_reload(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr._config.http_port = 9090
        mgr._config.bind_address = "10.0.0.1"
        mgr._save()

        mgr2 = _manager(tmp_path)
        assert mgr2.get_config().http_port == 9090
        assert mgr2.get_config().bind_address == "10.0.0.1"

    def test_route_fields_preserved_on_reload(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.add_route(
            "Test", "test.home", "http://localhost:3333",
            tls_mode="off",
            strip_prefix="/api",
            extra_headers={"X-Real-IP": "{remote_host}"},
        )
        mgr2 = _manager(tmp_path)
        routes = mgr2.list_routes()
        assert len(routes) == 1
        r = routes[0]
        assert r["tls_mode"] == "off"
        assert r["strip_prefix"] == "/api"
        assert r["extra_headers"] == {"X-Real-IP": "{remote_host}"}


# ---------------------------------------------------------------------------
# TestStateFile
# ---------------------------------------------------------------------------

class TestStateFile:
    def test_state_file_mode_600(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.add_route("Check", "check.home", "http://localhost:1")
        state_file = tmp_path / "local_proxy_state.json"
        assert state_file.exists()
        mode = oct(state_file.stat().st_mode)[-3:]
        assert mode == "600"

    def test_state_file_is_valid_json(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.add_route("JSON", "json.home", "http://localhost:2")
        state_file = tmp_path / "local_proxy_state.json"
        data = json.loads(state_file.read_text())
        assert "config" in data
        assert "routes" in data
