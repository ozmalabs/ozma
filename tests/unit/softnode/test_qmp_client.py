"""Unit tests for the dual-socket QMP client."""

import pytest
from unittest.mock import AsyncMock, MagicMock, patch
import json

pytestmark = pytest.mark.unit


class TestQMPInputClient:
    def test_initial_state(self):
        from qmp_client import QMPInputClient
        client = QMPInputClient("/tmp/nonexistent.sock")
        assert not client.connected

    @pytest.mark.asyncio
    async def test_send_when_disconnected(self):
        from qmp_client import QMPInputClient
        client = QMPInputClient("/tmp/nonexistent.sock")
        result = await client.send_input_events([{"type": "key", "data": {}}])
        assert result is False

    @pytest.mark.asyncio
    async def test_send_empty_events(self):
        from qmp_client import QMPInputClient
        client = QMPInputClient("/tmp/nonexistent.sock")
        client._connected = True
        result = await client.send_input_events([])
        assert result is False


class TestQMPControlClient:
    def test_initial_state(self):
        from qmp_client import QMPControlClient
        client = QMPControlClient("/tmp/nonexistent.sock")
        assert not client.connected

    @pytest.mark.asyncio
    async def test_send_command_when_disconnected(self):
        from qmp_client import QMPControlClient
        client = QMPControlClient("/tmp/nonexistent.sock")
        result = await client.send_command({"execute": "query-status"})
        assert result is None

    @pytest.mark.asyncio
    async def test_screendump_when_disconnected(self):
        from qmp_client import QMPControlClient
        client = QMPControlClient("/tmp/nonexistent.sock")
        result = await client.screendump("/tmp/test.ppm")
        assert result is False

    @pytest.mark.asyncio
    async def test_system_powerdown_when_disconnected(self):
        from qmp_client import QMPControlClient
        client = QMPControlClient("/tmp/nonexistent.sock")
        result = await client.system_powerdown()
        assert result is False


class TestQMPClientUnified:
    def test_dual_mode(self):
        from qmp_client import QMPClient
        client = QMPClient("/tmp/ctrl.sock", input_socket_path="/tmp/input.sock")
        assert client._dual is True
        assert client._input is not None

    def test_single_mode(self):
        from qmp_client import QMPClient
        client = QMPClient("/tmp/ctrl.sock")
        assert client._dual is False
        assert client._input is None

    def test_connected_requires_both_in_dual(self):
        from qmp_client import QMPClient
        client = QMPClient("/tmp/ctrl.sock", input_socket_path="/tmp/input.sock")
        # Neither connected
        assert not client.connected
        # Only ctrl
        client._ctrl._connected = True
        assert not client.connected
        # Both
        client._input._connected = True
        assert client.connected

    def test_connected_single_mode(self):
        from qmp_client import QMPClient
        client = QMPClient("/tmp/ctrl.sock")
        assert not client.connected
        client._ctrl._connected = True
        assert client.connected

    @pytest.mark.asyncio
    async def test_send_input_dual_mode(self):
        from qmp_client import QMPClient
        client = QMPClient("/tmp/ctrl.sock", input_socket_path="/tmp/input.sock")
        # Mock the input client
        client._input.send_input_events = AsyncMock(return_value=True)
        client._input._connected = True

        events = [{"type": "key", "data": {"key": {"type": "qcode", "data": "a"}, "down": True}}]
        result = await client.send_input_events(events)
        assert result is True
        client._input.send_input_events.assert_called_once_with(events)

    @pytest.mark.asyncio
    async def test_send_input_single_mode_falls_back(self):
        from qmp_client import QMPClient
        client = QMPClient("/tmp/ctrl.sock")
        # Mock control client
        client._ctrl.send_command = AsyncMock(return_value={"return": {}})
        client._ctrl._connected = True

        events = [{"type": "key", "data": {}}]
        result = await client.send_input_events(events)
        assert result is True
        client._ctrl.send_command.assert_called_once()


class TestQMPBackoff:
    def test_backoff_constants(self):
        from qmp_client import _BACKOFF_INITIAL, _BACKOFF_MAX, _BACKOFF_FACTOR
        assert _BACKOFF_INITIAL == 0.5
        assert _BACKOFF_MAX == 5.0
        assert _BACKOFF_FACTOR == 2.0
        # Verify exponential sequence caps
        b = _BACKOFF_INITIAL
        for _ in range(10):
            b = min(b * _BACKOFF_FACTOR, _BACKOFF_MAX)
        assert b == _BACKOFF_MAX
