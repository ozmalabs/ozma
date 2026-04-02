"""
E2E tests for Frigate NVR integration.

Tests: register Frigate as Ozma service → verify API → create camera share.
Frigate runs in mock mode (no cameras) so we test the API surface only.
"""

import pytest
import httpx
from conftest import OzmaClient, FRIGATE_URL


class TestFrigateServiceRegistration:

    def test_register_frigate(self, ozma: OzmaClient, frigate_ready: bool):
        assert frigate_ready, "Frigate not ready"

        from urllib.parse import urlparse
        parsed = urlparse(FRIGATE_URL)

        r = ozma.post("/api/v1/services", json={
            "name": "Frigate",
            "target_host": parsed.hostname,
            "target_port": parsed.port or 5000,
            "subdomain": "cameras",
            "service_type": "frigate",
            "health_path": "/api/version",
        })
        assert r.status_code in (200, 409)
        if r.status_code == 200:
            assert r.json()["service_type"] == "frigate"

    def test_frigate_health_check(self, ozma: OzmaClient):
        services = ozma.get("/api/v1/services").json()
        fg = next((s for s in services if s["service_type"] == "frigate"), None)
        assert fg, "Frigate service not registered"

        r = ozma.get(f"/api/v1/services/{fg['id']}/health")
        assert r.status_code in (200, 409)
        assert r.json()["healthy"] is True


class TestFrigateAPI:

    def test_frigate_version(self, frigate_ready: bool):
        assert frigate_ready
        r = httpx.get(f"{FRIGATE_URL}/api/version", timeout=10)
        assert r.status_code in (200, 409)

    def test_frigate_config(self, frigate_ready: bool):
        assert frigate_ready
        r = httpx.get(f"{FRIGATE_URL}/api/config", timeout=10)
        assert r.status_code in (200, 409)
        config = r.json()
        assert "mqtt" in config

    def test_frigate_stats(self, frigate_ready: bool):
        assert frigate_ready
        r = httpx.get(f"{FRIGATE_URL}/api/stats", timeout=10)
        assert r.status_code in (200, 409)

    def test_frigate_events_empty(self, frigate_ready: bool):
        assert frigate_ready
        r = httpx.get(f"{FRIGATE_URL}/api/events", timeout=10)
        assert r.status_code in (200, 409)
        assert isinstance(r.json(), list)


class TestFrigateSharing:

    def test_create_frigate_camera_share(self, ozma: OzmaClient, frigate_ready: bool):
        assert frigate_ready

        users = ozma.get("/api/v1/users").json()
        alice = next((u for u in users if u["username"] == "alice"), None)
        bob = next((u for u in users if u["username"] == "bob"), None)
        if not alice or not bob:
            pytest.skip("Need alice and bob users")

        # Authenticate as alice
        r = ozma.client.post("/api/v1/auth/token", json={
            "username": "alice", "password": "alicepass123",
        })
        alice_token = r.json()["token"]

        # Share a camera (mock — no real cameras in test config)
        r = ozma.client.post("/api/v1/shares", json={
            "grantee_user_id": bob["id"],
            "resource_type": "frigate_camera",
            "resource_id": "front_door",
            "alias": "alices-front-door",
            "permissions": ["read"],
        }, headers={"Authorization": f"Bearer {alice_token}"})
        assert r.status_code in (200, 409)
        grant = r.json()
        assert grant["resource_type"] == "frigate_camera"
        assert grant["active"] is True

    def test_create_frigate_feed_only_share(self, ozma: OzmaClient, frigate_ready: bool):
        assert frigate_ready

        users = ozma.get("/api/v1/users").json()
        alice = next((u for u in users if u["username"] == "alice"), None)
        bob = next((u for u in users if u["username"] == "bob"), None)
        if not alice or not bob:
            pytest.skip("Need alice and bob users")

        r = ozma.client.post("/api/v1/auth/token", json={
            "username": "alice", "password": "alicepass123",
        })
        alice_token = r.json()["token"]

        # Feed-only share (live stream, no recordings/events)
        r = ozma.client.post("/api/v1/shares", json={
            "grantee_user_id": bob["id"],
            "resource_type": "frigate_feed",
            "resource_id": "driveway",
            "alias": "alices-driveway-live",
            "permissions": ["read"],
        }, headers={"Authorization": f"Bearer {alice_token}"})
        assert r.status_code in (200, 409)
        assert r.json()["resource_type"] == "frigate_feed"
