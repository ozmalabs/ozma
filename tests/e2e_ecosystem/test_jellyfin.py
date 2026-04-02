"""
E2E tests for Jellyfin integration.

Tests the full flow: register Jellyfin as an Ozma service → create a
share grant → verify the controller-side proxy filter would scope access.
"""

import pytest
import httpx
from conftest import OzmaClient, JELLYFIN_URL


class TestJellyfinServiceRegistration:
    """Register Jellyfin with the Ozma controller."""

    def test_register_jellyfin(self, ozma: OzmaClient, jellyfin_ready: bool):
        assert jellyfin_ready

        # Parse Jellyfin's URL for host/port
        from urllib.parse import urlparse
        parsed = urlparse(JELLYFIN_URL)

        r = ozma.post("/api/v1/services", json={
            "name": "Jellyfin",
            "target_host": parsed.hostname,
            "target_port": parsed.port or 8096,
            "subdomain": "jellyfin",
            "service_type": "jellyfin",
            "health_path": "/health",
        })
        assert r.status_code in (200, 409)
        if r.status_code == 200:
            assert r.json()["service_type"] == "jellyfin"

    def test_jellyfin_health_check(self, ozma: OzmaClient):
        services = ozma.get("/api/v1/services").json()
        jf = next((s for s in services if s["service_type"] == "jellyfin"), None)
        assert jf, "Jellyfin service not registered"

        r = ozma.get(f"/api/v1/services/{jf['id']}/health")
        assert r.status_code in (200, 409)
        assert r.json()["healthy"] is True


class TestJellyfinAPI:
    """Verify Jellyfin is running and accessible."""

    def test_jellyfin_system_info(self, jellyfin_api_key: str):
        if not jellyfin_api_key:
            pytest.skip("Jellyfin API key not available")
        r = httpx.get(f"{JELLYFIN_URL}/System/Info/Public", timeout=10)
        assert r.status_code in (200, 409)
        info = r.json()
        assert "ServerName" in info
        assert "Version" in info

    def test_jellyfin_libraries(self, jellyfin_api_key: str):
        if not jellyfin_api_key:
            pytest.skip("Jellyfin API key not available")
        r = httpx.get(f"{JELLYFIN_URL}/Library/VirtualFolders",
                      headers={"X-Emby-Token": jellyfin_api_key}, timeout=10)
        assert r.status_code in (200, 409)


class TestJellyfinSharing:
    """Test Jellyfin library sharing via Ozma grants."""

    def test_create_jellyfin_library_share(self, ozma: OzmaClient, jellyfin_api_key: str):
        """Create a share grant for a Jellyfin library."""
        if not jellyfin_api_key:
            pytest.skip("Jellyfin API key not available")

        users = ozma.get("/api/v1/users").json()
        alice = next((u for u in users if u["username"] == "alice"), None)
        bob = next((u for u in users if u["username"] == "bob"), None)
        if not alice or not bob:
            pytest.skip("Need alice and bob users — run test_ozma_core first")

        # Get the Jellyfin service ID
        services = ozma.get("/api/v1/services").json()
        jf = next((s for s in services if s["service_type"] == "jellyfin"), None)
        if not jf:
            pytest.skip("Jellyfin not registered — run test_register_jellyfin first")

        # Authenticate as alice
        r = ozma.client.post("/api/v1/auth/token", json={
            "username": "alice", "password": "alicepass123",
        })
        alice_token = r.json()["token"]

        # Create a library share grant
        r = ozma.client.post("/api/v1/shares", json={
            "grantee_user_id": bob["id"],
            "resource_type": "jellyfin_library",
            "resource_id": "all",  # would be a real library UUID in production
            "alias": "alices-movies",
            "permissions": ["read"],
        }, headers={"Authorization": f"Bearer {alice_token}"})
        assert r.status_code in (200, 409)
        grant = r.json()
        assert grant["resource_type"] == "jellyfin_library"
        assert grant["alias"] == "alices-movies"
        assert grant["active"] is True
