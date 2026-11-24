# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Persistent per-node bearer-token store and rotation API tests.

Uses a real local HTTP server as the fake node. Covers persistence across
controller reloads, store-over-registration precedence, rotation propagating
to proxy headers (including an existing soft-node stream), revocation, atomic
0600 writes, and that secrets never appear in list/detail/event/log output.
"""

import asyncio
import json
import logging
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

from api import build_app
from auth import AuthConfig
from node_tokens import NodeTokenStore
from scenarios import ScenarioManager
from state import AppState, NodeInfo
from stream import StreamManager, _SoftNodeStream

pytestmark = pytest.mark.unit

NODE_ID = "token-store-node._ozma._udp.local."
TOKEN = "fake-node" + "-bearer-value"
ROTATED = "rotated-node" + "-bearer-value-2"


class _FakeNodeHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def _handle(self, method: str) -> None:
        length = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(length) if length else b""
        record = {
            "method": method,
            "path": self.path,
            "headers": {k.lower(): v for k, v in self.headers.items()},
            "body": raw,
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

    def do_POST(self) -> None:  # noqa: N802 — http.server API
        self._handle("POST")

    def do_DELETE(self) -> None:  # noqa: N802 — http.server API
        self._handle("DELETE")

    def log_message(self, *args) -> None:
        pass


def _responder(method: str, record: dict):
    return 200, {"ok": True}, "application/json"


@pytest.fixture
def fake_node():
    server = ThreadingHTTPServer(("127.0.0.1", 0), _FakeNodeHandler)
    server.requests = []
    server.responder = _responder
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


def _node(port: int, *, api_token: str = "", hw: str = "x86_64") -> NodeInfo:
    return NodeInfo(
        id=NODE_ID,
        host="127.0.0.1",
        port=7331,
        role="compute",
        hw=hw,
        fw_version="0.1.0",
        proto_version=1,
        capabilities=["hid", "video", "audio", "power"],
        stream_port=port,
        api_port=port,
        api_token=api_token,
    )


def _build_app(state: AppState, tmp_path, *, global_token: str = ""):
    streams = StreamManager(state, static_dir=tmp_path / "streams")
    scenarios = ScenarioManager(tmp_path / "scenarios.json", state)
    app = build_app(
        state,
        scenarios,
        streams=streams,
        auth_config=AuthConfig(enabled=False),
        node_api_token=global_token,
    )
    return app, streams


def _client(app):
    from fastapi.testclient import TestClient

    return TestClient(app)


# ── Store ─────────────────────────────────────────────────────────────────────


def test_store_writes_atomic_private_json(tmp_path):
    path = tmp_path / "node_tokens.json"
    store = NodeTokenStore(path)

    assert store.set(NODE_ID, TOKEN, source="rotation") is True

    assert path.exists()
    assert path.stat().st_mode & 0o777 == 0o600
    data = json.loads(path.read_text())
    assert data["version"] == 1
    assert data["revoked"] == []
    assert data["nodes"][NODE_ID]["token"] == TOKEN
    assert data["nodes"][NODE_ID]["source"] == "rotation"
    assert isinstance(data["nodes"][NODE_ID]["updated_at"], float)
    # Atomic write leaves no temp file behind.
    assert list(tmp_path.glob(f".{path.name}.*")) == []

    reloaded = NodeTokenStore(path)
    assert reloaded.get(NODE_ID) == TOKEN
    assert TOKEN not in json.dumps(reloaded.metadata(NODE_ID))


def test_store_tolerates_malformed_entries(tmp_path):
    path = tmp_path / "node_tokens.json"
    path.write_text(json.dumps({
        "version": 1,
        "nodes": {
            "good": {"token": TOKEN, "source": "manual", "updated_at": 1.0},
            "blank": {"token": "  "},
            "not-a-dict": "nope",
        },
    }))

    store = NodeTokenStore(path)

    assert store.get("good") == TOKEN
    assert store.get("blank") == ""
    assert store.node_ids() == ["good"]


def test_registration_token_is_persisted_and_reloaded(tmp_path):
    path = tmp_path / "node_tokens.json"

    async def _run():
        state = AppState(node_token_store_path=path)
        await state.add_node(_node(0, api_token=TOKEN))

        fresh = AppState(node_token_store_path=path)
        await fresh.add_node(_node(0))  # re-registration without a token
        return NodeTokenStore(path), fresh

    store, fresh = asyncio.run(_run())
    assert store.get(NODE_ID) == TOKEN
    assert fresh.nodes[NODE_ID].api_token == TOKEN


def test_store_token_overrides_registration_token(tmp_path):
    path = tmp_path / "node_tokens.json"
    NodeTokenStore(path).set(NODE_ID, TOKEN, source="rotation")

    async def _run():
        state = AppState(node_token_store_path=path)
        await state.add_node(_node(0, api_token="registration-token"))
        return state

    state = asyncio.run(_run())
    assert state.nodes[NODE_ID].api_token == TOKEN


def test_rotation_survives_reregistration_and_reload(tmp_path):
    path = tmp_path / "node_tokens.json"

    async def _run():
        state = AppState(node_token_store_path=path)
        await state.add_node(_node(0, api_token="registration-token"))
        state.set_node_token(NODE_ID, ROTATED)

        fresh = AppState(node_token_store_path=path)
        await fresh.add_node(_node(0, api_token="registration-token"))
        return fresh

    fresh = asyncio.run(_run())
    assert fresh.nodes[NODE_ID].api_token == ROTATED


def test_node_removal_cleanup_is_explicit(tmp_path):
    path = tmp_path / "node_tokens.json"

    async def _run():
        state = AppState(node_token_store_path=path)
        await state.add_node(_node(0, api_token=TOKEN))

        # A transient mDNS disappearance must not destroy a rotated token.
        await state.remove_node(NODE_ID)
        kept = state.node_tokens.get(NODE_ID)

        await state.add_node(_node(0, api_token=TOKEN))
        await state.remove_node(NODE_ID, forget_token=True)
        return kept, state.node_tokens.get(NODE_ID)

    kept, after_forget = asyncio.run(_run())
    assert kept == TOKEN
    assert after_forget == ""


def test_revocation_survives_reload_and_reregistration(tmp_path):
    path = tmp_path / "node_tokens.json"

    async def _run():
        state = AppState(node_token_store_path=path)
        await state.add_node(_node(0, api_token=TOKEN))
        state.clear_node_token(NODE_ID)

        fresh = AppState(node_token_store_path=path)
        await fresh.add_node(_node(0, api_token=TOKEN))
        store = NodeTokenStore(path)
        meta = store.metadata(NODE_ID)
        fresh_token = fresh.nodes[NODE_ID].api_token

        fresh.set_node_token(NODE_ID, ROTATED)
        reenabled = NodeTokenStore(path)
        return fresh_token, store, meta, reenabled

    fresh_token, store, meta, reenabled = asyncio.run(_run())
    assert fresh_token == ""
    assert store.get(NODE_ID) == ""
    assert meta["configured"] is False
    assert meta["revoked"] is True
    # An explicit rotation clears the tombstone.
    assert reenabled.get(NODE_ID) == ROTATED
    assert reenabled.metadata(NODE_ID)["revoked"] is False


# ── Rotation API + proxy propagation ─────────────────────────────────────────


def test_rotation_api_returns_generated_token_once_and_proxies_it(
    fake_node, tmp_path, caplog
):
    caplog.set_level(logging.DEBUG)
    state = AppState(node_token_store_path=tmp_path / "node_tokens.json")
    state.nodes[NODE_ID] = _node(fake_node.server_address[1], api_token=TOKEN)
    state.node_tokens.set(NODE_ID, TOKEN, source="registration")
    app, _ = _build_app(state, tmp_path)
    client = _client(app)

    response = client.put(f"/api/v1/nodes/{NODE_ID}/token")

    assert response.status_code == 200, response.text
    body = response.json()
    assert body["rotated"] is True
    assert body["token_returned_once"] is True
    assert body["fallback_configured"] is False
    new_token = body["token"]
    assert new_token and new_token != TOKEN

    # Proxy reads the live node token, so rotation takes effect immediately.
    proxied = client.get(f"/api/v1/nodes/{NODE_ID}/power/state")
    assert proxied.status_code == 200, proxied.text
    assert fake_node.requests[-1]["headers"]["authorization"] == f"Bearer {new_token}"

    # Subsequent metadata reads and writes never echo the secret.
    meta = client.get(f"/api/v1/nodes/{NODE_ID}/token")
    assert meta.status_code == 200
    assert meta.json()["configured"] is True
    assert meta.json()["fallback_configured"] is False
    assert "token" not in meta.json()
    assert new_token not in meta.text

    supplied = client.put(
        f"/api/v1/nodes/{NODE_ID}/token", json={"token": "operator-supplied"}
    )
    assert supplied.status_code == 200, supplied.text
    assert "token" not in supplied.json()
    assert "operator-supplied" not in supplied.text
    proxied = client.get(f"/api/v1/nodes/{NODE_ID}/power/state")
    assert proxied.status_code == 200, proxied.text
    assert fake_node.requests[-1]["headers"]["authorization"] == "Bearer operator-supplied"

    assert TOKEN not in caplog.text
    assert new_token not in caplog.text
    assert "operator-supplied" not in caplog.text


def test_rotation_updates_existing_soft_node_stream(fake_node, tmp_path):
    state = AppState(node_token_store_path=tmp_path / "node_tokens.json")
    node = _node(fake_node.server_address[1], api_token=TOKEN, hw="soft")
    state.nodes[NODE_ID] = node
    state.node_tokens.set(NODE_ID, TOKEN, source="registration")
    app, streams = _build_app(state, tmp_path)
    entry = _SoftNodeStream(
        NODE_ID, node.host, node.api_port, token=TOKEN
    )
    streams._captures[NODE_ID] = entry

    response = _client(app).put(
        f"/api/v1/nodes/{NODE_ID}/token", json={"token": ROTATED}
    )

    assert response.status_code == 200, response.text
    assert entry._token == ROTATED


def test_revoked_token_stops_being_sent(fake_node, tmp_path):
    state = AppState(node_token_store_path=tmp_path / "node_tokens.json")
    state.nodes[NODE_ID] = _node(fake_node.server_address[1], api_token=TOKEN)
    state.node_tokens.set(NODE_ID, TOKEN, source="registration")
    app, _ = _build_app(state, tmp_path)
    client = _client(app)

    revoked = client.delete(f"/api/v1/nodes/{NODE_ID}/token")

    assert revoked.status_code == 200, revoked.text
    assert revoked.json()["cleared"] is True
    assert revoked.json()["revoked"] is True
    assert revoked.json()["fallback_configured"] is False
    assert TOKEN not in revoked.text
    assert state.node_tokens.get(NODE_ID) == ""

    proxied = client.get(f"/api/v1/nodes/{NODE_ID}/power/state")
    assert proxied.status_code == 200, proxied.text
    assert "authorization" not in fake_node.requests[-1]["headers"]


def test_revocation_falls_back_to_global_token(fake_node, tmp_path):
    state = AppState(node_token_store_path=tmp_path / "node_tokens.json")
    state.nodes[NODE_ID] = _node(fake_node.server_address[1], api_token=TOKEN)
    state.node_tokens.set(NODE_ID, TOKEN, source="registration")
    app, _ = _build_app(state, tmp_path, global_token="global-fallback")
    client = _client(app)

    assert client.delete(f"/api/v1/nodes/{NODE_ID}/token").status_code == 200
    meta = client.get(f"/api/v1/nodes/{NODE_ID}/token")
    assert meta.status_code == 200
    assert meta.json()["fallback_configured"] is True
    assert "global-fallback" not in meta.text
    assert client.get(f"/api/v1/nodes/{NODE_ID}/power/state").status_code == 200

    auth = fake_node.requests[-1]["headers"]["authorization"]
    assert auth == "Bearer global-fallback"
    assert TOKEN not in auth


def test_malformed_token_body_does_not_echo_secret(tmp_path):
    state = AppState(node_token_store_path=tmp_path / "node_tokens.json")
    state.nodes[NODE_ID] = _node(0)
    app, _ = _build_app(state, tmp_path)
    client = _client(app)

    response = client.put(
        f"/api/v1/nodes/{NODE_ID}/token", json={"token": {"secret": TOKEN}}
    )

    assert response.status_code == 400
    assert TOKEN not in response.text
    assert state.node_tokens.get(NODE_ID) == ""


def test_token_unknown_node_is_404(tmp_path):
    state = AppState(node_token_store_path=tmp_path / "node_tokens.json")
    app, _ = _build_app(state, tmp_path)
    client = _client(app)

    assert client.get("/api/v1/nodes/missing/token").status_code == 404
    assert client.put("/api/v1/nodes/missing/token").status_code == 404
    assert client.delete("/api/v1/nodes/missing/token").status_code == 404


# ── Secret exposure ──────────────────────────────────────────────────────────


def test_secret_absent_from_list_detail_events_and_logs(fake_node, tmp_path, caplog):
    caplog.set_level(logging.DEBUG)

    async def _run():
        state = AppState(node_token_store_path=tmp_path / "node_tokens.json")
        await state.add_node(_node(fake_node.server_address[1], api_token=TOKEN))
        return state

    state = asyncio.run(_run())
    app, _ = _build_app(state, tmp_path)
    client = _client(app)

    listed = client.get("/api/v1/nodes")
    detail = client.get(f"/api/v1/nodes/{NODE_ID}")

    assert listed.status_code == 200
    assert detail.status_code == 200
    for response in (listed, detail):
        assert TOKEN not in response.text
        assert "api_token" not in response.text
        assert "node_tokens" not in response.text

    events = []
    while not state.events.empty():
        events.append(state.events.get_nowait())
    assert events
    assert TOKEN not in json.dumps(events)
    assert TOKEN not in caplog.text

    # Startup reload also stays quiet.
    NodeTokenStore(state.node_tokens.path)
    assert TOKEN not in caplog.text
