# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Controller meeting-detection report: typed pass-through and absent path.

The controller never scans processes. It consumes the supervisor's existing
``meeting`` field and whitelists it. These tests use a local HTTP server as the
fake supervisor and cover active/idle, absent, inconsistent, unconfigured, and
bounded/redacted payloads.
"""

import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import pytest

from api import build_app
from auth import AuthConfig
from meeting_status import normalise_meeting_status
from scenarios import ScenarioManager
from state import AppState
from stream import StreamManager

pytestmark = pytest.mark.unit

TOKEN = "supervisor-token-must-not-leak"


class _FakeSupervisorHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_GET(self) -> None:  # noqa: N802 — http.server API
        self.server.requests.append(
            {
                "path": self.path,
                "headers": {k.lower(): v for k, v in self.headers.items()},
            }
        )
        status, body = self.server.responder(self.path)
        payload = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, *args) -> None:
        pass


def _meeting_doc() -> dict:
    return {
        "ok": True,
        "version": "0.1.0",
        "profile": "hypervisor",
        "node_id": "ozma-virtual-node",
        "status_port": 9555,
        "supervisor": {},
        "vms": [],
        "meeting": {
            "enabled": True,
            "state": "running",
            "meeting_state": "active",
            "connected": True,
            "manage_child": False,
            "detector_interval_secs": 5,
            "poll_interval_ms": 2000,
            "restart_count": 1,
            "last_update_unix_ms": 1_700_000_000_000,
            "last_update_age_ms": 120,
            "last_detection_unix_ms": 1_700_000_000_000,
            "last_detection": None,
            "meetings": [
                {
                    "id": "zoom-1",
                    "platform": "zoom",
                    "active": True,
                    "started_at_unix_ms": 1_700_000_000_000,
                    "duration_s": None,
                    "title": "Daily standup",
                    "organizer": "host@example.test",
                    "attendee_count": 4,
                    "has_calendar_metadata": True,
                    # Hostile extra keys must never pass through.
                    "attendees": ["a@example.test"],
                    "calendar_uid": "secret-calendar-uid",
                    "socket_path": "/run/user/1000/ozma/meeting.sock",
                    "token": TOKEN,
                }
            ],
            "error": None,
            "socket_path": "/run/user/1000/ozma/meeting.sock",
            "binary": "/usr/local/bin/ozma-meeting-detect",
        },
    }


def _build_app(tmp_path: Path, *, supervisor_url: str = "", supervisor_token: str = ""):
    state = AppState()
    streams = StreamManager(state, static_dir=tmp_path / "streams")
    scenarios = ScenarioManager(tmp_path / "scenarios.json", state)
    return build_app(
        state,
        scenarios,
        streams=streams,
        auth_config=AuthConfig(enabled=False),
        supervisor_status_url=supervisor_url,
        supervisor_status_token=supervisor_token,
    )


@pytest.fixture
def fake_supervisor():
    server = ThreadingHTTPServer(("127.0.0.1", 0), _FakeSupervisorHandler)
    server.requests = []
    server.responder = lambda path: (200, _meeting_doc())
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield server
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)


@pytest.fixture(autouse=True)
def _isolate_mesh_config(monkeypatch, tmp_path):
    import mesh_network

    monkeypatch.setattr(mesh_network, "CONFIG_PATH", tmp_path / "mesh_network.json")


def _client(app):
    from fastapi.testclient import TestClient

    return TestClient(app)


def test_meeting_status_is_a_typed_passthrough(fake_supervisor, tmp_path):
    url = f"http://127.0.0.1:{fake_supervisor.server_address[1]}/status"
    client = _client(_build_app(tmp_path, supervisor_url=url))

    response = client.get("/api/v1/meeting/status")

    assert response.status_code == 200, response.text
    payload = response.json()
    assert payload["ok"] is True
    assert payload["available"] is True
    meeting = payload["meeting"]
    assert meeting["state"] == "running"
    assert meeting["meeting_state"] == "active"
    assert meeting["connected"] is True
    assert meeting["detector_interval_secs"] == 5
    assert meeting["poll_interval_ms"] == 2000
    assert meeting["last_update_age_ms"] == 120
    assert meeting["meetings"][0]["platform"] == "zoom"
    assert meeting["meetings"][0]["title"] == "Daily standup"
    assert meeting["meetings"][0]["attendee_count"] == 4
    assert meeting["meetings"][0]["has_calendar_metadata"] is True
    assert fake_supervisor.requests[-1]["path"] == "/status"


def test_meeting_report_drops_paths_attendees_tokens_and_calendar_ids(
    fake_supervisor, tmp_path
):
    url = f"http://127.0.0.1:{fake_supervisor.server_address[1]}/status"
    client = _client(
        _build_app(
            tmp_path,
            supervisor_url=url,
            supervisor_token=TOKEN,
        )
    )

    response = client.get("/api/v1/meeting/status")

    assert response.status_code == 200, response.text
    body = response.text
    assert TOKEN not in body
    assert "/usr/local/bin/ozma-meeting-detect" not in body
    assert "/run/user/1000/ozma/meeting.sock" not in body
    assert "secret-calendar-uid" not in body
    assert "a@example.test" not in body
    meeting = response.json()["meeting"]
    reported = meeting["meetings"][0]
    for forbidden in ("attendees", "calendar_uid", "socket_path", "token", "binary"):
        assert forbidden not in reported
    assert fake_supervisor.requests[-1]["headers"]["authorization"] == f"Bearer {TOKEN}"


def test_absent_meeting_field_keeps_the_endpoint_available_but_disabled(
    fake_supervisor, tmp_path
):
    doc = _meeting_doc()
    doc.pop("meeting")
    fake_supervisor.responder = lambda path: (200, doc)
    url = f"http://127.0.0.1:{fake_supervisor.server_address[1]}/status"
    client = _client(_build_app(tmp_path, supervisor_url=url))

    response = client.get("/api/v1/meeting/status")

    assert response.status_code == 200, response.text
    payload = response.json()
    assert payload["available"] is False
    assert payload["meeting"]["state"] == "disabled"
    assert payload["meeting"]["meeting_state"] == "unknown"
    assert payload["meeting"]["meetings"] == []


def test_unconfigured_supervisor_is_a_clear_503(tmp_path):
    client = _client(_build_app(tmp_path))

    response = client.get("/api/v1/meeting/status")

    assert response.status_code == 503
    assert "OZMA_SUPERVISOR_STATUS_URL" in response.json()["detail"]


def test_active_without_a_meeting_entry_is_not_fabricated(fake_supervisor, tmp_path):
    doc = _meeting_doc()
    doc["meeting"]["meeting_state"] = "active"
    doc["meeting"]["meetings"] = []
    doc["meeting"]["last_detection"] = None
    fake_supervisor.responder = lambda path: (200, doc)
    url = f"http://127.0.0.1:{fake_supervisor.server_address[1]}/status"
    client = _client(_build_app(tmp_path, supervisor_url=url))

    response = client.get("/api/v1/meeting/status")

    assert response.status_code == 200, response.text
    meeting = response.json()["meeting"]
    assert meeting["meeting_state"] == "unknown"
    assert meeting["meetings"] == []
    assert meeting["error"]["code"] == "inconsistent_snapshot"


def test_payload_is_bounded(fake_supervisor, tmp_path):
    doc = _meeting_doc()
    doc["meeting"]["meetings"] = [
        {
            "id": "id-" + ("x" * 200),
            "platform": "zoom",
            "active": True,
            "started_at_unix_ms": 1_700_000_000_000,
            "title": "t" * 10_000,
            "organizer": "o" * 10_000,
            "attendee_count": 99_999,
        }
        for _ in range(40)
    ]
    fake_supervisor.responder = lambda path: (200, doc)
    url = f"http://127.0.0.1:{fake_supervisor.server_address[1]}/status"
    client = _client(_build_app(tmp_path, supervisor_url=url))

    response = client.get("/api/v1/meeting/status")

    assert response.status_code == 200, response.text
    meetings = response.json()["meeting"]["meetings"]
    assert len(meetings) == 8
    assert len(meetings[0]["title"]) == 200
    assert len(meetings[0]["organizer"]) == 128
    assert len(meetings[0]["id"]) == 64
    assert meetings[0]["attendee_count"] == 1024


def test_normaliser_redacts_paths_and_bearer_tokens_in_errors():
    report = normalise_meeting_status(
        {
            "enabled": True,
            "state": "unavailable",
            "meeting_state": "unknown",
            "connected": False,
            "error": {
                "code": "socket_missing",
                "message": "failed /run/user/1000/meeting.sock with Bearer abc123",
            },
        }
    )

    assert report["error"]["code"] == "socket_missing"
    assert "/run/user/1000" not in report["error"]["message"]
    assert "abc123" not in report["error"]["message"]
    assert "<path>" in report["error"]["message"]
    assert "Bearer <redacted>" in report["error"]["message"]


def test_disabled_meeting_supervisor_does_not_claim_a_meeting():
    report = normalise_meeting_status(
        {
            "enabled": False,
            "state": "disabled",
            "meeting_state": "active",
            "meetings": [],
        }
    )

    assert report["enabled"] is False
    assert report["state"] == "disabled"
    assert report["meeting_state"] == "unknown"
    assert report["meetings"] == []


def test_hostile_types_are_typed_instead_of_raising():
    report = normalise_meeting_status(
        {
            "enabled": "yes",
            "state": ["running"],
            "meeting_state": {"nested": "active"},
            "meetings": [
                {
                    "platform": ["zoom"],
                    "active": True,
                    "attendee_count": True,
                    "title": ["not", "text"],
                }
            ],
            "error": {"code": ["socket_missing"], "message": ["no"]},
        }
    )

    assert report["enabled"] is False
    assert report["state"] == "disabled"
    assert report["meeting_state"] == "active"
    assert report["meetings"][0]["platform"] == "unknown"
    assert report["meetings"][0]["attendee_count"] == 0
    assert report["meetings"][0]["title"] == ""
    assert report["error"]["code"] == "unknown"


def test_status_snapshot_points_at_the_meeting_endpoint(tmp_path):
    client = _client(_build_app(tmp_path))

    response = client.get("/api/v1/status")

    assert response.status_code == 200, response.text
    payload = response.json()
    assert payload["meeting_detect"]["configured"] is False
    assert payload["meeting_detect"]["status_url"] == "/api/v1/meeting/status"
