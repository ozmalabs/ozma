"""
Shared fixtures for ecosystem E2E tests.

Each test file tests one service integration end-to-end:
  - Ozma controller is running with auth enabled
  - The target service (Jellyfin, Immich, etc.) is running
  - Tests exercise: service registration, user creation, sharing, proxy access
"""

import os
import time

import httpx
import pytest


# ── Environment ──────────────────────────────────────────────────────────

OZMA_URL = os.environ.get("OZMA_URL", "http://localhost:7380")
OZMA_PASSWORD = os.environ.get("OZMA_PASSWORD", "testpassword123")


def pytest_addoption(parser: pytest.Parser) -> None:
    """Add custom pytest options."""
    parser.addoption(
        "--ozma-password",
        action="store",
        default=OZMA_PASSWORD,
        help="Password for admin user authentication",
    )
JELLYFIN_URL = os.environ.get("JELLYFIN_URL", "http://localhost:8096")
IMMICH_URL = os.environ.get("IMMICH_URL", "http://localhost:2283")
ABS_URL = os.environ.get("ABS_URL", "http://localhost:13378")
FRIGATE_URL = os.environ.get("FRIGATE_URL", "http://localhost:5000")


# ── Helpers ──────────────────────────────────────────────────────────────

def wait_for_service(url: str, path: str = "/health", timeout: int = 60) -> bool:
    """Wait for a service to become healthy."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            r = httpx.get(f"{url}{path}", timeout=3)
            if r.status_code < 500:
                return True
        except httpx.ConnectError:
            pass
        time.sleep(1)
    return False


class OzmaClient:
    """Thin client for the Ozma controller API."""

    def __init__(self, base_url: str, password: str) -> None:
        self.base = base_url.rstrip("/")
        self.token = ""
        self.client = httpx.Client(base_url=self.base, timeout=15)
        self._authenticate(password)

    def _authenticate(self, password: str) -> None:
        r = self.client.post("/api/v1/auth/token", json={"password": password})
        if r.status_code == 200:
            self.token = r.json()["token"]
        # Try with username if that didn't work (multi-user mode)
        if not self.token:
            r = self.client.post("/api/v1/auth/token",
                                 json={"username": "admin", "password": password})
            if r.status_code == 200:
                self.token = r.json()["token"]

    @property
    def headers(self) -> dict:
        return {"Authorization": f"Bearer {self.token}"} if self.token else {}

    def get(self, path: str) -> httpx.Response:
        return self.client.get(path, headers=self.headers)

    def post(self, path: str, **kwargs) -> httpx.Response:
        return self.client.post(path, headers=self.headers, **kwargs)

    def put(self, path: str, **kwargs) -> httpx.Response:
        return self.client.put(path, headers=self.headers, **kwargs)

    def delete(self, path: str) -> httpx.Response:
        return self.client.delete(path, headers=self.headers)


# ── Fixtures ─────────────────────────────────────────────────────────────

@pytest.fixture(scope="session")
def ozma() -> OzmaClient:
    """Authenticated Ozma controller client with alice + bob users pre-created."""
    assert wait_for_service(OZMA_URL), f"Ozma controller not ready at {OZMA_URL}"
    client = OzmaClient(OZMA_URL, OZMA_PASSWORD)

    # Ensure alice and bob exist for sharing tests (idempotent)
    for user in [
        {"username": "alice", "display_name": "Alice", "password": "alicepass123", "role": "member"},
        {"username": "bob", "display_name": "Bob", "password": "bobpass123", "role": "member"},
    ]:
        r = client.post("/api/v1/users", json=user)
        # 200 = created, 409 = already exists — both fine
        if r.status_code not in (200, 409):
            raise RuntimeError(f"Failed to create user {user['username']}: {r.status_code} {r.text}")

    return client


@pytest.fixture(scope="session")
def jellyfin_ready() -> bool:
    return wait_for_service(JELLYFIN_URL, "/health")


@pytest.fixture(scope="session")
def immich_ready() -> bool:
    return wait_for_service(IMMICH_URL, "/api/server/ping")


@pytest.fixture(scope="session")
def abs_ready() -> bool:
    return wait_for_service(ABS_URL, "/healthcheck")


@pytest.fixture(scope="session")
def frigate_ready() -> bool:
    return wait_for_service(FRIGATE_URL, "/api/version")


@pytest.fixture(scope="session")
def jellyfin_api_key(jellyfin_ready) -> str:
    """Complete Jellyfin first-run setup and return an API key."""
    assert jellyfin_ready
    client = httpx.Client(base_url=JELLYFIN_URL, timeout=15)
    auth_header = {
        "X-Emby-Authorization":
            'MediaBrowser Client="Test", Device="E2E", DeviceId="e2e", Version="1.0"'
    }

    # Check if setup wizard is already completed
    r = client.get("/System/Info/Public")
    wizard_done = r.status_code == 200 and r.json().get("StartupWizardCompleted", False)

    if not wizard_done:
        # Complete the startup wizard
        client.post("/Startup/Configuration",
                    json={"UICulture": "en-US", "MetadataCountryCode": "US",
                          "PreferredMetadataLanguage": "en"})
        client.post("/Startup/User",
                    json={"Name": "admin", "Password": "admin123"})
        client.post("/Startup/RemoteAccess",
                    json={"EnableRemoteAccess": True, "EnableAutomaticPortMapping": False})
        client.post("/Startup/Complete")

    # Try to authenticate with known credentials
    for username, password in [("admin", "admin123"), ("admin", ""), ("jellyfin", "")]:
        r = client.post("/Users/AuthenticateByName",
                        json={"Username": username, "Pw": password},
                        headers=auth_header)
        if r.status_code == 200:
            return r.json().get("AccessToken", "")

    # Last resort: Jellyfin auto-wizard may need us to use QuickConnect or
    # check if it's truly unconfigured. Return empty — tests will skip.
    return ""


@pytest.fixture(scope="session")
def immich_api_key(immich_ready) -> str:
    """Complete Immich first-run setup and return an API key."""
    assert immich_ready
    client = httpx.Client(base_url=IMMICH_URL, timeout=15)

    # Create admin user
    r = client.post("/api/auth/admin-sign-up",
                    json={"email": "admin@test.local", "password": "admin123",
                          "name": "Admin"})
    if r.status_code in (201, 400):  # 400 = already exists
        # Login
        r = client.post("/api/auth/login",
                        json={"email": "admin@test.local", "password": "admin123"})
        if r.status_code == 201:
            token = r.json().get("accessToken", "")
            # Create an API key
            r = client.post("/api/api-keys",
                            json={"name": "e2e-test"},
                            headers={"Authorization": f"Bearer {token}"})
            if r.status_code == 201:
                return r.json().get("secret", "")
            return token
    return ""


@pytest.fixture(scope="session")
def abs_api_key(abs_ready) -> str:
    """Complete ABS first-run setup and return a token."""
    assert abs_ready
    client = httpx.Client(base_url=ABS_URL, timeout=15)

    # Create root user (first-run)
    r = client.post("/init",
                    json={"newRoot": {"username": "admin", "password": "admin123"}})
    # Login
    r = client.post("/login",
                    json={"username": "admin", "password": "admin123"})
    if r.status_code == 200:
        return r.json().get("user", {}).get("token", "")
    return ""
