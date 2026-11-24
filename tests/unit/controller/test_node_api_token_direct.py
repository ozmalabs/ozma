# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Direct controller → node bearer-token coverage.

The API proxies are covered by ``test_node_api_token_proxy.py``. This module
covers background/direct node clients that do not go through ``api.py``:
control-surface power, agent hints, soft-node stream snapshot/input/codec,
terminal snapshots, serial polling, RGB zones, mesh status, and backup proxy.

Each test uses a real local HTTP server as the fake node and asserts that a
configured token becomes ``Authorization: Bearer ...`` while an unconfigured
controller keeps the previous unauthenticated request.
"""

import asyncio
import contextlib
import json
import logging
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from types import SimpleNamespace

import pytest

from agent_engine import AgentEngine
from backup_status import BackupNudgeService
from controls import ControlManager
from mesh_intelligence import MeshIntelligence
from node_auth import (
    node_api_headers,
    node_api_headers_for_state,
    node_api_token,
    node_auth_error,
)
from rgb_outputs import RGBOutputManager, RGBZone
from serial_console import SerialConsoleManager
from state import AppState, NodeInfo
from stream import StreamManager, _SoftNodeStream
from terminal_bridge import TerminalBridgeManager

pytestmark = pytest.mark.unit

TOKEN = "direct" + "-node-token"
NODE_ID = "direct-token-node._ozma._udp.local."
JPEG = b"\xff\xd8\xff\xe0fake-jpeg\xff\xd9"


class _FakeNodeHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def _handle(self, method: str) -> None:
        try:
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
        except (BrokenPipeError, ConnectionResetError):
            pass

    def do_GET(self) -> None:  # noqa: N802 — http.server API
        self._handle("GET")

    def do_POST(self) -> None:  # noqa: N802 — http.server API
        self._handle("POST")

    def log_message(self, *args) -> None:
        pass


def _default_responder(method: str, record: dict):
    path = record["path"]
    if path.startswith("/display/snapshot"):
        return 200, JPEG, "image/jpeg"
    if path.startswith("/serial/buffer"):
        return 200, [], "application/json"
    if path.startswith("/node/status"):
        return 200, {"network": {}, "cpu": {}, "memory": {}, "storage": []}, "application/json"
    if path.startswith("/backup/snapshots"):
        return 200, {"snapshots": []}, "application/json"
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


def _node(port: int, *, api_token: str = "") -> NodeInfo:
    return NodeInfo(
        id=NODE_ID,
        host="127.0.0.1",
        port=7331,
        role="compute",
        hw="soft",
        fw_version="0.1.0",
        proto_version=1,
        capabilities=["hid", "video", "audio", "power", "serial", "rgb"],
        stream_port=port,
        api_port=port,
        api_token=api_token,
    )


def _state(port: int, *, global_token: str = "", node_token: str = ""):
    state = AppState()
    state.node_api_token = global_token
    node = _node(port, api_token=node_token)
    state.nodes[NODE_ID] = node
    return state, node


def _auth(node_or_none, state):
    header = node_api_headers_for_state(node_or_none, state)
    return header.get("Authorization")


@pytest.mark.parametrize("token,expected", [(TOKEN, f"Bearer {TOKEN}"), ("", None)])
async def test_controls_power_sends_token(fake_node, token, expected):
    state, node = _state(fake_node.server_address[1], global_token=token)
    manager = ControlManager(state, object())

    await manager._proxy_power(node.id, "on")

    record = fake_node.requests[-1]
    assert record["path"].startswith("/power/on")
    assert record["headers"].get("authorization") == expected


@pytest.mark.parametrize("token,expected", [(TOKEN, f"Bearer {TOKEN}"), ("", None)])
async def test_agent_hints_sends_token(fake_node, token, expected):
    state, node = _state(fake_node.server_address[1], global_token=token)
    engine = AgentEngine(state)

    hints = await engine._query_agent_hints(node)

    assert hints == {"ok": True}
    record = fake_node.requests[-1]
    assert record["path"].startswith("/ui/hints")
    assert record["headers"].get("authorization") == expected


@pytest.mark.parametrize("token,expected", [(TOKEN, f"Bearer {TOKEN}"), ("", None)])
async def test_soft_node_snapshot_pull_sends_token(fake_node, token, expected):
    cap = _SoftNodeStream(NODE_ID, "127.0.0.1", fake_node.server_address[1], token=token)
    cap.active = True
    task = asyncio.create_task(cap._pull_loop())
    try:
        for _ in range(50):
            if fake_node.requests:
                break
            await asyncio.sleep(0.02)
    finally:
        cap.active = False
        task.cancel()
        with contextlib.suppress(asyncio.CancelledError):
            await task

    record = fake_node.requests[-1]
    assert record["path"].startswith("/display/snapshot")
    assert record["headers"].get("authorization") == expected


@pytest.mark.parametrize("token,expected", [(TOKEN, f"Bearer {TOKEN}"), ("", None)])
async def test_soft_node_input_and_codec_send_token(fake_node, token, expected):
    state, node = _state(fake_node.server_address[1], global_token=token)
    manager = StreamManager(state)
    cap = _SoftNodeStream(node.id, node.host, node.api_port, token=token)
    manager._captures[node.id] = cap
    config = SimpleNamespace(codec="h264", hw_accel="auto", to_dict=lambda: {"codec": "h264"})

    await cap.send_pointer(10, 20, 0)
    await cap.send_key("a", True)
    assert await manager.switch_codec(node.id, config) is True

    by_path = {r["path"].split("?")[0]: r for r in fake_node.requests}
    for path in ("/input/mouse", "/input/key", "/codec"):
        assert by_path[path]["headers"].get("authorization") == expected


@pytest.mark.parametrize("token,expected", [(TOKEN, f"Bearer {TOKEN}"), ("", None)])
async def test_terminal_bridge_snapshot_sends_token(fake_node, token, expected):
    state, node = _state(fake_node.server_address[1], global_token=token)
    manager = TerminalBridgeManager(state)

    frame = await manager._make_frame_fn(node.id, node)()

    assert frame == JPEG
    record = fake_node.requests[-1]
    assert record["path"].startswith("/display/snapshot")
    assert record["headers"].get("authorization") == expected


@pytest.mark.parametrize("token,expected", [(TOKEN, f"Bearer {TOKEN}"), ("", None)])
async def test_serial_console_poll_sends_token(fake_node, token, expected):
    state, _ = _state(fake_node.server_address[1], global_token=token)
    manager = SerialConsoleManager(state)

    await manager._poll_node_serial()

    record = fake_node.requests[-1]
    assert record["path"].startswith("/serial/buffer")
    assert record["headers"].get("authorization") == expected


@pytest.mark.parametrize("token,expected", [(TOKEN, f"Bearer {TOKEN}"), ("", None)])
async def test_rgb_node_push_sends_token(fake_node, token, expected):
    state, node = _state(fake_node.server_address[1], global_token=token)
    manager = RGBOutputManager()
    manager.set_state(state)
    zone = RGBZone(
        id="node-rgb", name="node", zone_type="node",
        host=node.host, port=node.api_port, node_id=node.id,
    )

    await manager._push_node(zone, (1, 2, 3))

    record = fake_node.requests[-1]
    assert record["path"].startswith("/rgb/set")
    assert record["headers"].get("authorization") == expected


@pytest.mark.parametrize("token,expected", [(TOKEN, f"Bearer {TOKEN}"), ("", None)])
async def test_mesh_status_poll_sends_token(fake_node, token, expected):
    state, _ = _state(fake_node.server_address[1], global_token=token)
    manager = MeshIntelligence(state)

    await manager._collect_mesh_state()

    record = fake_node.requests[-1]
    assert record["path"].startswith("/node/status")
    assert record["headers"].get("authorization") == expected


@pytest.mark.parametrize("token,expected", [(TOKEN, f"Bearer {TOKEN}"), ("", None)])
async def test_backup_proxy_sends_token(fake_node, token, expected):
    state, node = _state(fake_node.server_address[1], global_token=token)
    service = BackupNudgeService(state, tracker=object())

    assert await service.proxy_get(node.id, "/backup/snapshots") == {"snapshots": []}
    assert await service.proxy_post(node.id, "/backup/run", {}) == {"ok": True}

    for record in fake_node.requests[-2:]:
        assert record["headers"].get("authorization") == expected


def test_helper_prefers_per_node_token_then_global():
    state, node = _state(1, global_token="global-token", node_token="node-token")

    assert node_api_token(node, "global-token") == "node-token"
    assert node_api_headers_for_state(node, state) == {
        "Authorization": "Bearer node-token"
    }
    assert node_api_headers_for_state(_node(1), state) == {
        "Authorization": "Bearer global-token"
    }
    assert node_api_headers_for_state(_node(1), AppState()) == {}
    assert node_api_headers(None, "  spaced-token  ") == {
        "Authorization": "Bearer spaced-token"
    }


def test_auth_error_is_token_free_and_actionable():
    message = node_auth_error(403)
    assert "403" in message
    assert "bearer token" in message.lower()
    assert "OZMA_NODE_API_TOKEN" in message


async def test_401_and_403_do_not_leak_token_to_logs(fake_node, caplog):
    caplog.set_level(logging.DEBUG)
    fake_node.responder = lambda method, record: (
        401, {"error": "unauthorized"}, "application/json"
    )
    state, node = _state(fake_node.server_address[1], global_token=TOKEN)
    controls = ControlManager(state, object())
    engine = AgentEngine(state)
    stream = StreamManager(state)
    cap = _SoftNodeStream(node.id, node.host, node.api_port, token=TOKEN)
    stream._captures[node.id] = cap
    config = SimpleNamespace(codec="h264", hw_accel="auto", to_dict=lambda: {"codec": "h264"})

    await controls._proxy_power(node.id, "on")
    assert await engine._query_agent_hints(node) is None
    await stream.switch_codec(node.id, config)

    fake_node.responder = lambda method, record: (
        403, {"error": "forbidden"}, "application/json"
    )
    await controls._proxy_power(node.id, "on")

    assert TOKEN not in caplog.text
    assert _auth(node, state) == f"Bearer {TOKEN}"
