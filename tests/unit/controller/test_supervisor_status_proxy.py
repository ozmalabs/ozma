# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Read-only controller proxy for the supervisor multi-VM status document.

The controller must not know supervisor internals: it fetches the configured
``OZMA_SUPERVISOR_STATUS_URL`` and returns the document unchanged inside a small
envelope. These tests use a local HTTP server as the fake supervisor and assert
the proxy, error handling, token handling, and dashboard hooks.
"""

import json
import socket
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import pytest

from api import build_app
from auth import AuthConfig
from scenarios import ScenarioManager
from state import AppState
from stream import StreamManager

pytestmark = pytest.mark.unit

TOKEN = "supervisor-secret-token"


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


def _status_doc() -> dict:
    return {
        "ok": True,
        "version": "0.1.0",
        "profile": "hypervisor",
        "node_id": "ozma-virtual-node",
        "status_port": 9555,
        "child_poll_interval_ms": 5000,
        "child_poll_timeout_ms": 2000,
        "supervisor": {
            "discovered": 2,
            "tracked": 1,
            "running": 1,
            "failed": 0,
            "skipped": 0,
            "excluded": 1,
            "next_hid_port": 7610,
            "next_http_port": 7710,
        },
        "vms": [
            {
                "name": "vm-a",
                "state": "running",
                "qmp_socket": "/run/ozma/qmp/vm-a.sock",
                "hid_udp_port": 7600,
                "http_port": 7700,
                "child": {
                    "reachable": True,
                    "hls": "running",
                    "webrtc": "not-configured",
                    "stale": False,
                    "last_seen_age_ms": 250,
                },
            },
            {"name": "template-win11", "state": "excluded", "excluded": True},
        ],
    }


@pytest.fixture
def fake_supervisor():
    server = ThreadingHTTPServer(("127.0.0.1", 0), _FakeSupervisorHandler)
    server.requests = []
    server.responder = lambda path: (200, _status_doc())
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
    """Keep build_app from writing controller/mesh_network.json in the repo."""
    import mesh_network

    monkeypatch.setattr(mesh_network, "CONFIG_PATH", tmp_path / "mesh_network.json")


def _build_app(
    tmp_path: Path,
    *,
    supervisor_url: str = "",
    supervisor_token: str = "",
):
    state = AppState()
    streams = StreamManager(state, static_dir=tmp_path / "streams")
    scenarios = ScenarioManager(tmp_path / "scenarios.json", state)
    app = build_app(
        state,
        scenarios,
        streams=streams,
        auth_config=AuthConfig(enabled=False),
        supervisor_status_url=supervisor_url,
        supervisor_status_token=supervisor_token,
    )
    return app


def _client(app):
    from fastapi.testclient import TestClient

    return TestClient(app)


def test_supervisor_status_is_proxied_without_controller_parsing(fake_supervisor, tmp_path):
    url = f"http://127.0.0.1:{fake_supervisor.server_address[1]}/status"
    client = _client(_build_app(tmp_path, supervisor_url=url))

    response = client.get("/api/v1/supervisor/status")

    assert response.status_code == 200, response.text
    payload = response.json()
    assert payload["ok"] is True
    assert payload["configured"] is True
    assert payload["status"]["profile"] == "hypervisor"
    assert payload["status"]["vms"][0]["name"] == "vm-a"
    assert payload["status"]["vms"][0]["http_port"] == 7700
    assert fake_supervisor.requests
    assert fake_supervisor.requests[-1]["path"] == "/status"


def test_supervisor_token_is_sent_upstream_and_never_returned(fake_supervisor, tmp_path):
    url = f"http://127.0.0.1:{fake_supervisor.server_address[1]}/status"
    client = _client(
        _build_app(tmp_path, supervisor_url=url, supervisor_token=TOKEN)
    )

    response = client.get("/api/v1/supervisor/status")

    assert response.status_code == 200, response.text
    assert fake_supervisor.requests[-1]["headers"]["authorization"] == f"Bearer {TOKEN}"
    assert TOKEN not in response.text


def test_supervisor_status_not_configured_is_a_clear_503(tmp_path):
    client = _client(_build_app(tmp_path))

    response = client.get("/api/v1/supervisor/status")

    assert response.status_code == 503
    assert "OZMA_SUPERVISOR_STATUS_URL" in response.json()["detail"]


def test_unreachable_supervisor_is_a_clear_502(tmp_path):
    probe = socket.socket()
    probe.bind(("127.0.0.1", 0))
    port = probe.getsockname()[1]
    probe.close()
    client = _client(
        _build_app(tmp_path, supervisor_url=f"http://127.0.0.1:{port}/status")
    )

    response = client.get("/api/v1/supervisor/status")

    assert response.status_code == 502
    assert "Supervisor unreachable" in response.json()["detail"]


def test_non_supervisor_document_is_rejected(fake_supervisor, tmp_path):
    fake_supervisor.responder = lambda path: (200, {"ok": True, "profile": "vm"})
    url = f"http://127.0.0.1:{fake_supervisor.server_address[1]}/status"
    client = _client(_build_app(tmp_path, supervisor_url=url))

    response = client.get("/api/v1/supervisor/status")

    assert response.status_code == 502
    assert "unexpected profile" in response.json()["detail"]


def test_supervisor_http_error_is_a_clear_502(fake_supervisor, tmp_path):
    fake_supervisor.responder = lambda path: (500, {"error": "boom"})
    url = f"http://127.0.0.1:{fake_supervisor.server_address[1]}/status"
    client = _client(_build_app(tmp_path, supervisor_url=url))

    response = client.get("/api/v1/supervisor/status")

    assert response.status_code == 502
    assert "HTTP 500" in response.json()["detail"]


def test_dashboard_renders_the_read_only_multi_vm_panel():
    dashboard = (
        Path(__file__).resolve().parents[3]
        / "controller"
        / "static"
        / "dashboard.html"
    ).read_text()

    assert 'id="panel-vms"' in dashboard
    assert 'id="vms-list"' in dashboard
    assert "function fetchVMs()" in dashboard
    assert "function renderVMs()" in dashboard
    assert "/supervisor/status" in dashboard
    # Read-only: the VM panel must not expose control buttons.
    panel = dashboard.split('id="panel-vms"', 1)[1].split("</div>\n\n  <!--", 1)[0]
    for forbidden in ("Power", "Restart", "Delete", "Stop"):
        assert forbidden not in panel
