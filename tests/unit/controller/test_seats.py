# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Read-only multi-seat grouping tests.

Covers the controller-side derivation used by ``GET /api/v1/seats`` and the
dashboard Seats panel. All nodes are fakes: no real discovery, supervisor, or
node process is involved.
"""

from __future__ import annotations

import json
import shutil
import subprocess
import time
from pathlib import Path

import pytest

import seats
from seats import group_seats, normalize_node_id, parse_seat_id, seat_identity
from state import AppState, NodeInfo

pytestmark = pytest.mark.unit

DASHBOARD = Path(__file__).resolve().parents[3] / "controller" / "static" / "dashboard.html"


def _node(
    node_id: str,
    *,
    host: str = "10.0.0.10",
    port: int = 7331,
    api_port: int | None = None,
    stream_port: int | None = None,
    seat_id: str = "",
    parent_node_id: str = "",
    display_outputs: list[dict] | None = None,
    last_seen: float | None = None,
    api_token: str = "",
) -> NodeInfo:
    kwargs = dict(
        id=node_id,
        host=host,
        port=port,
        role="compute",
        hw="soft",
        fw_version="1.0.0",
        proto_version=1,
        capabilities=["hid", "screen", "audio"],
        api_port=api_port,
        stream_port=stream_port,
        seat_id=seat_id,
        parent_node_id=parent_node_id,
        display_outputs=display_outputs or [],
        api_token=api_token,
    )
    if last_seen is not None:
        kwargs["last_seen"] = last_seen
    return NodeInfo(**kwargs)


class TestSeatIdentity:
    def test_normalize_strips_mdns_suffix(self):
        assert normalize_node_id("host-seat-0._ozma._udp.local.") == "host-seat-0"
        assert normalize_node_id(" host-seat-0._ozma._tcp.local ") == "host-seat-0"
        assert normalize_node_id("plain") == "plain"
        assert normalize_node_id(None) == ""

    @pytest.mark.parametrize(
        "value,expected",
        [
            ("host-seat-0", ("host", 0)),
            ("my-pc-seat-12", ("my-pc", 12)),
            ("host-seat-0._ozma._udp.local.", ("host", 0)),
            ("host-seat-07", ("host", 7)),
            ("seat-0", None),
            ("myseat-1", None),
            ("host-seat", None),
            ("host-seat-x", None),
            ("workstation-1", None),
        ],
    )
    def test_parse_seat_id_convention(self, value, expected):
        assert parse_seat_id(value) == expected

    def test_ordinary_node_has_no_identity(self):
        assert seat_identity(_node("workstation-1._ozma._udp.local.")) is None

    def test_explicit_seat_id_wins_over_node_name(self):
        identity = seat_identity(
            _node("worker-7._ozma._udp.local.", seat_id="host-x-seat-3")
        )
        assert identity == {
            "seat_id": "host-x-seat-3",
            "machine": "host-x",
            "seat_index": 3,
        }

    def test_parent_node_id_marks_a_seat_without_name_convention(self):
        identity = seat_identity(
            _node("worker-7._ozma._udp.local.", parent_node_id="host-y._ozma._udp.local.")
        )
        assert identity == {
            "seat_id": "worker-7",
            "machine": "host-y",
            "seat_index": None,
        }

    def test_parent_machine_overrides_name_prefix(self):
        identity = seat_identity(
            _node("old-name-seat-2._ozma._udp.local.", parent_node_id="host-z")
        )
        assert identity["machine"] == "host-z"
        assert identity["seat_index"] == 2


class TestSeatGrouping:
    def test_name_convention_groups_by_machine_and_seat(self):
        nodes = [
            _node(
                "host-a-seat-0._ozma._udp.local.",
                host="10.0.0.10",
                port=7331,
                api_port=7382,
                display_outputs=[{"width": 1920, "height": 1080, "name": "HDMI-1"}],
            ),
            _node(
                "host-a-seat-1._ozma._udp.local.",
                host="10.0.0.10",
                port=7332,
                api_port=7383,
            ),
        ]

        result = group_seats(nodes, now=1000.0)

        assert result["seat_count"] == 2
        assert len(result["groups"]) == 1
        group = result["groups"][0]
        assert group["machine"] == "host-a"
        assert group["host"] == "host-a"
        assert group["seat_count"] == 2
        assert [seat["seat_index"] for seat in group["seats"]] == [0, 1]
        assert [seat["node_id"] for seat in group["seats"]] == [
            "host-a-seat-0._ozma._udp.local.",
            "host-a-seat-1._ozma._udp.local.",
        ]
        seat0 = group["seats"][0]
        assert seat0["port"] == 7331
        assert seat0["api_port"] == 7382
        assert seat0["displays"] == [
            {"index": 0, "width": 1920, "height": 1080, "name": "HDMI-1"}
        ]

    def test_multi_host_separation(self):
        nodes = [
            _node("host-a-seat-0._ozma._udp.local.", host="10.0.0.10"),
            _node("host-b-seat-0._ozma._udp.local.", host="10.0.0.20"),
        ]

        result = group_seats(nodes, now=1000.0)

        assert [group["machine"] for group in result["groups"]] == ["host-a", "host-b"]
        assert result["seat_count"] == 2
        assert all(group["seat_count"] == 1 for group in result["groups"])

    def test_explicit_seat_id_and_parent_grouping(self):
        nodes = [
            _node("unrelated-1", seat_id="rig-1-seat-0"),
            _node("unrelated-2", parent_node_id="rig-1._ozma._udp.local."),
        ]

        result = group_seats(nodes, now=1000.0)

        assert len(result["groups"]) == 1
        group = result["groups"][0]
        assert group["machine"] == "rig-1"
        assert [seat["seat_index"] for seat in group["seats"]] == [0, None]
        assert result["unseated_node_ids"] == []

    def test_unrelated_nodes_never_become_seats(self):
        nodes = [
            _node("workstation-1._ozma._udp.local."),
            _node("seat-0._ozma._udp.local."),
            _node("desk-seat._ozma._udp.local."),
            _node("camera-front._ozma._udp.local."),
        ]

        result = group_seats(nodes, now=1000.0)

        assert result["groups"] == []
        assert result["seat_count"] == 0
        assert result["unseated_node_ids"] == [
            "camera-front._ozma._udp.local.",
            "desk-seat._ozma._udp.local.",
            "seat-0._ozma._udp.local.",
            "workstation-1._ozma._udp.local.",
        ]

    def test_online_and_offline_seats(self):
        nodes = [
            _node("host-a-seat-0", last_seen=990.0),
            _node("host-a-seat-1", last_seen=100.0),
        ]

        result = group_seats(nodes, now=1000.0, offline_after=180.0)

        group = result["groups"][0]
        assert group["online_count"] == 1
        assert group["offline_count"] == 1
        assert group["online"] is True
        assert group["all_online"] is False
        assert [seat["online"] for seat in group["seats"]] == [True, False]
        assert result["online_count"] == 1
        assert result["offline_count"] == 1

    def test_partial_group_is_online_but_not_all_online(self):
        result = group_seats(
            [_node("host-a-seat-0", last_seen=1.0)], now=1000.0, offline_after=180.0
        )

        group = result["groups"][0]
        assert group["online"] is False
        assert group["all_online"] is False
        assert group["offline_count"] == 1


# ── API ──────────────────────────────────────────────────────────────────────


@pytest.fixture(autouse=True)
def _isolate_mesh_config(monkeypatch, tmp_path):
    """Keep build_app from writing controller/mesh_network.json in the repo."""
    import mesh_network

    monkeypatch.setattr(mesh_network, "CONFIG_PATH", tmp_path / "mesh_network.json")


@pytest.fixture(autouse=True)
def _old_nodes_have_no_seat_report(monkeypatch):
    """Default every test to a node that does not serve /api/v1/seats.

    Individual tests override ``seats.fetch_node_seat_report`` when they want a
    node report. This keeps the network out of the default unit tests and
    proves the historical grouping is preserved when the route is absent.
    """

    async def _absent(node, **kwargs):
        return (404, None)

    monkeypatch.setattr(seats, "fetch_node_seat_report", _absent)


def _client(state: AppState, tmp_path: Path):
    from fastapi.testclient import TestClient
    from api import build_app
    from scenarios import ScenarioManager

    app = build_app(state, ScenarioManager(tmp_path / "scenarios.json", state))
    return TestClient(app)


def _state_with(*nodes: NodeInfo) -> AppState:
    state = AppState()
    for node in nodes:
        state.nodes[node.id] = node
    return state


class TestSeatsAPI:
    def test_seats_api_shape_and_grouping(self, tmp_path):
        state = _state_with(
            _node(
                "host-a-seat-0._ozma._udp.local.",
                port=7331,
                api_port=7382,
                display_outputs=[{"width": 2560, "height": 1440, "name": "DP-1"}],
            ),
            _node("host-a-seat-1._ozma._udp.local.", port=7332, api_port=7383),
            _node("host-b-seat-0._ozma._udp.local.", host="10.0.0.20"),
            _node("workstation-1._ozma._udp.local."),
        )

        response = _client(state, tmp_path).get("/api/v1/seats")

        assert response.status_code == 200
        payload = response.json()
        assert set(payload) == {
            "groups",
            "seat_count",
            "online_count",
            "offline_count",
            "unseated_node_ids",
        }
        assert payload["seat_count"] == 3
        assert payload["online_count"] == 3
        assert payload["unseated_node_ids"] == ["workstation-1._ozma._udp.local."]

        groups = {group["machine"]: group for group in payload["groups"]}
        assert set(groups) == {"host-a", "host-b"}
        assert groups["host-a"]["seat_count"] == 2
        assert groups["host-a"]["seats"][0]["displays"] == [
            {"index": 0, "width": 2560, "height": 1440, "name": "DP-1"}
        ]
        assert groups["host-b"]["seats"][0]["port"] == 7331

    def test_seats_api_never_exposes_tokens_or_stored_secrets(self, tmp_path):
        secret = "do-not-leak-this-node-token"
        state = _state_with(
            _node("host-a-seat-0", api_token=secret),
        )
        state.set_node_token("host-a-seat-0", secret, source="test")

        body = _client(state, tmp_path).get("/api/v1/seats").text

        assert secret not in body
        assert "api_token" not in body
        assert "token" not in body.lower()

    def test_register_accepts_optional_seat_id_and_old_nodes_still_work(self, tmp_path):
        state = AppState()
        client = _client(state, tmp_path)

        new_response = client.post(
            "/api/v1/nodes/register",
            json={
                "id": "host-seat-0._ozma._udp.local.",
                "host": "10.0.0.10",
                "seat_id": "host-seat-0",
            },
        )
        old_response = client.post(
            "/api/v1/nodes/register",
            json={"id": "legacy-node._ozma._udp.local.", "host": "10.0.0.11"},
        )

        assert new_response.status_code == 200
        assert new_response.json()["node"]["seat_id"] == "host-seat-0"
        assert old_response.status_code == 200
        assert "seat_id" not in old_response.json()["node"]
        assert state.nodes["legacy-node._ozma._udp.local."].seat_id == ""

        seats = client.get("/api/v1/seats").json()
        assert [group["machine"] for group in seats["groups"]] == ["host"]
        assert seats["unseated_node_ids"] == ["legacy-node._ozma._udp.local."]

    def test_seats_api_consumes_a_node_seat_report(self, tmp_path, monkeypatch):
        async def _fetch(node, **kwargs):
            if node.id == "worker-7._ozma._udp.local.":
                return (
                    200,
                    {
                        "ok": True,
                        "configured": True,
                        "mode": "read-only",
                        "lifecycle": "external",
                        "start_stop": False,
                        "seat_id": "host-x-seat-3",
                        "plan": {"version": 1, "seats": [{"id": "host-x-seat-3"}]},
                        "status": {
                            "version": 1,
                            "seat_count": 2,
                            "node_count": 2,
                            "seats": [
                                {
                                    "seat_id": "host-x-seat-3",
                                    "state": "planned",
                                    "available": True,
                                }
                            ],
                        },
                        "node_seat": {
                            "seat_id": "host-x-seat-3",
                            "state": "planned",
                            "available": True,
                        },
                    },
                )
            return (404, None)

        monkeypatch.setattr(seats, "fetch_node_seat_report", _fetch)
        state = _state_with(_node("worker-7._ozma._udp.local.", api_port=7382))

        payload = _client(state, tmp_path).get("/api/v1/seats").json()

        assert payload["seat_count"] == 1
        assert payload["unseated_node_ids"] == []
        group = payload["groups"][0]
        assert group["machine"] == "host-x"
        seat = group["seats"][0]
        assert seat["seat_id"] == "host-x-seat-3"
        assert seat["seat_index"] == 3
        assert seat["multiseat"] == {
            "mode": "read-only",
            "plan_seat_count": 2,
            "state": "planned",
            "available": True,
        }
        # The node's local store path/plan is never passed through.
        assert "source" not in seat
        assert "plan" not in seat

    def test_seats_api_passes_through_a_typed_node_error_and_keeps_grouping(
        self, tmp_path, monkeypatch
    ):
        async def _fetch(node, **kwargs):
            return (
                503,
                {
                    "ok": False,
                    "configured": True,
                    "mode": "error",
                    "error": {
                        "code": "seat_store_parse",
                        "message": "failed to parse seat store /secret/path/seats.json",
                    },
                },
            )

        monkeypatch.setattr(seats, "fetch_node_seat_report", _fetch)
        state = _state_with(_node("host-a-seat-0._ozma._udp.local."))

        response = _client(state, tmp_path).get("/api/v1/seats")
        payload = response.json()

        assert response.status_code == 200
        assert payload["seat_count"] == 1
        assert payload["groups"][0]["seats"][0]["seat_id"] == "host-a-seat-0"
        assert payload["node_errors"] == [
            {
                "node_id": "host-a-seat-0._ozma._udp.local.",
                "status": 503,
                "error": {
                    "code": "seat_store_parse",
                    "message": "failed to parse seat store /secret/path/seats.json",
                },
            }
        ]

    def test_seats_api_absent_report_keeps_the_historical_shape(self, tmp_path):
        # The autouse fixture answers 404 for every node, which is how an older
        # node or a node without --multiseat behaves.
        state = _state_with(_node("host-a-seat-0._ozma._udp.local."))

        payload = _client(state, tmp_path).get("/api/v1/seats").json()

        assert payload["seat_count"] == 1
        assert "node_errors" not in payload
        assert "multiseat" not in payload["groups"][0]["seats"][0]


# ── Dashboard (static + rendered) ────────────────────────────────────────────


def _render_seats_js(payload: dict, active_node_id: str) -> str:
    """Build a tiny Node harness around the dashboard's renderSeats()."""
    html = DASHBOARD.read_text()
    start = html.index("function renderSeats() {")
    end = html.index("// ── Render: supervisor", start)
    render_fn = html[start:end]
    return f"""
const list = {{ innerHTML: '' }};
const count = {{ textContent: '' }};
function $(id) {{
  if (id === 'seats-list') return list;
  if (id === 'seat-count') return count;
  return null;
}}
function esc(s) {{
  return String(s ?? '')
    .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;').replace(/'/g, '&#39;');
}}
function shortId(id) {{
  return String(id || '').replace(/\\._ozma\\._udp\\.local\\.$/, '').replace(/\\._ozma\\._tcp\\.local\\.$/, '');
}}
let activeNodeId = {json.dumps(active_node_id)};
let seatsData = {json.dumps(payload)};
{render_fn}
renderSeats();
process.stdout.write(JSON.stringify({{ html: list.innerHTML, count: count.textContent }}));
"""


