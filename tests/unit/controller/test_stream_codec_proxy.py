# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Controller `/api/v1/streams/{id}/codec` proxy tests.

A remote (`hls-remote`) node owns its encoder, so the controller must proxy
codec reads and switches to the node's `/codec` route instead of reporting
controller-local probe data or returning 404. These tests use a real local
HTTP server as the fake node; no QEMU or Rust node is required.
"""

import json
import socket
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import pytest

from api import build_app
from auth import AuthConfig
from codec_manager import CodecConfig
from scenarios import ScenarioManager
from state import AppState, NodeInfo
from stream import StreamManager

pytestmark = pytest.mark.unit

NODE_ID = "fake-remote._ozma._udp.local."


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
        status, body = self.server.responder(method, record)
        payload = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def do_GET(self) -> None:  # noqa: N802 — http.server API
        self._handle("GET")

    def do_POST(self) -> None:  # noqa: N802 — http.server API
        self._handle("POST")

    def log_message(self, *args) -> None:
        pass


@pytest.fixture
def fake_node():
    server = ThreadingHTTPServer(("127.0.0.1", 0), _FakeNodeHandler)
    server.requests = []
    server.responder = lambda method, record: (200, {})
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


def _unused_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def _build_app(port: int, tmp_path: Path, *, auth_enabled: bool = False):
    state = AppState()
    node = NodeInfo(
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
    )
    state.nodes[NODE_ID] = node
    streams = StreamManager(state, static_dir=tmp_path / "streams")
    streams.register_node(node)
    scenarios = ScenarioManager(tmp_path / "scenarios.json", state)
    app = build_app(
        state,
        scenarios,
        streams=streams,
        auth_config=AuthConfig(enabled=auth_enabled),
    )
    return app


def _client(app):
    from fastapi.testclient import TestClient

    return TestClient(app)


def test_get_codec_proxies_node_status_body_and_headers(fake_node, tmp_path):
    node_body = {
        "current_encoder": "h264_vaapi",
        "available": ["h264", "h265"],
        "override": {"codec": "h264"},
        "receipt": {"selected": {"codec": "h264"}},
        "pending_restart": False,
        "applied": True,
    }
    fake_node.responder = lambda method, record: (200, node_body)

    response = _client(_build_app(fake_node.server_address[1], tmp_path)).get(
        f"/api/v1/streams/{NODE_ID}/codec"
    )

    assert response.status_code == 200
    assert response.json() == node_body
    assert response.headers["content-type"].startswith("application/json")
    assert len(fake_node.requests) == 1
    assert fake_node.requests[0]["method"] == "GET"
    assert fake_node.requests[0]["path"] == "/codec"
    assert fake_node.requests[0]["body"] is None


def test_post_codec_forwards_config_and_returns_node_result(fake_node, tmp_path):
    node_result = {
        "ok": True,
        "applied": True,
        "pending_restart": False,
        "encoder": "h264_vaapi",
        "requested_encoder": "h264_vaapi",
        "backend": "vaapi",
        "hls": "running",
    }
    fake_node.responder = lambda method, record: (200, node_result)

    response = _client(_build_app(fake_node.server_address[1], tmp_path)).post(
        f"/api/v1/streams/{NODE_ID}/codec",
        json={"codec": "h264", "hw_accel": "vaapi", "bitrate": "4M"},
    )

    assert response.status_code == 200
    assert response.json() == node_result
    assert len(fake_node.requests) == 1
    record = fake_node.requests[0]
    assert record["method"] == "POST"
    assert record["path"] == "/codec"
    assert record["headers"]["content-type"] == "application/json"
    assert record["body"] == CodecConfig(
        codec="h264", hw_accel="vaapi", bitrate="4M"
    ).to_dict()


@pytest.mark.parametrize("status", [400, 502])
def test_post_codec_propagates_typed_node_error(fake_node, tmp_path, status):
    node_error = {
        "ok": False,
        "code": "encoder-unavailable" if status == 502 else "invalid-codec",
        "error": "node rejected the codec request",
    }
    fake_node.responder = lambda method, record: (status, node_error)

    response = _client(_build_app(fake_node.server_address[1], tmp_path)).post(
        f"/api/v1/streams/{NODE_ID}/codec", json={"codec": "h264"}
    )

    assert response.status_code == status
    assert response.json() == node_error


def test_get_codec_propagates_node_error(fake_node, tmp_path):
    node_error = {"ok": False, "code": "internal", "error": "codec state unavailable"}
    fake_node.responder = lambda method, record: (502, node_error)

    response = _client(_build_app(fake_node.server_address[1], tmp_path)).get(
        f"/api/v1/streams/{NODE_ID}/codec"
    )

    assert response.status_code == 502
    assert response.json() == node_error


@pytest.mark.parametrize("method", ["get", "post"])
def test_codec_proxy_reports_unreachable_node(tmp_path, method):
    app = _build_app(_unused_port(), tmp_path)
    client = _client(app)
    url = f"/api/v1/streams/{NODE_ID}/codec"

    if method == "get":
        response = client.get(url)
    else:
        response = client.post(url, json={"codec": "h264"})

    assert response.status_code == 502
    assert "Node unreachable" in response.json()["detail"]


def test_codec_proxy_reports_node_timeout(monkeypatch, tmp_path):
    import httpx

    async def _timeout(self, *args, **kwargs):
        raise httpx.ReadTimeout("fake timeout")

    monkeypatch.setattr(httpx.AsyncClient, "get", _timeout)

    # No live server is needed: the timeout is raised before connecting.
    app = _build_app(_unused_port(), tmp_path)
    response = _client(app).get(f"/api/v1/streams/{NODE_ID}/codec")

    assert response.status_code == 502
    assert "timed out" in response.json()["detail"]


def test_codec_proxy_preserves_controller_auth(fake_node, tmp_path):
    fake_node.responder = lambda method, record: (200, {"applied": True})
    app = _build_app(fake_node.server_address[1], tmp_path, auth_enabled=True)

    response = _client(app).post(
        f"/api/v1/streams/{NODE_ID}/codec", json={"codec": "h264"}
    )

    assert response.status_code == 401
    assert fake_node.requests == []


def test_non_remote_stream_keeps_controller_local_codec(tmp_path):
    # No capture entry (and therefore no node API endpoint) keeps the old
    # controller-local response shape for non-remote streams.
    state = AppState()
    streams = StreamManager(state, static_dir=tmp_path / "streams")
    scenarios = ScenarioManager(tmp_path / "scenarios.json", state)
    app = build_app(state, scenarios, streams=streams)

    response = _client(app).get("/api/v1/streams/unknown-node/codec")

    assert response.status_code == 200
    data = response.json()
    assert data["node_id"] == "unknown-node"
    assert data["current_encoder"] == ""
    assert data["stats"] == {}
    assert "available_encoders" in data
