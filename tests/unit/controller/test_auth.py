# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Unit tests for auth.py — password hashing, JWT creation/verification."""
import base64
import json
import sys
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).parent.parent.parent.parent / "controller"))
pytestmark = pytest.mark.unit


class TestPasswordHashing:
    def test_hash_and_verify(self):
        from auth import hash_password, verify_password
        h = hash_password("hunter2")
        assert verify_password("hunter2", h)
        assert not verify_password("wrong", h)

    def test_case_sensitive(self):
        from auth import hash_password, verify_password
        h = hash_password("secret")
        assert not verify_password("Secret", h)

    def test_empty_password(self):
        from auth import hash_password, verify_password
        h = hash_password("")
        assert verify_password("", h)
        assert not verify_password("notempty", h)

    def test_generate_admin_password(self):
        from auth import generate_admin_password
        pw = generate_admin_password()
        assert len(pw) >= 16
        # Two calls produce different passwords
        assert generate_admin_password() != generate_admin_password()


class TestJWT:
    @pytest.fixture
    def keypair(self):
        from transport import IdentityKeyPair
        return IdentityKeyPair.generate()

    def test_create_and_verify(self, keypair):
        from auth import create_jwt, verify_jwt
        token = create_jwt(keypair, ["read", "write"], subject="user-1")
        claims = verify_jwt(token, keypair.public_key)
        assert claims is not None
        assert claims["sub"] == "user-1"
        assert "read" in claims["scopes"]
        assert "write" in claims["scopes"]

    def test_default_subject_is_admin(self, keypair):
        from auth import create_jwt, verify_jwt
        token = create_jwt(keypair, ["read"])
        claims = verify_jwt(token, keypair.public_key)
        assert claims["sub"] == "admin"

    def test_expired_token_rejected(self, keypair):
        from auth import create_jwt, verify_jwt
        token = create_jwt(keypair, ["read"], expiry_seconds=-1)
        assert verify_jwt(token, keypair.public_key) is None

    def test_wrong_key_rejected(self, keypair):
        from auth import create_jwt, verify_jwt
        from transport import IdentityKeyPair
        token = create_jwt(keypair, ["read"])
        other = IdentityKeyPair.generate()
        assert verify_jwt(token, other.public_key) is None

    def test_tampered_payload_rejected(self, keypair):
        from auth import create_jwt, verify_jwt
        token = create_jwt(keypair, ["read"])
        parts = token.split(".")
        # Decode, escalate scopes, re-encode without re-signing
        payload = json.loads(base64.urlsafe_b64decode(parts[1] + "=="))
        payload["scopes"] = ["admin"]
        parts[1] = base64.urlsafe_b64encode(
            json.dumps(payload).encode()
        ).rstrip(b"=").decode()
        tampered = ".".join(parts)
        assert verify_jwt(tampered, keypair.public_key) is None

    def test_malformed_token_returns_none(self, keypair):
        from auth import verify_jwt
        assert verify_jwt("not.a.jwt", keypair.public_key) is None
        assert verify_jwt("", keypair.public_key) is None

    def test_audience_validation(self, keypair):
        from auth import create_jwt, verify_jwt
        # Token without audience claim
        token = create_jwt(keypair, ["read"], subject="user-1")
        # Should pass when no audience expected
        assert verify_jwt(token, keypair.public_key, expected_audience=None) is not None
        # Should fail when audience is expected but not present
        assert verify_jwt(token, keypair.public_key, expected_audience="ozma-controller") is None

    def test_issuer_validation(self, keypair):
        from auth import create_jwt, verify_jwt
        # Token without issuer claim
        token = create_jwt(keypair, ["read"], subject="user-1")
        # Should pass when no issuer expected
        assert verify_jwt(token, keypair.public_key, expected_issuer=None) is not None
        # Should fail when issuer is expected but not present
        assert verify_jwt(token, keypair.public_key, expected_issuer="ozma-controller") is None

    def test_correct_audience_issuer_pass(self, keypair):
        from auth import create_jwt, verify_jwt
        token = create_jwt(
            keypair, ["read"], subject="user-1",
            audience="ozma-controller",
            issuer="ozma-controller"
        )
        claims = verify_jwt(
            token, keypair.public_key,
            expected_audience="ozma-controller",
            expected_issuer="ozma-controller"
        )
        assert claims is not None
        assert claims["aud"] == "ozma-controller"
        assert claims["iss"] == "ozma-controller"

    def test_wrong_audience_rejected(self, keypair):
        from auth import create_jwt, verify_jwt
        token = create_jwt(
            keypair, ["read"], subject="user-1",
            audience="wrong-audience"
        )
        assert verify_jwt(
            token, keypair.public_key,
            expected_audience="ozma-controller"
        ) is None

    def test_wrong_issuer_rejected(self, keypair):
        from auth import create_jwt, verify_jwt
        token = create_jwt(
            keypair, ["read"], subject="user-1",
            issuer="wrong-issuer"
        )
        assert verify_jwt(
            token, keypair.public_key,
            expected_issuer="ozma-controller"
        ) is None

    def test_audience_as_list(self, keypair):
        from auth import create_jwt, verify_jwt
        # Create token with audience as list
        header = {"alg": "EdDSA", "typ": "JWT"}
        now = int(__import__("time").time())
        payload = {
            "sub": "user-1",
            "scopes": ["read"],
            "iat": now,
            "exp": now + 86400,
            "aud": ["ozma-controller", "ozma-dashboard"]
        }
        from auth import _b64url_encode
        import json
        header_b64 = _b64url_encode(json.dumps(header, separators=(",", ":")).encode())
        payload_b64 = _b64url_encode(json.dumps(payload, separators=(",", ":")).encode())
        signing_input = f"{header_b64}.{payload_b64}".encode()
        signature = keypair.sign(signing_input)
        sig_b64 = _b64url_encode(signature)
        token = f"{header_b64}.{payload_b64}.{sig_b64}"

        # Should pass with matching audience in list
        assert verify_jwt(
            token, keypair.public_key,
            expected_audience="ozma-controller"
        ) is not None
        # Should pass with different matching audience in list
        assert verify_jwt(
            token, keypair.public_key,
            expected_audience="ozma-dashboard"
        ) is not None
        # Should fail with non-matching audience
        assert verify_jwt(
            token, keypair.public_key,
            expected_audience="other-service"
        ) is None


class TestWireGuardBypass:
    def test_wg_ip_allowed(self):
        from auth import AuthConfig, is_wireguard_source
        cfg = AuthConfig()
        assert is_wireguard_source("10.200.5.1", cfg)

    def test_wg_ip_boundary(self):
        from auth import AuthConfig, is_wireguard_source
        cfg = AuthConfig()
        assert is_wireguard_source("10.200.0.1", cfg)
        assert is_wireguard_source("10.200.255.254", cfg)

    def test_external_ip_denied(self):
        from auth import AuthConfig, is_wireguard_source
        cfg = AuthConfig()
        assert not is_wireguard_source("1.2.3.4", cfg)
        assert not is_wireguard_source("192.168.1.1", cfg)
        assert not is_wireguard_source("10.0.0.1", cfg)   # different /16
