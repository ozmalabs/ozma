"""
E2E tests for Audiobookshelf integration.

Tests: register ABS as Ozma service → verify API → create library share.
"""

import pytest
import httpx
from conftest import OzmaClient, ABS_URL


class TestABSServiceRegistration:

    def test_register_abs(self, ozma: OzmaClient, abs_api_key: str):
        assert abs_api_key, "ABS setup failed — no token"

        from urllib.parse import urlparse
        parsed = urlparse(ABS_URL)

        r = ozma.post("/api/v1/services", json={
            "name": "Audiobookshelf",
            "target_host": parsed.hostname,
            "target_port": parsed.port or 13378,
            "subdomain": "audiobooks",
            "service_type": "audiobookshelf",
            "health_path": "/healthcheck",
        })
        assert r.status_code in (200, 409)
        if r.status_code == 200:
            assert r.json()["service_type"] == "audiobookshelf"

    def test_abs_health_check(self, ozma: OzmaClient):
        services = ozma.get("/api/v1/services").json()
        abs_svc = next((s for s in services if s["service_type"] == "audiobookshelf"), None)
        assert abs_svc, "ABS service not registered"

        r = ozma.get(f"/api/v1/services/{abs_svc['id']}/health")
        assert r.status_code in (200, 409)  # 409 = already registered
        assert r.json()["healthy"] is True


class TestABSAPI:

    def test_abs_status(self, abs_api_key: str):
        assert abs_api_key
        r = httpx.get(f"{ABS_URL}/api/libraries",
                      headers={"Authorization": f"Bearer {abs_api_key}"}, timeout=10)
        assert r.status_code in (200, 409)  # 409 = already registered

    def test_abs_create_library(self, abs_api_key: str):
        """Create a test library for sharing tests."""
        assert abs_api_key
        r = httpx.post(f"{ABS_URL}/api/libraries",
                       json={
                           "name": "E2E Audiobooks",
                           "folders": [{"fullPath": "/metadata"}],
                           "mediaType": "book",
                       },
                       headers={"Authorization": f"Bearer {abs_api_key}"}, timeout=10)
        # 200 = created, 400 = already exists (both ok for idempotent test)
        assert r.status_code in (200, 400)


class TestABSSharing:

    def test_create_abs_library_share(self, ozma: OzmaClient, abs_api_key: str):
        assert abs_api_key

        users = ozma.get("/api/v1/users").json()
        alice = next((u for u in users if u["username"] == "alice"), None)
        bob = next((u for u in users if u["username"] == "bob"), None)
        if not alice or not bob:
            pytest.skip("Need alice and bob users")

        # Get ABS libraries
        r = httpx.get(f"{ABS_URL}/api/libraries",
                      headers={"Authorization": f"Bearer {abs_api_key}"}, timeout=10)
        libraries = r.json().get("libraries", r.json()) if isinstance(r.json(), dict) else r.json()
        if not libraries:
            pytest.skip("No ABS libraries")
        lib_id = libraries[0]["id"] if isinstance(libraries[0], dict) else libraries[0]

        # Authenticate as alice
        r = ozma.client.post("/api/v1/auth/token", json={
            "username": "alice", "password": "alicepass123",
        })
        alice_token = r.json()["token"]

        r = ozma.client.post("/api/v1/shares", json={
            "grantee_user_id": bob["id"],
            "resource_type": "abs_library",
            "resource_id": str(lib_id),
            "alias": "alices-audiobooks",
            "permissions": ["read"],
        }, headers={"Authorization": f"Bearer {alice_token}"})
        assert r.status_code in (200, 409)  # 409 = already registered
        grant = r.json()
        assert grant["resource_type"] == "abs_library"
        assert grant["active"] is True
