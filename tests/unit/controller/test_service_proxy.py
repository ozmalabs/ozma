# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Unit tests for service_proxy.py — SSRF validation, service CRUD."""
import sys
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).parent.parent.parent.parent / "controller"))
pytestmark = pytest.mark.unit


class TestSSRF:
    @pytest.mark.parametrize("host", [
        "169.254.169.254",   # AWS/GCP metadata endpoint
        "169.254.0.1",       # link-local
        "0.0.0.0",
        "100.64.0.1",        # carrier-grade NAT (RFC 6598)
        "::1",               # IPv6 loopback
    ])
    def test_blocked_hosts_raise(self, host):
        from service_proxy import validate_target_host
        with pytest.raises(ValueError):
            validate_target_host(host)

    @pytest.mark.parametrize("host", [
        "192.168.1.100",
        "10.0.0.5",
        "jellyfin.lan",
    ])
    def test_allowed_hosts_pass(self, host):
        from service_proxy import validate_target_host
        validate_target_host(host)   # must not raise

    def test_google_metadata_blocked(self):
        from service_proxy import validate_target_host
        with pytest.raises(ValueError):
            validate_target_host("metadata.google.internal")


class TestSubdomainValidation:
    @pytest.mark.parametrize("sub", ["api", "auth", "admin", "www", "static"])
    def test_reserved_subdomains_in_blocklist(self, sub):
        from service_proxy import _RESERVED_SUBDOMAINS
        assert sub in _RESERVED_SUBDOMAINS

    def test_reserved_subdomain_raises(self):
        from service_proxy import validate_subdomain
        with pytest.raises(ValueError):
            validate_subdomain("api")

    @pytest.mark.parametrize("sub", ["jellyfin", "immich", "my-app", "gitea"])
    def test_valid_subdomains_pass(self, sub):
        from service_proxy import validate_subdomain
        validate_subdomain(sub)   # must not raise

    @pytest.mark.parametrize("sub", ["", "-bad", "bad-", "UPPER", "a" * 64])
    def test_invalid_format_raises(self, sub):
        from service_proxy import validate_subdomain
        with pytest.raises(ValueError):
            validate_subdomain(sub)


class TestServiceCRUD:
    @pytest.fixture
    def svc_mgr(self, tmp_path):
        from service_proxy import ServiceProxyManager
        return ServiceProxyManager(tmp_path / "services.json")

    def test_register_service(self, svc_mgr):
        svc = svc_mgr.register_service(
            name="Jellyfin", owner_user_id="u-1",
            target_host="192.168.1.10", target_port=8096,
        )
        assert svc.name == "Jellyfin"
        assert svc.target_port == 8096
        assert svc_mgr.get_service(svc.id) is not None

    def test_get_unknown_service_returns_none(self, svc_mgr):
        assert svc_mgr.get_service("no-such-id") is None

    def test_remove_service(self, svc_mgr):
        svc = svc_mgr.register_service("X", "u-1", "10.0.0.1", 80)
        assert svc_mgr.remove_service(svc.id)
        assert svc_mgr.get_service(svc.id) is None

    def test_remove_nonexistent_returns_false(self, svc_mgr):
        assert not svc_mgr.remove_service("no-such-id")

    def test_list_services(self, svc_mgr):
        svc_mgr.register_service("A", "u-1", "10.0.0.1", 80)
        svc_mgr.register_service("B", "u-1", "10.0.0.2", 8080)
        assert len(svc_mgr.list_services()) == 2

    def test_target_url(self, svc_mgr):
        svc = svc_mgr.register_service("Svc", "u-1", "192.168.1.5", 9000,
                                        protocol="https")
        assert svc.target_url() == "https://192.168.1.5:9000"

    def test_persistence(self, tmp_path):
        from service_proxy import ServiceProxyManager
        mgr1 = ServiceProxyManager(tmp_path / "services.json")
        svc = mgr1.register_service("Persist", "u-1", "10.0.0.1", 8080)
        mgr2 = ServiceProxyManager(tmp_path / "services.json")
        assert mgr2.get_service(svc.id) is not None
