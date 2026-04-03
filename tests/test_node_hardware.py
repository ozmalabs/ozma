"""
Hardware node compatibility tests.

These tests validate that a physical node board is correctly configured and
communicates with the controller. They are skipped automatically when the
required hardware or environment variables are not present.

Usage (with hardware connected):
    # Orange Pi Zero 3 node at 192.168.1.50
    OZMA_NODE_IP=192.168.1.50 pytest tests/test_node_hardware.py -v

    # Against a running controller
    OZMA_NODE_IP=192.168.1.50 OZMA_CONTROLLER_URL=http://localhost:7380 \
        pytest tests/test_node_hardware.py -v

Environment variables:
    OZMA_NODE_IP            IP address of the physical node under test (required)
    OZMA_CONTROLLER_URL     Controller base URL (default: http://localhost:7380)
    OZMA_NODE_PORT          Node daemon port (default: 7380)
    OZMA_HW_PROFILE         Board profile: zero3|opi5 (optional, for board-specific checks)
"""

import os
import asyncio
import subprocess
import pytest
import aiohttp

# ---------------------------------------------------------------------------
# Fixtures and helpers
# ---------------------------------------------------------------------------

NODE_IP = os.environ.get("OZMA_NODE_IP")
CONTROLLER_URL = os.environ.get("OZMA_CONTROLLER_URL", "http://localhost:7380")
HW_PROFILE = os.environ.get("OZMA_HW_PROFILE", "")

requires_node = pytest.mark.skipif(
    not NODE_IP,
    reason="Set OZMA_NODE_IP to run hardware node tests",
)


@pytest.fixture
def node_ip():
    return NODE_IP


@pytest.fixture
async def controller_session():
    async with aiohttp.ClientSession() as session:
        yield session


# ---------------------------------------------------------------------------
# Connectivity
# ---------------------------------------------------------------------------


@requires_node
def test_node_reachable(node_ip):
    """Node responds to ping."""
    result = subprocess.run(
        ["ping", "-c", "3", "-W", "2", node_ip],
        capture_output=True,
    )
    assert result.returncode == 0, f"Node at {node_ip} did not respond to ping"


@requires_node
@pytest.mark.asyncio
async def test_node_registered_with_controller(controller_session):
    """Node appears in controller node list after boot."""
    async with controller_session.get(f"{CONTROLLER_URL}/api/v1/nodes") as resp:
        assert resp.status == 200
        nodes = await resp.json()

    node_ips = [n.get("ip") or n.get("address") for n in nodes]
    assert NODE_IP in node_ips, (
        f"Node {NODE_IP} not found in controller node list: {node_ips}"
    )


@requires_node
@pytest.mark.asyncio
async def test_node_status_online(controller_session):
    """Node status is 'online' in the controller."""
    async with controller_session.get(f"{CONTROLLER_URL}/api/v1/nodes") as resp:
        nodes = await resp.json()

    node = next((n for n in nodes if n.get("ip") == NODE_IP or n.get("address") == NODE_IP), None)
    assert node is not None, f"Node {NODE_IP} not found"
    assert node.get("status") == "online", f"Expected online, got: {node.get('status')}"


# ---------------------------------------------------------------------------
# HDMI capture
# ---------------------------------------------------------------------------


@requires_node
@pytest.mark.asyncio
async def test_capture_stream_available(controller_session):
    """Node exposes an HLS or MJPEG stream URL and it responds."""
    async with controller_session.get(f"{CONTROLLER_URL}/api/v1/nodes") as resp:
        nodes = await resp.json()

    node = next((n for n in nodes if n.get("ip") == NODE_IP or n.get("address") == NODE_IP), None)
    assert node is not None

    stream_url = node.get("stream_url")
    assert stream_url, "Node has no stream_url — is capture running and a source connected?"

    async with controller_session.get(stream_url) as resp:
        # HLS playlist or MJPEG boundary — either way a 200 means capture is live
        assert resp.status == 200, f"Stream at {stream_url} returned {resp.status}"


@requires_node
@pytest.mark.asyncio
async def test_capture_resolution(controller_session):
    """Captured stream reports at least 1280×720 resolution."""
    async with controller_session.get(f"{CONTROLLER_URL}/api/v1/nodes") as resp:
        nodes = await resp.json()

    node = next((n for n in nodes if n.get("ip") == NODE_IP or n.get("address") == NODE_IP), None)
    assert node is not None

    # Node metrics or info endpoint should expose capture resolution
    node_id = node.get("id") or node.get("name")
    async with controller_session.get(f"{CONTROLLER_URL}/api/v1/nodes/{node_id}") as resp:
        info = await resp.json()

    capture = info.get("capture", {})
    width = capture.get("width", 0)
    height = capture.get("height", 0)
    assert width >= 1280 and height >= 720, (
        f"Expected ≥1280×720, got {width}×{height} — "
        "check HDMI source is active and capture card is connected"
    )


