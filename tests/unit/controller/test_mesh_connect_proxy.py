# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Controller → Rust-node mesh/Connect passthrough tests.

The Rust node exposes its own opt-in mesh state under `/api/v1/connect/*`. The
controller proxies the node-scoped routes, sends the configured node bearer
token, preserves typed ``supported:false`` 501 documents, and leaves both the
global Connect cloud client and Python soft-node behavior untouched.
"""

import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import pytest

from api import build_app
from auth import AuthConfig
from scenarios import ScenarioManager
from state import AppState, NodeInfo
from stream import StreamManager

pytestmark = pytest.mark.unit

NODE_ID = "fake-mesh-node._ozma._udp.local."
TOKEN = "mesh-node-token"


class _FakeNodeHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def _handle(self, method: str) -> None:
        length = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(length) if length else b""
        record = {
            "method": method,
            "path": self.path,
            "headers": {k.lower(): v for k, v in self.headers.items()},
            "body": json.loads(raw) if raw else None,
        }
        self.server.requests.append(record)
        status, body, content_type = self.server.responder(method, record)
        payload = json.dumps(body).encode() if isinstance(body, (dict, list)) else body
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def do_GET(self) -> None:  # noqa: N802 — http.server API
        self._handle("GET")

    def log_message(self, *args) -> None:
        pass


@pytest.fixture
def fake_node():
    server = ThreadingHTTPServer(("127.0.0.1", 0), _FakeNodeHandler)
    server.requests = []
    server.responder = lambda _method, _record: (200, {"ok": True}, "application/json")
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


def _node(port: int, *, api_token: str = "") -> NodeInfo:
    return NodeInfo(
        id=NODE_ID,
        host="127.0.0.1",
        port=7331,
        role="compute",
        hw="x86_64",
        fw_version="0.1.0",
        proto_version=1,
        capabilities=["hid", "video", "audio"],
        stream_port=port,
        stream_path="/stream/stream.m3u8",
        api_port=port,
        api_token=api_token,
    )


def _build_app(port: int, tmp_path: Path, *, global_token: str = ""):
    state = AppState()
    state.node_api_token = global_token
    node = _node(port)
    state.nodes[NODE_ID] = node
    streams = StreamManager(state, static_dir=tmp_path / "streams")
    streams.register_node(node)
    scenarios = ScenarioManager(tmp_path / "scenarios.json", state)
    app = build_app(
        state,
        scenarios,
        streams=streams,
        auth_config=AuthConfig(enabled=False),
        node_api_token=global_token,
    )
    return app, state


def _client(app):
    from fastapi.testclient import TestClient

    return TestClient(app)


def _mesh_status() -> dict:
    return {
        "ok": True,
        "capability": "mesh",
        "supported": True,
        "enabled": True,
        "state": "running",
        "available": True,
        "wireguard": "userspace-boringtun",
        "identity": {
            "id": "rust-node",
            "mesh_ip": "10.200.9.1",
            "wg_port": 51820,
            "wg_pubkey": "pubkey",
            "wireguard": "userspace-boringtun",
        },
        "peer_count": 1,
        "peers": [
            {
                "id": "peer-a",
                "mesh_ip": "10.200.9.2",
                "wg_port": 51820,
                "wg_pubkey": "peer-pubkey",
                "tunnel": "established",
                "established": True,
                "tx_bytes": 10,
                "rx_bytes": 20,
                "loss": 0.0,
            }
        ],
        "event_count": 2,
        "event_capacity": 64,
        "capabilities": [
            "/api/v1/connect/status",
            "/api/v1/connect/peers",
            "/api/v1/connect/events",
        ],
    }


@pytest.mark.parametrize("surface", ["status", "peers", "events"])
def test_connect_proxy_forwards_token_and_node_document(fake_node, tmp_path, surface):
    body = _mesh_status() if surface == "status" else {"ok": True, "state": "running"}
    fake_node.responder = lambda _method, _record: (200, body, "application/json")
    app, _ = _build_app(fake_node.server_address[1], tmp_path, global_token=TOKEN)

    response = _client(app).get(f"/api/v1/nodes/{NODE_ID}/connect/{surface}")

    assert response.status_code == 200, response.text
    assert response.json() == body
    record = fake_node.requests[-1]
    assert record["path"] == f"/api/v1/connect/{surface}"
    assert record["headers"]["authorization"] == f"Bearer {TOKEN}"


def test_connect_status_advertises_capabilities(fake_node, tmp_path):
    fake_node.responder = lambda _method, _record: (
        200,
        _mesh_status(),
        "application/json",
    )
    app, _ = _build_app(fake_node.server_address[1], tmp_path, global_token=TOKEN)

    response = _client(app).get(f"/api/v1/nodes/{NODE_ID}/connect/status")

    assert response.status_code == 200, response.text
    payload = response.json()
    assert payload["capability"] == "mesh"
    assert payload["state"] == "running"
    assert payload["capabilities"] == [
        "/api/v1/connect/status",
        "/api/v1/connect/peers",
        "/api/v1/connect/events",
    ]
    assert payload["peer_count"] == 1


@pytest.mark.parametrize(
    "code,reason",
    [
        ("mesh.key-missing", "no readable WireGuard private-key file"),
        ("mesh.root-required", "kernel WireGuard requires root"),
        ("mesh.wg-missing", "kernel WireGuard requires the wg binary"),
    ],
)
def test_connect_proxy_preserves_typed_unsupported(fake_node, tmp_path, code, reason):
    typed = {
        "ok": False,
        "code": code,
        "surface": "/api/v1/connect/status",
        "capability": "mesh",
        "supported": False,
        "reason": reason,
        "error": reason,
        "remediation": "configure a mesh key or leave mesh disabled",
    }
    fake_node.responder = lambda _method, _record: (501, typed, "application/json")
    app, _ = _build_app(fake_node.server_address[1], tmp_path, global_token=TOKEN)

    response = _client(app).get(f"/api/v1/nodes/{NODE_ID}/connect/status")

    assert response.status_code == 501, response.text
    assert response.json() == typed
    assert response.headers["content-type"].startswith("application/json")
    record = fake_node.requests[-1]
    assert record["headers"]["authorization"] == f"Bearer {TOKEN}"


def test_connect_proxy_leaves_python_soft_node_404_unchanged(fake_node, tmp_path):
    fake_node.responder = lambda _method, _record: (
        404,
        {"detail": "Not Found"},
        "application/json",
    )
    app, _ = _build_app(fake_node.server_address[1], tmp_path, global_token=TOKEN)

    response = _client(app).get(f"/api/v1/nodes/{NODE_ID}/connect/status")

    assert response.status_code == 404
    assert "mesh/Connect" in response.json()["detail"]


def test_connect_proxy_maps_node_auth_failure_without_leaking_token(fake_node, tmp_path):
    fake_node.responder = lambda _method, _record: (
        401,
        {"error": "Authentication required"},
        "application/json",
    )
    app, _ = _build_app(fake_node.server_address[1], tmp_path, global_token=TOKEN)

    response = _client(app).get(f"/api/v1/nodes/{NODE_ID}/connect/status")

    assert response.status_code == 502
    assert TOKEN not in response.text


def test_global_connect_cloud_status_is_not_replaced_by_node_proxy(fake_node, tmp_path):
    fake_node.responder = lambda _method, _record: (
        200,
        _mesh_status(),
        "application/json",
    )
    app, _ = _build_app(fake_node.server_address[1], tmp_path, global_token=TOKEN)

    response = _client(app).get("/api/v1/connect/status")

    assert response.status_code == 200, response.text
    payload = response.json()
    assert payload["authenticated"] is False
    assert payload["tier"] == "free"
    assert "state" not in payload
    assert not fake_node.requests
