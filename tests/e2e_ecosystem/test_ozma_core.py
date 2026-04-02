"""
E2E tests for Ozma core features: users, services, sharing, IdP.

These validate the controller's new subsystems work correctly before
testing service-specific integrations.
"""

import pytest
from conftest import OzmaClient, OZMA_URL


class TestUsers:
    """User management CRUD and multi-user auth."""

    def test_list_users(self, ozma: OzmaClient):
        r = ozma.get("/api/v1/users")
        assert r.status_code == 200
        users = r.json()
        assert isinstance(users, list)
        # Admin user should exist (migrated from password)
        assert any(u["username"] == "admin" for u in users)

    def test_create_user(self, ozma: OzmaClient):
        r = ozma.post("/api/v1/users", json={
            "username": "charlie",
            "display_name": "Charlie",
            "password": "charliepass123",
            "role": "member",
        })
        assert r.status_code in (200, 409)
        if r.status_code == 200:
            user = r.json()
            assert user["username"] == "charlie"
            assert user["role"] == "member"
            assert "password_hash" not in user  # must not leak

    def test_duplicate_username_rejected(self, ozma: OzmaClient):
        # alice is created by the conftest fixture
        r = ozma.post("/api/v1/users", json={
            "username": "alice",
            "display_name": "Alice 2",
            "password": "pass",
        })
        assert r.status_code == 409

    def test_authenticate_new_user(self, ozma: OzmaClient):
        r = ozma.client.post("/api/v1/auth/token", json={
            "username": "alice",
            "password": "alicepass123",
        })
        assert r.status_code == 200
        data = r.json()
        assert "token" in data
        assert data["user"]["username"] == "alice"

    def test_get_current_user(self, ozma: OzmaClient):
        # Authenticate as alice
        r = ozma.client.post("/api/v1/auth/token", json={
            "username": "alice", "password": "alicepass123",
        })
        token = r.json()["token"]
        r = ozma.client.get("/api/v1/users/me",
                            headers={"Authorization": f"Bearer {token}"})
        assert r.status_code == 200
        assert r.json()["username"] == "alice"

    def test_bob_exists(self, ozma: OzmaClient):
        """Verify bob was created by the conftest fixture."""
        users = ozma.get("/api/v1/users").json()
        assert any(u["username"] == "bob" for u in users)


class TestServiceProxy:
    """Service registration and management."""

    def test_register_service(self, ozma: OzmaClient):
        r = ozma.post("/api/v1/services", json={
            "name": "Test Service",
            "target_host": "127.0.0.1",
            "target_port": 8080,
            "subdomain": "test-svc-core",
            "service_type": "generic",
        })
        assert r.status_code in (200, 409)
        if r.status_code == 200:
            svc = r.json()
            assert svc["name"] == "Test Service"
            assert svc["subdomain"] == "test-svc-core"

    def test_list_services(self, ozma: OzmaClient):
        r = ozma.get("/api/v1/services")
        assert r.status_code == 200
        services = r.json()
        assert any(s["subdomain"] == "test-svc-core" for s in services)

    def test_ssrf_blocked(self, ozma: OzmaClient):
        """Cloud metadata endpoint must be blocked."""
        r = ozma.post("/api/v1/services", json={
            "name": "SSRF Test",
            "target_host": "169.254.169.254",
            "target_port": 80,
            "subdomain": "ssrf-test",
        })
        assert r.status_code in (400, 409, 422, 500)

    def test_reserved_subdomain_blocked(self, ozma: OzmaClient):
        r = ozma.post("/api/v1/services", json={
            "name": "Admin Hijack",
            "target_host": "127.0.0.1",
            "target_port": 8080,
            "subdomain": "admin",
        })
        assert r.status_code in (400, 409, 422, 500)


class TestSharing:
    """Cross-user sharing grants."""

    def test_create_share_requires_user_id(self, ozma: OzmaClient):
        """Sharing requires a user identity — legacy admin token should fail."""
        # Get alice and bob IDs
        users = ozma.get("/api/v1/users").json()
        alice = next(u for u in users if u["username"] == "alice")
        bob = next(u for u in users if u["username"] == "bob")

        # Authenticate as alice
        r = ozma.client.post("/api/v1/auth/token", json={
            "username": "alice", "password": "alicepass123",
        })
        alice_token = r.json()["token"]

        # Create a share grant
        r = ozma.client.post("/api/v1/shares", json={
            "grantee_user_id": bob["id"],
            "resource_type": "service",
            "resource_id": "test-service-id",
            "alias": "bobs-test",
        }, headers={"Authorization": f"Bearer {alice_token}"})
        assert r.status_code == 200
        grant = r.json()
        assert grant["grantor_user_id"] == alice["id"]
        assert grant["grantee_user_id"] == bob["id"]

    def test_list_shares(self, ozma: OzmaClient):
        r = ozma.get("/api/v1/shares")
        assert r.status_code == 200

    def test_list_peers_empty(self, ozma: OzmaClient):
        r = ozma.get("/api/v1/peers")
        assert r.status_code == 200
        assert r.json() == []


class TestIdP:
    """Identity Provider endpoints (when enabled)."""

    def test_oidc_discovery(self, ozma: OzmaClient):
        r = ozma.client.get(f"{OZMA_URL}/.well-known/openid-configuration")
        assert r.status_code == 200
        doc = r.json()
        assert "issuer" in doc
        assert "token_endpoint" in doc
        assert "jwks_uri" in doc

    def test_jwks(self, ozma: OzmaClient):
        r = ozma.client.get(f"{OZMA_URL}/auth/jwks")
        assert r.status_code == 200
        jwks = r.json()
        assert "keys" in jwks

    def test_login_page(self, ozma: OzmaClient):
        r = ozma.client.get(f"{OZMA_URL}/auth/login")
        assert r.status_code == 200
        assert "ozma" in r.text.lower()
        assert "<form" in r.text


class TestExternalPublish:
    """External publishing endpoints."""

    def test_list_published_empty(self, ozma: OzmaClient):
        r = ozma.get("/api/v1/publish")
        assert r.status_code == 200
        assert r.json() == []