# ---------------------------------------------------------------------------
# HID gadget
# ---------------------------------------------------------------------------


@requires_node
@pytest.mark.asyncio
async def test_hid_switch_to_node(controller_session):
    """Controller can switch active focus to the physical node."""
    async with controller_session.get(f"{CONTROLLER_URL}/api/v1/nodes") as resp:
        nodes = await resp.json()

    node = next((n for n in nodes if n.get("ip") == NODE_IP or n.get("address") == NODE_IP), None)
    assert node is not None
    node_id = node.get("id") or node.get("name")

    async with controller_session.post(
        f"{CONTROLLER_URL}/api/v1/scenarios/{node_id}/activate"
    ) as resp:
        assert resp.status in (200, 204), f"Activate returned {resp.status}"

    # Verify active node reflects the switch
    await asyncio.sleep(0.5)
    async with controller_session.get(f"{CONTROLLER_URL}/api/v1/state") as resp:
        state = await resp.json()

    assert state.get("active_node_id") == node_id


# ---------------------------------------------------------------------------
# Board-specific: Orange Pi Zero 3
# ---------------------------------------------------------------------------


@requires_node
@pytest.mark.skipif(HW_PROFILE != "zero3", reason="Set OZMA_HW_PROFILE=zero3")
@pytest.mark.asyncio
async def test_zero3_capture_is_1080p30(controller_session):
    """Zero 3 + MS2109 captures at 1080×1920 @ ~30fps (USB 2.0 ceiling)."""
    async with controller_session.get(f"{CONTROLLER_URL}/api/v1/nodes") as resp:
        nodes = await resp.json()

    node = next((n for n in nodes if n.get("ip") == NODE_IP or n.get("address") == NODE_IP), None)
    node_id = node.get("id") or node.get("name")

    async with controller_session.get(f"{CONTROLLER_URL}/api/v1/nodes/{node_id}") as resp:
        info = await resp.json()

    capture = info.get("capture", {})
    width = capture.get("width", 0)
    height = capture.get("height", 0)
    fps = capture.get("fps", 0)

    assert width == 1920 and height == 1080, f"Expected 1920×1080, got {width}×{height}"
    assert 25 <= fps <= 35, f"Expected ~30fps, got {fps}"


# ---------------------------------------------------------------------------
# Board-specific: Orange Pi 5 (magic dock)
# ---------------------------------------------------------------------------


@requires_node
@pytest.mark.skipif(HW_PROFILE != "opi5", reason="Set OZMA_HW_PROFILE=opi5")
@pytest.mark.asyncio
async def test_opi5_capture_is_1080p60(controller_session):
    """OPi5 + MS2130 captures at 1920×1080 @ ~60fps (USB 3.0)."""
    async with controller_session.get(f"{CONTROLLER_URL}/api/v1/nodes") as resp:
        nodes = await resp.json()

    node = next((n for n in nodes if n.get("ip") == NODE_IP or n.get("address") == NODE_IP), None)
    node_id = node.get("id") or node.get("name")

    async with controller_session.get(f"{CONTROLLER_URL}/api/v1/nodes/{node_id}") as resp:
        info = await resp.json()

    capture = info.get("capture", {})
    width = capture.get("width", 0)
    height = capture.get("height", 0)
    fps = capture.get("fps", 0)

    assert width == 1920 and height == 1080, f"Expected 1920×1080, got {width}×{height}"
    assert 55 <= fps <= 65, f"Expected ~60fps, got {fps}"


@requires_node
@pytest.mark.skipif(HW_PROFILE != "opi5", reason="Set OZMA_HW_PROFILE=opi5")
@pytest.mark.asyncio
async def test_opi5_dock_mode_laptop_registered(controller_session):
    """OPi5 magic dock: the laptop connected via USB-C appears as the node's target."""
    async with controller_session.get(f"{CONTROLLER_URL}/api/v1/nodes") as resp:
        nodes = await resp.json()

    node = next((n for n in nodes if n.get("ip") == NODE_IP or n.get("address") == NODE_IP), None)
    assert node is not None, f"OPi5 node not found at {NODE_IP}"

    # In dock mode the node's HID gadget is active (laptop has accepted the composite device)
    node_id = node.get("id") or node.get("name")
    async with controller_session.get(f"{CONTROLLER_URL}/api/v1/nodes/{node_id}") as resp:
        info = await resp.json()

    hid_status = info.get("hid_status", "unknown")
    assert hid_status == "connected", (
        f"HID gadget not connected — is the laptop plugged in via USB-C? Status: {hid_status}"
    )
