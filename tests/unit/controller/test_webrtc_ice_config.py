# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Controller-side browser ICE server configuration.

The dashboard and console fetch ``/api/v1/webrtc/ice`` instead of hard-coding
``iceServers: []``. The controller parses ``OZMA_WEBRTC_ICE_SERVERS`` once at
startup and refuses malformed entries rather than silently producing a broken
browser peer connection.
"""

import json

import pytest

from config import Config, parse_ice_servers

pytestmark = pytest.mark.unit


def test_empty_value_keeps_historical_empty_ice_servers():
    assert parse_ice_servers("") == []
    assert parse_ice_servers("   ") == []


def test_stun_and_turn_entries_are_returned_for_the_browser():
    raw = json.dumps(
        [
            {"urls": "stun:stun.example.org:3478"},
            {
                "urls": ["turn:turn.example.org:3478?transport=udp"],
                "username": "alice",
                "credential": "s3cret",
            },
        ]
    )
    servers = parse_ice_servers(raw)
    assert servers[0] == {"urls": "stun:stun.example.org:3478"}
    assert servers[1]["username"] == "alice"
    assert servers[1]["credential"] == "s3cret"
    assert servers[1]["urls"] == ["turn:turn.example.org:3478?transport=udp"]


@pytest.mark.parametrize(
    "value",
    [
        "not-json",
        "{}",
        '[{"urls": ""}]',
        '[{"urls": "http://example.org"}]',
        '[{"username": "alice", "credential": "s3cret"}]',
        '[{"urls": "turn:example.org:3478", "username": "alice"}]',
    ],
)
def test_malformed_values_are_rejected(value):
    with pytest.raises(ValueError):
        parse_ice_servers(value)


def test_config_reads_the_environment(monkeypatch):
    raw = '[{"urls": "stun:stun.example.org:3478"}]'
    monkeypatch.setenv("OZMA_WEBRTC_ICE_SERVERS", raw)
    config = Config.from_env()
    assert config.webrtc_ice_servers == raw
    assert parse_ice_servers(config.webrtc_ice_servers) == [
        {"urls": "stun:stun.example.org:3478"}
    ]


def test_config_defaults_to_none_configured(monkeypatch):
    monkeypatch.delenv("OZMA_WEBRTC_ICE_SERVERS", raising=False)
    assert Config.from_env().webrtc_ice_servers == ""


def _ice_app(tmp_path, servers):
    from api import build_app
    from auth import AuthConfig
    from scenarios import ScenarioManager
    from state import AppState
    from stream import StreamManager

    state = AppState()
    streams = StreamManager(state, static_dir=tmp_path / "streams")
    scenarios = ScenarioManager(tmp_path / "scenarios.json", state)
    return build_app(
        state,
        scenarios,
        streams=streams,
        auth_config=AuthConfig(enabled=False),
        webrtc_ice_servers=servers,
    )


def test_browser_ice_endpoint_returns_configured_servers(tmp_path):
    from fastapi.testclient import TestClient

    configured = [
        {"urls": "stun:stun.example.org:3478"},
        {
            "urls": "turn:turn.example.org:3478",
            "username": "alice",
            "credential": "s3cret",
        },
    ]
    client = TestClient(_ice_app(tmp_path, configured))
    response = client.get("/api/v1/webrtc/ice")
    assert response.status_code == 200
    assert response.json() == {"iceServers": configured}


def test_browser_ice_endpoint_defaults_to_empty(tmp_path):
    from fastapi.testclient import TestClient

    client = TestClient(_ice_app(tmp_path, []))
    response = client.get("/api/v1/webrtc/ice")
    assert response.status_code == 200
    assert response.json() == {"iceServers": []}
