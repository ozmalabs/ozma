# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Controller → node bearer-token propagation tests.

The controller proxies snapshot, hints, metrics, power, codec, WebRTC and input
calls to a node's HTTP API. When that node runs with `--api-token`, the proxy
must send `Authorization: Bearer <token>`; without a configured token it must
keep the previous unauthenticated behavior. These tests use a real local HTTP
server as the fake node and assert the header, 401 handling, and that the token
never leaks into responses or logs.
"""

import json
import logging
import socket
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import pytest

from api import build_app
from auth import AuthConfig
from config import Config
from device_metrics import MetricSource, MetricsCollector
from scenarios import ScenarioManager
from state import AppState, NodeInfo
from stream import StreamManager

pytestmark = pytest.mark.unit

NODE_ID = "fake-token-node._ozma._udp.local."
TOKEN = "test-node-secret-token"
JPEG = b"\xff\xd8\xff\xe0fake-jpeg\xff\xd9"


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
        if isinstance(body, (dict, list)):
            payload = json.dumps(body).encode()
        else:
            payload = body
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def do_GET(self) -> None:  # noqa: N802 — http.server API
        self._handle("GET")

    def do_POST(self) -> None:  # noqa: N802 — http.server API
        self._handle("POST")

    def do_DELETE(self) -> None:  # noqa: N802 — http.server API
        self._handle("DELETE")

    def log_message(self, *args) -> None:
        pass


def _default_responder(method: str, record: dict):
    path = record["path"]
    if path.startswith("/display/snapshot"):
        return 200, JPEG, "image/jpeg"
    if path.startswith("/webrtc/offer"):
        return 200, {"type": "answer", "sdp": "v=0"}, "application/json"
    if path.startswith("/agent/metrics"):
        return 200, {"cpu_usage": 1.0, "uptime": 2.0}, "application/json"
    return 200, {"ok": True}, "application/json"


@pytest.fixture
def fake_node():
    server = ThreadingHTTPServer(("127.0.0.1", 0), _FakeNodeHandler)
    server.requests = []
    server.responder = _default_responder
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
        capabilities=["hid", "video", "audio", "power"],
        stream_port=port,
        stream_path="/stream/stream.m3u8",
        api_port=port,
        api_token=api_token,
    )


def _build_app(
    port: int,
    tmp_path: Path,
    *,
    global_token: str = "",
    node_token: str = "",
    auth_enabled: bool = False,
):
    state = AppState()
    state.node_api_token = global_token
    node = _node(port, api_token=node_token)
    state.nodes[NODE_ID] = node
    streams = StreamManager(state, static_dir=tmp_path / "streams")
    streams.register_node(node)
    scenarios = ScenarioManager(tmp_path / "scenarios.json", state)
    app = build_app(
        state,
        scenarios,
        streams=streams,
        auth_config=AuthConfig(enabled=auth_enabled),
        node_api_token=global_token,
    )
    return app, state


def _client(app):
    from fastapi.testclient import TestClient

    return TestClient(app)


# (method, controller URL, request JSON, expected node path prefix)
PROXY_CASES = [
    ("post", f"/api/v1/nodes/{NODE_ID}/proxy/input/key", {"key": "a"}, "/input/key"),
    (
        "post",
        "/api/v1/audio/room-correction/apply",
        {"node_id": NODE_ID, "profile_id": "p1"},
        "/audio/apply",
    ),
    (
        "post",
        f"/api/v1/streams/{NODE_ID}/webrtc",
        {"type": "offer", "sdp": "v=0"},
        "/webrtc/offer",
    ),
    ("get", f"/api/v1/streams/{NODE_ID}/snapshot", None, "/display/snapshot"),
    ("get", f"/api/v1/nodes/{NODE_ID}/ui/hints", None, "/ui/hints"),
    ("get", f"/api/v1/nodes/{NODE_ID}/power/state", None, "/power/state"),
    ("post", f"/api/v1/nodes/{NODE_ID}/power/pause", {}, "/power/pause"),
    ("post", f"/api/v1/nodes/{NODE_ID}/power/resume", {}, "/power/resume"),
    ("get", f"/api/v1/nodes/{NODE_ID}/display", None, "/api/v1/connection"),
    (
        "post",
        f"/api/v1/nodes/{NODE_ID}/display/select",
        {"index": 1},
        "/display/select",
    ),
    ("get", f"/api/v1/nodes/{NODE_ID}/usb", None, "/usb"),
    ("get", f"/api/v1/streams/{NODE_ID}/codec", None, "/codec"),
    ("get", f"/api/v1/remote/{NODE_ID}/screenshot", None, "/display/snapshot"),
    ("get", f"/api/v1/nodes/{NODE_ID}/hardware", None, "/hardware"),
    ("get", f"/api/v1/nodes/{NODE_ID}/hardware/summary", None, "/hardware/summary"),
    ("get", f"/api/v1/nodes/{NODE_ID}/hardware/sensors", None, "/hardware/sensors"),
    ("get", f"/api/v1/nodes/{NODE_ID}/storage", None, "/storage"),
    ("get", f"/api/v1/nodes/{NODE_ID}/apps", None, "/apps"),
    ("get", f"/api/v1/nodes/{NODE_ID}/self", None, "/self"),
    ("get", f"/api/v1/nodes/{NODE_ID}/bluetooth/status", None, "/bluetooth/status"),
    (
        "get",
        f"/api/v1/nodes/{NODE_ID}/bluetooth/devices?limit=5",
        None,
        "/bluetooth/devices",
    ),
    (
        "post",
        f"/api/v1/nodes/{NODE_ID}/bluetooth/scan",
        {"duration_seconds": 3},
        "/bluetooth/scan",
    ),
    (
        "post",
        f"/api/v1/nodes/{NODE_ID}/bluetooth/pair",
        {"address": "AA:BB:CC:DD:EE:FF"},
        "/bluetooth/pair",
    ),
    (
        "post",
        f"/api/v1/nodes/{NODE_ID}/bluetooth/connect",
        {"address": "AA:BB:CC:DD:EE:FF"},
        "/bluetooth/connect",
    ),
    (
        "post",
        f"/api/v1/nodes/{NODE_ID}/bluetooth/disconnect",
        {"address": "AA:BB:CC:DD:EE:FF"},
        "/bluetooth/disconnect",
    ),
    (
        "post",
        f"/api/v1/nodes/{NODE_ID}/bluetooth/forget",
        {"address": "AA:BB:CC:DD:EE:FF"},
        "/bluetooth/forget",
    ),
    ("get", f"/api/v1/audio/nodes?node_id={NODE_ID}", None, "/audio/nodes"),
    (
        "post",
        "/api/v1/audio/room-correction/sweep",
        {"node_id": NODE_ID, "source": "ozma-vm1.monitor", "sink": "ozma-vm1"},
        "/audio/sweep",
    ),
]


@pytest.mark.parametrize("method,url,body,node_path", PROXY_CASES)
def test_proxy_sends_configured_bearer_token(
    fake_node, tmp_path, method, url, body, node_path
):
    app, _ = _build_app(fake_node.server_address[1], tmp_path, global_token=TOKEN)
    client = _client(app)

    response = client.get(url) if method == "get" else client.post(url, json=body)

    assert response.status_code == 200, response.text
    assert fake_node.requests, f"{url} did not reach the node"
    record = fake_node.requests[-1]
    assert record["path"].startswith(node_path)
    assert record["headers"]["authorization"] == f"Bearer {TOKEN}"


def test_apps_proxy_forwards_limit_to_the_node(fake_node, tmp_path):
    app, _ = _build_app(fake_node.server_address[1], tmp_path, global_token=TOKEN)
    client = _client(app)

    response = client.get(f"/api/v1/nodes/{NODE_ID}/apps?limit=7")

    assert response.status_code == 200, response.text
    record = fake_node.requests[-1]
    assert record["path"].startswith("/apps?limit=7")
    assert record["headers"]["authorization"] == f"Bearer {TOKEN}"

    client.get(f"/api/v1/nodes/{NODE_ID}/apps?limit=1000")
    assert fake_node.requests[-1]["path"].startswith("/apps?limit=100")


def _typed_unsupported(capability: str, surface: str) -> dict:
    return {
        "ok": False,
        "code": f"{capability}.unsupported",
        "surface": surface,
        "capability": capability,
        "supported": False,
        "reason": f"{capability} is not implemented on this node",
        "error": f"{capability} is not implemented on this node",
        "python_reference": "node/provider.py",
        "remediation": "use the Python node or a real provider",
    }


SURFACE_PROXY_CASES = [
    (
        "get",
        f"/api/v1/nodes/{NODE_ID}/rpa/screen",
        None,
        "/api/v1/rpa/screen",
        "rpa",
        "/api/v1/rpa/*",
    ),
    (
        "get",
        f"/api/v1/nodes/{NODE_ID}/rpa/screenshot",
        None,
        "/api/v1/rpa/screenshot",
        "rpa",
        "/api/v1/rpa/*",
    ),
    (
        "post",
        f"/api/v1/nodes/{NODE_ID}/rpa/key",
        {"key": "enter"},
        "/api/v1/rpa/key",
        "rpa",
        "/api/v1/rpa/*",
    ),
    (
        "get",
        f"/api/v1/nodes/{NODE_ID}/current",
        None,
        "/current",
        "current",
        "/current",
    ),
    (
        "get",
        f"/api/v1/nodes/{NODE_ID}/rgb/state",
        None,
        "/rgb/state",
        "rgb",
        "/rgb/*",
    ),
    (
        "post",
        f"/api/v1/nodes/{NODE_ID}/rgb/set",
        {"color": [1, 2, 3]},
        "/rgb/set",
        "rgb",
        "/rgb/*",
    ),
    (
        "get",
        f"/api/v1/backup/nodes/{NODE_ID}/snapshots",
        None,
        "/backup/snapshots",
        "backup",
        "/backup/*",
    ),
    (
        "post",
        f"/api/v1/backup/nodes/{NODE_ID}/run",
        {"mode": "smart"},
        "/backup/run",
        "backup",
        "/backup/*",
    ),
    (
        "get",
        f"/api/v1/nodes/{NODE_ID}/ui/hints",
        None,
        "/ui/hints",
        "ui_windows",
        "/ui/windows",
    ),
]


@pytest.mark.parametrize(
    "method,url,body,node_path,capability,surface", SURFACE_PROXY_CASES
)
def test_controller_only_surface_proxies_preserve_typed_unsupported(
    fake_node, tmp_path, method, url, body, node_path, capability, surface
):
    expected = _typed_unsupported(capability, surface)
    fake_node.responder = lambda _method, _record: (
        501,
        expected,
        "application/json",
    )
    app, _ = _build_app(fake_node.server_address[1], tmp_path, global_token=TOKEN)
    client = _client(app)

    response = client.get(url) if method == "get" else client.post(url, json=body)

    assert response.status_code == 501, response.text
    assert response.json() == expected
    assert response.headers["content-type"].startswith("application/json")
    record = fake_node.requests[-1]
    assert record["path"].startswith(node_path)
    assert record["headers"]["authorization"] == f"Bearer {TOKEN}"


@pytest.mark.parametrize(
    "url,node_path,code",
    [
        (f"/api/v1/nodes/{NODE_ID}/storage", "/storage", "storage.proc-unavailable"),
        (f"/api/v1/nodes/{NODE_ID}/apps", "/apps", "apps.proc-unavailable"),
        (f"/api/v1/nodes/{NODE_ID}/self", "/self", "self.proc-unavailable"),
    ],
)
def test_host_surface_proxies_preserve_typed_errors(
    fake_node, tmp_path, url, node_path, code
):
    expected = {
        "ok": False,
        "available": False,
        "code": code,
        "error": "procfs is not readable",
    }
    fake_node.responder = lambda _method, _record: (
        503,
        expected,
        "application/json",
    )
    app, _ = _build_app(fake_node.server_address[1], tmp_path, global_token=TOKEN)

    response = _client(app).get(url)

    assert response.status_code == 503, response.text
    assert response.json() == expected
    record = fake_node.requests[-1]
    assert record["path"].startswith(node_path)
    assert record["headers"]["authorization"] == f"Bearer {TOKEN}"


def test_rpa_screenshot_still_returns_real_jpeg_bytes(fake_node, tmp_path):
    fake_node.responder = lambda _method, _record: (200, JPEG, "image/jpeg")
    app, _ = _build_app(fake_node.server_address[1], tmp_path, global_token=TOKEN)

    response = _client(app).get(f"/api/v1/nodes/{NODE_ID}/rpa/screenshot")

    assert response.status_code == 200, response.text
    assert response.headers["content-type"].startswith("image/jpeg")
    assert response.content == JPEG
    record = fake_node.requests[-1]
    assert record["path"] == "/api/v1/rpa/screenshot"
    assert record["headers"]["authorization"] == f"Bearer {TOKEN}"


def test_display_read_returns_node_outputs_and_uses_the_bearer_token(
    fake_node, tmp_path
):
    fake_node.responder = lambda method, record: (
        200,
        {
            "ok": True,
            "display_outputs": [
                {"index": 0, "name": "Console_0", "capture_source_id": "dbus0"},
                {"index": 1, "name": "Console_1", "capture_source_id": ""},
            ],
            "display_capture": {
                "active_console": 0,
                "provider": "dbus",
                "limitation": "only Console_0 has a live capture source",
            },
        },
        "application/json",
    )
    app, _ = _build_app(fake_node.server_address[1], tmp_path, global_token=TOKEN)

    response = _client(app).get(f"/api/v1/nodes/{NODE_ID}/display")

    assert response.status_code == 200, response.text
    assert response.json()["display_capture"]["active_console"] == 0
    assert response.json()["display_outputs"][1]["capture_source_id"] == ""
    record = fake_node.requests[-1]
    assert record["path"] == "/api/v1/connection"
    assert record["headers"]["authorization"] == f"Bearer {TOKEN}"


def test_display_select_updates_cached_outputs_after_node_success(fake_node, tmp_path):
    switched = [
        {"index": 0, "name": "Console_0", "capture_source_id": ""},
        {"index": 1, "name": "Console_1", "capture_source_id": "dbus0"},
    ]
    fake_node.responder = lambda method, record: (
        200,
        {
            "ok": True,
            "active_console": 1,
            "previous_console": 0,
            "display_outputs": switched,
            "display_capture": {"active_console": 1, "provider": "dbus"},
            "changed": True,
            "restarted": {"hls": True, "webrtc": False},
        },
        "application/json",
    )
    app, state = _build_app(fake_node.server_address[1], tmp_path, global_token=TOKEN)

    response = _client(app).post(
        f"/api/v1/nodes/{NODE_ID}/display/select", json={"index": 1}
    )

    assert response.status_code == 200, response.text
    assert response.json()["active_console"] == 1
    assert state.nodes[NODE_ID].display_outputs == switched
    record = fake_node.requests[-1]
    assert record["method"] == "POST"
    assert record["path"] == "/display/select"
    assert record["body"] == {"index": 1}
    assert record["headers"]["authorization"] == f"Bearer {TOKEN}"


def test_snapshot_proxy_forwards_a_console_query(fake_node, tmp_path):
    app, _ = _build_app(fake_node.server_address[1], tmp_path, global_token=TOKEN)

    response = _client(app).get(
        f"/api/v1/streams/{NODE_ID}/snapshot", params={"console": "Console_1"}
    )

    assert response.status_code == 200, response.text
    assert response.headers["content-type"] == "image/jpeg"
    record = fake_node.requests[-1]
    assert record["path"] == "/display/snapshot?console=Console_1"
    assert record["headers"]["authorization"] == f"Bearer {TOKEN}"


def test_display_info_proxy_forwards_a_console_query(fake_node, tmp_path):
    fake_node.responder = lambda method, record: (
        200,
        {
            "ok": True,
            "console": 1,
            "active": False,
            "source_id": "dbus0",
            "width": 1280,
            "height": 1024,
            "snapshot_available": True,
        },
        "application/json",
    )
    app, _ = _build_app(fake_node.server_address[1], tmp_path, global_token=TOKEN)

    response = _client(app).get(
        f"/api/v1/nodes/{NODE_ID}/display/info", params={"console": "1"}
    )

    assert response.status_code == 200, response.text
    assert response.json()["console"] == 1
    record = fake_node.requests[-1]
    assert record["path"] == "/display/info?console=1"
    assert record["headers"]["authorization"] == f"Bearer {TOKEN}"


def test_display_select_preserves_single_head_409_without_updating_state(
    fake_node, tmp_path
):
    original = [
        {"index": 0, "name": "Console_0", "capture_source_id": "vnc0"},
        {"index": 1, "name": "Console_1", "capture_source_id": ""},
    ]
    fake_node.responder = lambda method, record: (
        409,
        {
            "ok": False,
            "code": "conflict",
            "error": "the vnc capture provider exposes one default console",
        },
        "application/json",
    )
    app, state = _build_app(fake_node.server_address[1], tmp_path, global_token=TOKEN)
    state.nodes[NODE_ID].display_outputs = original

    response = _client(app).post(
        f"/api/v1/nodes/{NODE_ID}/display/select", json={"index": 1}
    )

    assert response.status_code == 409
    assert response.json()["code"] == "conflict"
    assert state.nodes[NODE_ID].display_outputs == original


@pytest.mark.parametrize("action", ["pause", "resume"])
def test_power_pause_and_resume_actions_are_propagated(fake_node, tmp_path, action):
    app, _ = _build_app(fake_node.server_address[1], tmp_path, global_token=TOKEN)
    client = _client(app)

    response = client.post(f"/api/v1/nodes/{NODE_ID}/power/{action}", json={})

    assert response.status_code == 200, response.text
    assert response.json() == {"ok": True}
    record = fake_node.requests[-1]
    assert record["method"] == "POST"
    assert record["path"] == f"/power/{action}"
    assert record["headers"]["authorization"] == f"Bearer {TOKEN}"


@pytest.mark.parametrize("method,url,body,node_path", PROXY_CASES)
def test_proxy_omits_authorization_when_no_token_configured(
    fake_node, tmp_path, method, url, body, node_path
):
    app, _ = _build_app(fake_node.server_address[1], tmp_path)
    client = _client(app)

    response = client.get(url) if method == "get" else client.post(url, json=body)

    assert response.status_code == 200, response.text
    record = fake_node.requests[-1]
    assert "authorization" not in record["headers"]


def test_per_node_registration_token_wins_over_global(fake_node, tmp_path):
    app, _ = _build_app(
        fake_node.server_address[1],
        tmp_path,
        global_token="global-token",
        node_token="per-node-token",
    )

    response = _client(app).get(f"/api/v1/nodes/{NODE_ID}/power/state")

    assert response.status_code == 200, response.text
    assert fake_node.requests[-1]["headers"]["authorization"] == "Bearer per-node-token"


def test_registration_accepts_token_without_serializing_it(fake_node, tmp_path):
    app, state = _build_app(
        fake_node.server_address[1], tmp_path, global_token="global-token"
    )
    client = _client(app)

    registered = client.post(
        "/api/v1/nodes/register",
        json={
            "id": NODE_ID,
            "host": "127.0.0.1",
            "api_port": str(fake_node.server_address[1]),
            "stream_port": str(fake_node.server_address[1]),
            "cap": "hid,video,audio,power",
            "api_token": "registered-node-token",
        },
    )

    assert registered.status_code == 200, registered.text
    assert "registered-node-token" not in registered.text
    assert "api_token" not in registered.json()["node"]
    assert state.nodes[NODE_ID].api_token == "registered-node-token"

    listed = client.get("/api/v1/nodes")
    assert listed.status_code == 200
    assert "registered-node-token" not in listed.text
    assert "api_token" not in listed.text

    proxied = client.get(f"/api/v1/nodes/{NODE_ID}/power/state")
    assert proxied.status_code == 200, proxied.text
    assert (
        fake_node.requests[-1]["headers"]["authorization"]
        == "Bearer registered-node-token"
    )


def test_webrtc_raw_proxy_passes_node_401_through(fake_node, tmp_path):
    fake_node.responder = lambda method, record: (
        401,
        {"error": "unauthorized"},
        "application/json",
    )
    app, _ = _build_app(fake_node.server_address[1], tmp_path, global_token=TOKEN)

    response = _client(app).post(
        f"/api/v1/streams/{NODE_ID}/webrtc", json={"type": "offer", "sdp": "v=0"}
    )

    assert response.status_code == 401
    assert response.json() == {"error": "unauthorized"}
    assert TOKEN not in response.text


def test_codec_proxy_passes_node_401_through(fake_node, tmp_path):
    fake_node.responder = lambda method, record: (
        401,
        {"ok": False, "error": "unauthorized"},
        "application/json",
    )
    app, _ = _build_app(fake_node.server_address[1], tmp_path, global_token=TOKEN)

    response = _client(app).get(f"/api/v1/streams/{NODE_ID}/codec")

    assert response.status_code == 401
    assert response.json() == {"ok": False, "error": "unauthorized"}
    assert TOKEN not in response.text


@pytest.mark.parametrize(
    "url",
    [
        f"/api/v1/nodes/{NODE_ID}/power/state",
        f"/api/v1/streams/{NODE_ID}/snapshot",
        f"/api/v1/nodes/{NODE_ID}/ui/hints",
    ],
)
def test_node_401_maps_to_clear_token_free_controller_error(fake_node, tmp_path, url):
    fake_node.responder = lambda method, record: (
        401,
        {"error": "unauthorized"},
        "application/json",
    )
    app, _ = _build_app(fake_node.server_address[1], tmp_path, global_token=TOKEN)

    response = _client(app).get(url)

    assert response.status_code == 502
    detail = response.json()["detail"]
    assert "401" in detail
    assert "bearer token" in detail.lower()
    assert TOKEN not in response.text


def test_json_input_proxy_maps_node_401_to_clear_error(fake_node, tmp_path):
    fake_node.responder = lambda method, record: (
        401,
        {"error": "unauthorized"},
        "application/json",
    )
    app, _ = _build_app(fake_node.server_address[1], tmp_path, global_token=TOKEN)

    response = _client(app).post(
        f"/api/v1/nodes/{NODE_ID}/proxy/input/key", json={"key": "a"}
    )

    assert response.status_code == 502
    assert "401" in response.json()["detail"]
    assert TOKEN not in response.text


def test_token_never_appears_in_responses_or_logs(fake_node, tmp_path, caplog):
    caplog.set_level(logging.DEBUG)
    app, _ = _build_app(fake_node.server_address[1], tmp_path, global_token=TOKEN)
    client = _client(app)

    responses = [
        client.get(f"/api/v1/streams/{NODE_ID}/snapshot"),
        client.get(f"/api/v1/nodes/{NODE_ID}/ui/hints"),
        client.get(f"/api/v1/nodes/{NODE_ID}/power/state"),
        client.post(
            f"/api/v1/streams/{NODE_ID}/webrtc", json={"type": "offer", "sdp": "v=0"}
        ),
    ]

    for response in responses:
        assert TOKEN not in response.text
    assert TOKEN not in caplog.text


async def test_metrics_poller_sends_global_token(fake_node):
    state = AppState()
    state.node_api_token = TOKEN
    node = _node(fake_node.server_address[1])
    collector = MetricsCollector(state)
    source = MetricSource(id=NODE_ID, name="fake")

    assert await collector._fetch_agent(node, source) is True
    assert fake_node.requests[-1]["path"] == "/agent/metrics"
    assert fake_node.requests[-1]["headers"]["authorization"] == f"Bearer {TOKEN}"


async def test_metrics_poller_omits_header_without_token(fake_node):
    state = AppState()
    node = _node(fake_node.server_address[1])
    collector = MetricsCollector(state)
    source = MetricSource(id=NODE_ID, name="fake")

    assert await collector._fetch_agent(node, source) is True
    assert "authorization" not in fake_node.requests[-1]["headers"]


def test_config_reads_node_api_token_from_env(monkeypatch):
    monkeypatch.setenv("OZMA_NODE_API_TOKEN", TOKEN)
    assert Config.from_env().node_api_token == TOKEN


def test_node_info_to_dict_excludes_api_token():
    node = _node(_unused_port(), api_token=TOKEN)
    assert "api_token" not in node.to_dict()
    assert TOKEN not in json.dumps(node.to_dict())


def _unused_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def test_node_display_includes_per_console_hls_status(fake_node, tmp_path):
    fake_node.responder = lambda method, record: (
        200,
        {
            "ok": True,
            "display_outputs": [
                {"index": 0, "name": "Console_0", "source_type": "dbus"},
                {"index": 1, "name": "Console_1", "source_type": "dbus"},
            ],
            "display_capture": {"active_console": 0, "provider": "dbus"},
            "multi_console_hls": {
                "enabled": True,
                "max_pipelines": 2,
                "active_console": 0,
                "consoles": [
                    {"console": 0, "active": True, "status": "running"},
                    {"console": 1, "active": False, "status": "running"},
                ],
            },
        },
        "application/json",
    )
    app, _ = _build_app(fake_node.server_address[1], tmp_path, global_token=TOKEN)

    response = _client(app).get(f"/api/v1/nodes/{NODE_ID}/display")

    assert response.status_code == 200, response.text
    body = response.json()
    assert body["multi_console_hls"]["enabled"] is True
    assert body["multi_console_hls"]["consoles"][1]["console"] == 1


def test_hls_proxy_forwards_a_per_console_playlist_path(fake_node, tmp_path):
    playlist = b"#EXTM3U\nseg_00000.ts\n"

    def responder(method, record):
        if record["path"] == "/stream/console-1/stream.m3u8":
            return 200, playlist, "application/vnd.apple.mpegurl"
        return 404, {"error": "missing"}, "application/json"

    fake_node.responder = responder
    app, _ = _build_app(fake_node.server_address[1], tmp_path)
    client = _client(app)

    response = client.get(
        f"/api/v1/streams/{NODE_ID}/hls/console-1/stream.m3u8"
    )

    assert response.status_code == 200, response.text
    assert response.content == playlist
    assert response.headers["content-type"].startswith("application/vnd.apple.mpegurl")
    assert fake_node.requests[-1]["path"] == "/stream/console-1/stream.m3u8"


def test_software_inventory_ingest_is_exempt_from_controller_auth(fake_node, tmp_path):
    """Node telemetry stays pushable when controller auth is enabled.

    Only the ingest POST is exempt; the fleet read views remain guarded.
    """
    app, _ = _build_app(
        fake_node.server_address[1], tmp_path, auth_enabled=True
    )
    client = _client(app)

    ingest = client.post(
        "/api/v1/software/hosts/node-1._ozma._udp.local./inventory",
        json={"records": [], "reported_at": 1.0},
    )
    assert ingest.status_code == 200, ingest.text

    read = client.get("/api/v1/software/hosts")
    assert read.status_code == 401


def test_backup_status_proxy_preserves_real_json_without_token_leak(fake_node, tmp_path):
    expected = {
        "ok": True,
        "supported": True,
        "configured": True,
        "restic": {"available": True, "program": "/usr/bin/restic"},
        "repo": {"path": "/srv/ozma-backup", "state": "ready"},
        "password": {"configured": True},
        "sources": ["/srv/data"],
        "restore_roots": ["/srv/restore"],
        "last_snapshot": {"id": "a" * 64, "short_id": "a" * 8},
        "snapshot_error": None,
        "last_run": None,
    }
    fake_node.responder = lambda _method, _record: (200, expected, "application/json")
    app, _ = _build_app(fake_node.server_address[1], tmp_path, global_token=TOKEN)

    response = _client(app).get(f"/api/v1/backup/nodes/{NODE_ID}/status")

    assert response.status_code == 200, response.text
    assert response.json() == expected
    assert TOKEN not in response.text
    record = fake_node.requests[-1]
    assert record["path"] == "/backup/status"
    assert record["headers"]["authorization"] == f"Bearer {TOKEN}"


def test_backup_proxy_preserves_typed_errors_without_token_leak(fake_node, tmp_path):
    expected = {
        "ok": False,
        "supported": True,
        "code": "backup.path-not-allowed",
        "error": "the path is outside the configured backup allow-list",
        "remediation": "Only paths below --backup-source roots are allowed.",
    }
    fake_node.responder = lambda _method, _record: (400, expected, "application/json")
    app, _ = _build_app(fake_node.server_address[1], tmp_path, global_token=TOKEN)

    response = _client(app).post(
        f"/api/v1/backup/nodes/{NODE_ID}/run",
        json={"mode": "smart", "paths": ["/etc"], "tags": ["manual"]},
    )

    assert response.status_code == 400, response.text
    assert response.json() == expected
    assert TOKEN not in response.text
    record = fake_node.requests[-1]
    assert record["path"] == "/backup/run"
    assert record["body"] == {
        "mode": "smart",
        "paths": ["/etc"],
        "tags": ["manual"],
    }
    assert record["headers"]["authorization"] == f"Bearer {TOKEN}"


def test_backup_snapshots_proxy_preserves_node_document(fake_node, tmp_path):
    expected = {
        "ok": True,
        "snapshots": [{"id": "b" * 64, "short_id": "b" * 8, "tags": ["manual"]}],
        "count": 1,
        "total": 1,
        "limit": 20,
        "truncated": False,
    }
    fake_node.responder = lambda _method, _record: (200, expected, "application/json")
    app, _ = _build_app(fake_node.server_address[1], tmp_path, global_token=TOKEN)

    response = _client(app).get(
        f"/api/v1/backup/nodes/{NODE_ID}/snapshots"
    )

    assert response.status_code == 200, response.text
    assert response.json() == expected
    assert TOKEN not in response.text
    assert fake_node.requests[-1]["path"] == "/backup/snapshots"


def test_backup_delete_proxy_uses_delete_and_preserves_policy(fake_node, tmp_path):
    snapshot_id = "c" * 64
    expected = {
        "ok": True,
        "deleted": snapshot_id,
        "pruned": True,
        "policy": "exact-snapshot-only",
    }
    fake_node.responder = lambda _method, _record: (200, expected, "application/json")
    app, _ = _build_app(fake_node.server_address[1], tmp_path, global_token=TOKEN)

    response = _client(app).delete(
        f"/api/v1/backup/nodes/{NODE_ID}/snapshots/{snapshot_id}"
    )

    assert response.status_code == 200, response.text
    assert response.json() == expected
    assert TOKEN not in response.text
    record = fake_node.requests[-1]
    assert record["method"] == "DELETE"
    assert record["path"] == f"/backup/snapshots/{snapshot_id}"
    assert record["headers"]["authorization"] == f"Bearer {TOKEN}"


def test_backup_init_proxy_preserves_typed_missing_binary(fake_node, tmp_path):
    expected = {
        "ok": False,
        "supported": True,
        "code": "backup.restic-missing",
        "error": "the restic binary is not available",
        "remediation": "Install restic or point --backup-restic at an executable.",
    }
    fake_node.responder = lambda _method, _record: (503, expected, "application/json")
    app, _ = _build_app(fake_node.server_address[1], tmp_path, global_token=TOKEN)

    response = _client(app).post(f"/api/v1/backup/nodes/{NODE_ID}/init")

    assert response.status_code == 503, response.text
    assert response.json() == expected
    assert TOKEN not in response.text
    assert fake_node.requests[-1]["path"] == "/backup/init"


def test_backup_stats_proxy_preserves_repository_statistics(fake_node, tmp_path):
    expected = {
        "ok": True,
        "supported": True,
        "repo": {"path": "/srv/ozma-backup", "state": "ready"},
        "snapshots": {"count": 2},
        "unique": {"size_bytes": 900, "file_count": 3},
        "restore": {"size_bytes": 1200, "file_count": 4},
        "repository": {
            "size_bytes": 700,
            "uncompressed_size_bytes": 1000,
            "blob_count": 5,
            "compression_ratio": 1.4,
            "compression_space_saving_percent": 30.0,
        },
        "measured_with": "restic-stats",
        "generated_at": 1,
    }
    fake_node.responder = lambda _method, _record: (200, expected, "application/json")
    app, _ = _build_app(fake_node.server_address[1], tmp_path, global_token=TOKEN)

    response = _client(app).get(f"/api/v1/backup/nodes/{NODE_ID}/stats")

    assert response.status_code == 200, response.text
    assert response.json() == expected
    assert TOKEN not in response.text
    record = fake_node.requests[-1]
    assert record["method"] == "GET"
    assert record["path"] == "/backup/stats"
    assert record["headers"]["authorization"] == f"Bearer {TOKEN}"


def test_backup_verify_proxy_forwards_subset_and_preserves_typed_failure(
    fake_node, tmp_path
):
    expected = {
        "ok": False,
        "supported": True,
        "code": "backup.subset-invalid",
        "error": "the read-data subset must be between 1 and 5 percent",
        "remediation": "Send read_data_percent between 1 and 5.",
    }
    fake_node.responder = lambda _method, _record: (400, expected, "application/json")
    app, _ = _build_app(fake_node.server_address[1], tmp_path, global_token=TOKEN)

    response = _client(app).post(
        f"/api/v1/backup/nodes/{NODE_ID}/verify",
        json={"read_data_percent": 9},
    )

    assert response.status_code == 400, response.text
    assert response.json() == expected
    assert TOKEN not in response.text
    record = fake_node.requests[-1]
    assert record["method"] == "POST"
    assert record["path"] == "/backup/verify"
    assert record["body"] == {"read_data_percent": 9}
    assert record["headers"]["authorization"] == f"Bearer {TOKEN}"


def test_backup_estimate_proxy_forwards_paths_and_preserves_typed_unsupported(
    fake_node, tmp_path
):
    expected = {
        "ok": False,
        "supported": False,
        "code": "backup.unsupported",
        "capability": "backup",
        "error": "estimate is not implemented",
    }
    fake_node.responder = lambda _method, _record: (501, expected, "application/json")
    app, _ = _build_app(fake_node.server_address[1], tmp_path, global_token=TOKEN)

    response = _client(app).post(
        f"/api/v1/backup/nodes/{NODE_ID}/estimate",
        json={"paths": ["/srv/data"]},
    )

    assert response.status_code == 501, response.text
    assert response.json() == expected
    assert TOKEN not in response.text
    record = fake_node.requests[-1]
    assert record["method"] == "POST"
    assert record["path"] == "/backup/estimate"
    assert record["body"] == {"paths": ["/srv/data"]}
    assert record["headers"]["authorization"] == f"Bearer {TOKEN}"


def test_bluetooth_status_proxy_preserves_real_json_without_token_leak(
    fake_node, tmp_path
):
    expected = {
        "ok": True,
        "supported": True,
        "configured": True,
        "available": True,
        "program": "/usr/bin/bluetoothctl",
        "adapter": "80:84:89:F6:E2:E8",
        "adapters": [
            {
                "address": "80:84:89:F6:E2:E8",
                "name": "SFFPC",
                "default": True,
                "powered": True,
                "discoverable": False,
                "discovering": False,
                "pairable": True,
            }
        ],
        "discovering": False,
        "scan": {"active": False, "default_seconds": 8, "max_seconds": 15},
    }
    fake_node.responder = lambda _method, _record: (200, expected, "application/json")
    app, _ = _build_app(fake_node.server_address[1], tmp_path, global_token=TOKEN)

    response = _client(app).get(
        f"/api/v1/nodes/{NODE_ID}/bluetooth/status"
    )

    assert response.status_code == 200, response.text
    assert response.json() == expected
    assert TOKEN not in response.text
    record = fake_node.requests[-1]
    assert record["method"] == "GET"
    assert record["path"] == "/bluetooth/status"
    assert record["headers"]["authorization"] == f"Bearer {TOKEN}"


def test_bluetooth_scan_proxy_forwards_bounds_and_preserves_typed_timeout(
    fake_node, tmp_path
):
    expected = {
        "ok": False,
        "supported": True,
        "configured": True,
        "available": False,
        "code": "bluetooth.timeout",
        "error": "bluetoothctl bluetoothctl timed out",
        "remediation": "Retry with a shorter scan.",
    }
    fake_node.responder = lambda _method, _record: (504, expected, "application/json")
    app, _ = _build_app(fake_node.server_address[1], tmp_path, global_token=TOKEN)

    response = _client(app).post(
        f"/api/v1/nodes/{NODE_ID}/bluetooth/scan",
        json={"duration_seconds": 15},
    )

    assert response.status_code == 504, response.text
    assert response.json() == expected
    assert TOKEN not in response.text
    record = fake_node.requests[-1]
    assert record["method"] == "POST"
    assert record["path"] == "/bluetooth/scan"
    assert record["body"] == {"duration_seconds": 15}
    assert record["headers"]["authorization"] == f"Bearer {TOKEN}"


def test_bluetooth_pair_proxy_preserves_typed_invalid_address(fake_node, tmp_path):
    expected = {
        "ok": False,
        "supported": True,
        "configured": True,
        "available": False,
        "code": "bluetooth.invalid-address",
        "error": "the Bluetooth address is invalid; expected AA:BB:CC:DD:EE:FF",
        "remediation": "Send exactly one six-octet MAC address.",
    }
    fake_node.responder = lambda _method, _record: (400, expected, "application/json")
    app, _ = _build_app(fake_node.server_address[1], tmp_path, global_token=TOKEN)

    response = _client(app).post(
        f"/api/v1/nodes/{NODE_ID}/bluetooth/pair",
        json={"address": "not-a-mac"},
    )

    assert response.status_code == 400, response.text
    assert response.json() == expected
    assert TOKEN not in response.text
    record = fake_node.requests[-1]
    assert record["path"] == "/bluetooth/pair"
    assert record["body"] == {"address": "not-a-mac"}
    assert record["headers"]["authorization"] == f"Bearer {TOKEN}"


def test_bluetooth_forget_proxy_preserves_typed_unavailable(fake_node, tmp_path):
    expected = {
        "ok": False,
        "supported": True,
        "configured": False,
        "available": False,
        "code": "bluetooth.not-configured",
        "error": "Bluetooth is not configured on this node",
        "remediation": "Start the node with --bluetooth.",
    }
    fake_node.responder = lambda _method, _record: (503, expected, "application/json")
    app, _ = _build_app(fake_node.server_address[1], tmp_path, global_token=TOKEN)

    response = _client(app).post(
        f"/api/v1/nodes/{NODE_ID}/bluetooth/forget",
        json={"address": "AA:BB:CC:DD:EE:FF"},
    )

    assert response.status_code == 503, response.text
    assert response.json() == expected
    assert TOKEN not in response.text
    record = fake_node.requests[-1]
    assert record["method"] == "POST"
    assert record["path"] == "/bluetooth/forget"
    assert record["body"] == {"address": "AA:BB:CC:DD:EE:FF"}
    assert record["headers"]["authorization"] == f"Bearer {TOKEN}"


def test_audio_nodes_proxy_preserves_real_graph_json(fake_node, tmp_path):
    expected = {
        "ok": True,
        "available": True,
        "backend": "pipewire",
        "nodes": [
            {
                "id": 42,
                "name": "ozma-vm1",
                "description": "Ozma VM 1",
                "media_class": "Audio/Sink",
                "default": True,
                "state": "RUNNING",
                "mute": False,
                "volume_percent": 75.0,
                "monitor_source": "ozma-vm1.monitor",
                "monitor_of_sink": None,
            }
        ],
        "streams": [],
        "links": [],
        "default_sink": "ozma-vm1",
        "default_source": "ozma-vm1.monitor",
        "truncated": False,
        "links_available": False,
    }
    fake_node.responder = lambda _method, _record: (200, expected, "application/json")
    app, _ = _build_app(fake_node.server_address[1], tmp_path, global_token=TOKEN)

    response = _client(app).get(f"/api/v1/audio/nodes?node_id={NODE_ID}")

    assert response.status_code == 200, response.text
    assert response.json() == expected
    assert TOKEN not in response.text
    record = fake_node.requests[-1]
    assert record["method"] == "GET"
    assert record["path"] == "/audio/nodes"
    assert record["headers"]["authorization"] == f"Bearer {TOKEN}"


def test_audio_sweep_proxy_passes_typed_error_and_forwards_bounds(fake_node, tmp_path):
    expected = {
        "ok": False,
        "available": False,
        "profile": None,
        "code": "audio.missing_command",
        "error": "audio routing command `pw-play` was not found",
        "remediation": "Install pipewire-bin.",
    }
    fake_node.responder = lambda _method, _record: (503, expected, "application/json")
    app, _ = _build_app(fake_node.server_address[1], tmp_path, global_token=TOKEN)

    response = _client(app).post(
        "/api/v1/audio/room-correction/sweep",
        json={
            "node_id": NODE_ID,
            "source": "ozma-vm1.monitor",
            "sink": "ozma-vm1",
            "start_hz": 30,
            "end_hz": 16000,
            "duration_secs": 3,
            "room_name": "Office",
        },
    )

    assert response.status_code == 503, response.text
    assert response.json() == expected
    assert TOKEN not in response.text
    record = fake_node.requests[-1]
    assert record["method"] == "POST"
    assert record["path"] == "/audio/sweep"
    assert record["headers"]["authorization"] == f"Bearer {TOKEN}"
    assert record["body"] == {
        "source": "ozma-vm1.monitor",
        "sink": "ozma-vm1",
        "start_hz": 30,
        "end_hz": 16000,
        "duration_secs": 3,
        "room_name": "Office",
    }


def test_audio_sweep_proxy_surfaces_node_disconnect_as_typed_502(fake_node, tmp_path):
    app, _ = _build_app(_unused_port(), tmp_path, global_token=TOKEN)

    response = _client(app).post(
        "/api/v1/audio/room-correction/sweep",
        json={"node_id": NODE_ID, "source": "monitor", "sink": "sink"},
    )

    assert response.status_code == 502, response.text
    body = response.json()
    assert body["ok"] is False
    assert body["available"] is False
    assert body["code"] == "audio.proxy_failed"
    assert TOKEN not in response.text


def test_audio_profile_get_preserves_typed_node_error(fake_node, tmp_path):
    expected = {
        "ok": False,
        "available": False,
        "stored": False,
        "profile": None,
        "code": "audio-profile-store-unsupported",
        "error": "room-correction profile storage is not enabled on this node",
    }
    fake_node.responder = lambda _method, _record: (501, expected, "application/json")
    app, _ = _build_app(fake_node.server_address[1], tmp_path, global_token=TOKEN)

    response = _client(app).get(
        f"/api/v1/audio/room-correction/profile?node_id={NODE_ID}"
    )

    assert response.status_code == 501, response.text
    assert response.json() == expected
    assert TOKEN not in response.text
    record = fake_node.requests[-1]
    assert record["method"] == "GET"
    assert record["path"] == "/audio/room-correction/profile"
    assert record["headers"]["authorization"] == f"Bearer {TOKEN}"


def test_audio_profile_put_forwards_without_node_id_and_preserves_errors(fake_node, tmp_path):
    expected = {
        "ok": False,
        "available": False,
        "stored": False,
        "profile": None,
        "code": "audio.invalid_spec",
        "error": "Invalid audio routing room profile id: must not be empty.",
    }
    fake_node.responder = lambda _method, _record: (400, expected, "application/json")
    app, _ = _build_app(fake_node.server_address[1], tmp_path, global_token=TOKEN)
    profile = {"id": "", "name": "broken", "bands": [], "band_count": 0}

    response = _client(app).post(
        "/api/v1/audio/room-correction/profile",
        json={"node_id": NODE_ID, "profile": profile},
    )

    assert response.status_code == 400, response.text
    assert response.json() == expected
    assert TOKEN not in response.text
    record = fake_node.requests[-1]
    assert record["method"] == "POST"
    assert record["path"] == "/audio/room-correction/profile"
    assert record["body"] == {"profile": profile}
    assert record["headers"]["authorization"] == f"Bearer {TOKEN}"


def test_audio_profile_delete_proxies_delete_and_returns_body(fake_node, tmp_path):
    expected = {"ok": True, "available": True, "cleared": True, "backend": "pipewire"}
    fake_node.responder = lambda _method, _record: (200, expected, "application/json")
    app, _ = _build_app(fake_node.server_address[1], tmp_path, global_token=TOKEN)

    response = _client(app).delete(
        f"/api/v1/audio/room-correction/profile?node_id={NODE_ID}"
    )

    assert response.status_code == 200, response.text
    assert response.json() == expected
    assert TOKEN not in response.text
    record = fake_node.requests[-1]
    assert record["method"] == "DELETE"
    assert record["path"] == "/audio/room-correction/profile"
    assert record["headers"]["authorization"] == f"Bearer {TOKEN}"
