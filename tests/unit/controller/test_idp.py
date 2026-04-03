# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Unit tests for idp.py — sessions, OIDC discovery."""
import sys
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).parent.parent.parent.parent / "controller"))
pytestmark = pytest.mark.unit


@pytest.fixture
def idp_instance(tmp_path):
    from idp import IdentityProvider, IdPConfig
    from users import UserManager
    user_mgr = UserManager(tmp_path / "users.json")
    user_mgr.create_user("testuser", "Test User", password="pw")
    cfg = IdPConfig(enabled=True, issuer_url="http://localhost:7380")
    return IdentityProvider(config=cfg, user_manager=user_mgr)


class TestIdPConfig:
    def test_discovery_document_keys(self, idp_instance):
        doc = idp_instance.oidc_discovery()
        assert "issuer" in doc
        assert "authorization_endpoint" in doc
        assert "token_endpoint" in doc
        assert "jwks_uri" in doc
        assert "userinfo_endpoint" in doc

    def test_discovery_issuer_matches_config(self, idp_instance):
        doc = idp_instance.oidc_discovery()
        assert doc["issuer"] == "http://localhost:7380"

    def test_enabled_property(self, idp_instance):
        assert idp_instance.enabled is True

    def test_disabled_idp(self, tmp_path):
        from idp import IdentityProvider, IdPConfig
        from users import UserManager
        user_mgr = UserManager(tmp_path / "users.json")
        cfg = IdPConfig(enabled=False)
        idp = IdentityProvider(config=cfg, user_manager=user_mgr)
        assert not idp.enabled


class TestSessions:
    def test_create_session_returns_token(self, idp_instance):
        session_id = idp_instance.create_session("user-id-1")
        assert session_id
        assert isinstance(session_id, str)
        assert len(session_id) > 8

    def test_validate_session_returns_user_id(self, idp_instance):
        session_id = idp_instance.create_session("user-id-2")
        resolved = idp_instance.validate_session(session_id)
        assert resolved == "user-id-2"

    def test_revoke_session(self, idp_instance):
        session_id = idp_instance.create_session("user-id-3")
        idp_instance.revoke_session(session_id)
        assert idp_instance.validate_session(session_id) is None

    def test_invalid_session_returns_none(self, idp_instance):
        assert idp_instance.validate_session("invalid-garbage") is None
        assert idp_instance.validate_session("") is None

    def test_multiple_sessions_independent(self, idp_instance):
        s1 = idp_instance.create_session("user-A")
        s2 = idp_instance.create_session("user-B")
        assert s1 != s2
        assert idp_instance.validate_session(s1) == "user-A"
        assert idp_instance.validate_session(s2) == "user-B"

    def test_session_count(self, idp_instance):
        assert idp_instance.active_session_count() == 0
        idp_instance.create_session("u1")
        idp_instance.create_session("u2")
        assert idp_instance.active_session_count() == 2

    def test_revoked_session_reduces_count(self, idp_instance):
        s = idp_instance.create_session("u1")
        count_before = idp_instance.active_session_count()
        idp_instance.revoke_session(s)
        assert idp_instance.active_session_count() == count_before - 1


class TestJWKS:
    def test_jwks_has_keys(self, idp_instance):
        jwks = idp_instance.jwks()
        assert "keys" in jwks
        assert isinstance(jwks["keys"], list)

    def test_jwks_with_signing_key(self, tmp_path):
        from idp import IdentityProvider, IdPConfig
        from users import UserManager
        from transport import IdentityKeyPair
        user_mgr = UserManager(tmp_path / "users.json")
        cfg = IdPConfig(enabled=True, issuer_url="http://localhost:7380")
        kp = IdentityKeyPair.generate()
        idp = IdentityProvider(config=cfg, user_manager=user_mgr, signing_key=kp)
        jwks = idp.jwks()
        assert len(jwks["keys"]) >= 1