class TestSeatsDashboard:
    def test_dashboard_has_read_only_seats_panel(self):
        html = DASHBOARD.read_text()

        assert 'id="panel-seats"' in html
        assert 'id="seats-list"' in html
        assert 'id="seat-count"' in html
        assert "api('GET', '/seats')" in html
        assert "function renderSeats()" in html
        assert "function fetchSeats()" in html

        start = html.index("function renderSeats() {")
        end = html.index("// ── Render: supervisor", start)
        block = html[start:end]
        assert "seat-group" in block
        assert "node-status-dot" in block
        assert "esc(" in block
        # Read-only: no destructive buttons, inline handlers, or fetch writes.
        assert "onclick" not in block
        assert "api(" not in block
        assert "nodePower" not in block

    def test_render_seats_groups_cards_and_online_state(self, tmp_path):
        node = shutil.which("node")
        if node is None:
            pytest.skip("node is not installed")

        payload = {
            "groups": [
                {
                    "machine": "host-a",
                    "host": "host-a",
                    "seat_count": 2,
                    "online_count": 1,
                    "offline_count": 1,
                    "online": True,
                    "all_online": False,
                    "seats": [
                        {
                            "node_id": "host-a-seat-0._ozma._udp.local.",
                            "seat_index": 0,
                            "online": True,
                            "port": 7331,
                            "api_port": 7382,
                            "displays": [
                                {"index": 0, "width": 1920, "height": 1080, "name": "HDMI-1"}
                            ],
                        },
                        {
                            "node_id": "host-a-seat-1._ozma._udp.local.",
                            "seat_index": 1,
                            "online": False,
                            "port": 7332,
                            "api_port": 7383,
                            "displays": [],
                        },
                    ],
                }
            ],
            "seat_count": 2,
            "online_count": 1,
            "offline_count": 1,
            "unseated_node_ids": [],
        }
        script = tmp_path / "render_seats.js"
        script.write_text(
            _render_seats_js(payload, "host-a-seat-0._ozma._udp.local.")
        )

        completed = subprocess.run(
            [node, str(script)],
            capture_output=True,
            text=True,
            timeout=20,
            check=False,
        )

        assert completed.returncode == 0, completed.stderr
        rendered = json.loads(completed.stdout)
        html = rendered["html"]
        assert int(rendered["count"]) == 2
        assert "host-a" in html
        assert "1/2 online" in html
        assert "Seat 0" in html
        assert "Seat 1" in html
        assert "UDP 7331" in html
        assert "HTTP 7382" in html
        assert "1920×1080" in html
        assert "node-status-dot online" in html
        assert "node-status-dot offline" in html
        assert "active-node" in html
        assert "onclick" not in html
        assert "api_token" not in html
