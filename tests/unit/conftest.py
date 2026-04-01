"""Unit-tier fixtures — mock AppState, mock QMP, mock PipeWire, etc.

All fixtures here work without external services.
"""

import asyncio
import json
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock

import pytest

pytestmark = pytest.mark.unit


# ── AppState + NodeInfo ───────────────────────────────────────────────────

def _make_node(**kwargs):
    """Create a NodeInfo with sensible defaults."""
    from state import NodeInfo
    defaults = dict(
        id="test-node._ozma._udp.local.",
        host="10.0.0.5",
        port=7331,
        role="compute",
        hw="virtual",
        fw_version="1.0.0",
        proto_version=1,
        capabilities=["hid", "screen", "audio"],
        audio_type="pipewire",
        audio_sink="ozma-test",
    )
    defaults.update(kwargs)
    return NodeInfo(**defaults)


@pytest.fixture
def make_node():
    """Factory fixture for creating NodeInfo objects."""
    return _make_node


@pytest.fixture
def app_state():
    """A clean AppState with no nodes."""
    from state import AppState
    return AppState()


@pytest.fixture
def populated_state():
    """AppState with two nodes pre-registered."""
    from state import AppState
    state = AppState()
    state.nodes[_make_node(id="hw-1._ozma._udp.local.", hw="rpi4").id] = \
        _make_node(id="hw-1._ozma._udp.local.", hw="rpi4")
    state.nodes[_make_node(id="sw-1._ozma._udp.local.", hw="soft").id] = \
        _make_node(id="sw-1._ozma._udp.local.", hw="soft")
    return state


# ── Mock QMP ──────────────────────────────────────────────────────────────

@pytest.fixture
def mock_qmp_writer():
    """Mock asyncio.StreamWriter for QMP socket tests."""
    writer = MagicMock()
    writer.write = MagicMock()
    writer.drain = AsyncMock()
    writer.close = MagicMock()
    writer.is_closing = MagicMock(return_value=False)
    return writer


@pytest.fixture
def mock_qmp_reader():
    """Mock asyncio.StreamReader that yields QMP greeting + OK response."""
    reader = AsyncMock()
    greeting = json.dumps({"QMP": {"version": {"qemu": {"major": 8}}}}).encode() + b"\n"
    ok = json.dumps({"return": {}}).encode() + b"\n"
    reader.readline = AsyncMock(side_effect=[greeting, ok])
    return reader


# ── Mock PipeWire ─────────────────────────────────────────────────────────

@pytest.fixture
def mock_pw_graph(data_dir):
    """Load mock PipeWire graph from fixture file."""
    path = data_dir / "mock_pw_dump.json"
    if path.exists():
        return json.loads(path.read_text())
    # Inline minimal graph if file doesn't exist yet
    return [
        {"id": 50, "type": "PipeWire:Interface:Node", "info": {
            "props": {"node.name": "ozma-vm1", "media.class": "Audio/Sink"}}},
        {"id": 51, "type": "PipeWire:Interface:Node", "info": {
            "props": {"node.name": "ozma-vm1.monitor", "media.class": "Audio/Source"}}},
    ]


# ── Mock scenarios ────────────────────────────────────────────────────────

@pytest.fixture
def mock_scenarios():
    """Two-scenario config."""
    return {
        "scenarios": [
            {"id": "vm1", "name": "VM 1", "node_id": "vm1._ozma._udp.local.", "color": "#4A90D9"},
            {"id": "vm2", "name": "VM 2", "node_id": "vm2._ozma._udp.local.", "color": "#50C878"},
        ],
        "default": "vm1",
    }


# ── Mock images ───────────────────────────────────────────────────────────

@pytest.fixture
def blank_image():
    """1024x768 black PIL Image."""
    try:
        from PIL import Image
        return Image.new("RGB", (1024, 768), (0, 0, 0))
    except ImportError:
        pytest.skip("Pillow not available")


@pytest.fixture
def gui_light_image():
    """1024x768 light-themed GUI screenshot (mostly white)."""
    try:
        from PIL import Image
        return Image.new("RGB", (1024, 768), (240, 240, 240))
    except ImportError:
        pytest.skip("Pillow not available")


# ── Config ────────────────────────────────────────────────────────────────

@pytest.fixture
def config():
    """Default Config for unit tests."""
    try:
        from config import Config
        return Config()
    except ImportError:
        return {}
