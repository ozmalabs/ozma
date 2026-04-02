"""
E2E tests for Immich integration.

Tests: register Immich as Ozma service → verify API → create album share.
"""

import pytest
import httpx
from conftest import OzmaClient, IMMICH_URL


class TestImmichServiceRegistration:

    def test_register_immich(self, ozma: OzmaClient, immich_api_key: str):
        assert immich_api_key, "Immich setup failed — no API key"

        from urllib.parse import urlparse
        parsed = urlparse(IMMICH_URL)

        r = ozma.post("/api/v1/services", json={
            "name": "Immich",
            "target_host": parsed.hostname,
            "target_port": parsed.port or 2283,
            "subdomain": "photos",
            "service_type": "immich",
            "health_path": "/api/server/ping",
        })
        assert r.status_code in (200, 409)
        if r.status_code == 200:
            assert r.json()["service_type"] == "immich"

    def test_immich_health_check(self, ozma: OzmaClient):
        services = ozma.get("/api/v1/services").json()
        imm = next((s for s in services if s["service_type"] == "immich"), None)
        assert imm, "Immich service not registered"

        r = ozma.get(f"/api/v1/services/{imm['id']}/health")
        assert r.status_code in (200, 409)
        assert r.json()["healthy"] is True


class TestImmichAPI:

    def test_immich_server_info(self, immich_api_key: str):
        assert immich_api_key
        r = httpx.get(f"{IMMICH_URL}/api/server/ping", timeout=10)
        assert r.status_code in (200, 409)
        assert r.json().get("res") == "pong"

    def test_immich_create_album(self, immich_api_key: str):
        """Create a test album for sharing tests."""
        assert immich_api_key
        r = httpx.post(f"{IMMICH_URL}/api/albums",
                       json={"albumName": "E2E Test Album"},
                       headers={"Authorization": f"Bearer {immich_api_key}"}, timeout=10)
        assert r.status_code in (200, 201)
        album = r.json()
        assert album["albumName"] == "E2E Test Album"


class TestImmichSharing:

    def test_create_immich_album_share(self, ozma: OzmaClient, immich_api_key: str):
        assert immich_api_key

        users = ozma.get("/api/v1/users").json()
        alice = next((u for u in users if u["username"] == "alice"), None)
        bob = next((u for u in users if u["username"] == "bob"), None)
        if not alice or not bob:
            pytest.skip("Need alice and bob users")

        # Get Immich service
        services = ozma.get("/api/v1/services").json()
        imm = next((s for s in services if s["service_type"] == "immich"), None)
        if not imm:
            pytest.skip("Immich not registered")

        # Get the test album ID
        r = httpx.get(f"{IMMICH_URL}/api/albums",
                      headers={"Authorization": f"Bearer {immich_api_key}"}, timeout=10)
        albums = r.json()
        test_album = next((a for a in albums if a["albumName"] == "E2E Test Album"), None)
        if not test_album:
            pytest.skip("Test album not created")

        # Authenticate as alice and create share
        r = ozma.client.post("/api/v1/auth/token", json={
            "username": "alice", "password": "alicepass123",
        })
        alice_token = r.json()["token"]

        r = ozma.client.post("/api/v1/shares", json={
            "grantee_user_id": bob["id"],
            "resource_type": "immich_album",
            "resource_id": test_album["id"],
            "alias": "alices-photos",
            "permissions": ["read"],
        }, headers={"Authorization": f"Bearer {alice_token}"})
        assert r.status_code in (200, 409)
        grant = r.json()
        assert grant["resource_type"] == "immich_album"
        assert grant["active"] is True
