# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Tests for agent.multiseat.monitoring — management API and Prometheus metrics."""

from __future__ import annotations

import json
from unittest.mock import AsyncMock, MagicMock, PropertyMock, patch

import pytest

from agent.multiseat.monitoring import (
    MonitoringServer, _build_prometheus_metrics, _prom_escape, _gauge, _gauge_line,
)
from agent.multiseat.gpu_inventory import GPUInventory, GPUInfo, EncoderInfo
from agent.multiseat.encoder_allocator import EncoderAllocator, EncoderSession, EncoderHints
from agent.multiseat.seat import Seat


# ── Helpers ──────────────────────────────────────────────────────────────────

def _make_mock_manager(
    *,
    gpu_count: int = 1,
    seat_count: int = 2,
    with_encoder: bool = True,
    with_games: bool = False,
    with_hotplug: bool = False,
) -> MagicMock:
    """Create a mock SeatManager for monitoring tests."""
    manager = MagicMock()

    # GPU inventory
    inv = GPUInventory()
    inv._discovered = True
    gpu = GPUInfo(
        index=0, name="NVIDIA GeForce RTX 4070", vendor="nvidia",
        is_igpu=False, pci_slot="0000:01:00.0", vram_mb=12288,
        render_device="/dev/dri/renderD128",
        encoders=[
            EncoderInfo(name="h264_nvenc", codec="h264", gpu_index=0,
                        max_sessions=5, quality=8, latency=2),
        ],
    )
    inv._gpus = [gpu]
    inv._software_encoders = [
        EncoderInfo(name="libx264", codec="h264", gpu_index=-1,
                    max_sessions=-1, quality=9, latency=8),
    ]
    manager._gpu_inventory = inv

    # Encoder allocator
    if with_encoder:
        alloc = EncoderAllocator(inv)
        for i in range(seat_count):
            alloc.allocate(f"seat-{i}")
        manager._encoder_allocator = alloc
    else:
        manager._encoder_allocator = None

    # Seats
    seats = []
    for i in range(seat_count):
        seat = Seat(name=f"seat-{i}", seat_index=i, display_index=i,
                    udp_port=7331 + i, api_port=7382 + i)
        seats.append(seat)
    manager.seats = seats
    manager.seat_count = len(seats)

    # Game launcher
    if with_games:
        mock_launcher = MagicMock()
        mock_launcher.games = []
        mock_launcher.running_state = MagicMock(return_value=None)
        manager.game_launcher = mock_launcher
    else:
        manager.game_launcher = None

    # Hotplug
    if with_hotplug:
        mock_hotplug = MagicMock()
        mock_hotplug._known_groups = {"1-1": MagicMock()}
        mock_hotplug._pending_removals = {}
        mock_hotplug.to_dict.return_value = {
            "known_groups": 1,
            "hub_to_seat": {"1-1": "seat-0"},
            "pending_additions": 0,
            "pending_removals": {},
            "persisted_mappings": 0,
        }
        manager.hotplug = mock_hotplug
    else:
        manager.hotplug = None

    # Isolation manager
    mock_iso = MagicMock()
    mock_iso.get_context = MagicMock(return_value=None)
    mock_iso.get_available = MagicMock(return_value=["none"])
    mock_iso.to_dict = MagicMock(return_value={
        "available_backends": ["none"],
        "seats": {},
    })
    manager.isolation_manager = mock_iso

    return manager


# ── Prometheus helpers ───────────────────────────────────────────────────────

class TestPrometheusHelpers:
    def test_prom_escape(self):
        assert _prom_escape('hello') == 'hello'
        assert _prom_escape('hello"world') == 'hello\\"world'
        assert _prom_escape('hello\\world') == 'hello\\\\world'
        assert _prom_escape('hello\nworld') == 'hello\\nworld'

    def test_gauge(self):
        result = _gauge("test_metric", "A test metric", 42)
        assert "# HELP test_metric A test metric" in result
        assert "# TYPE test_metric gauge" in result
        assert "test_metric 42" in result

    def test_gauge_with_labels(self):
        result = _gauge("test_metric", "test", 1, 'label="value"')
        assert 'test_metric{label="value"} 1' in result

    def test_gauge_line(self):
        result = _gauge_line("metric", 5, 'gpu="0"')
        assert 'metric{gpu="0"} 5' in result

    def test_gauge_line_no_labels(self):
        result = _gauge_line("metric", 5)
        assert "metric 5" in result


# ── Prometheus metrics builder ───────────────────────────────────────────────

class TestBuildPrometheusMetrics:
    def test_basic_metrics(self):
        manager = _make_mock_manager()
        lines = _build_prometheus_metrics(manager)
        text = "".join(lines)

        assert "ozma_gpu_count" in text
        assert "ozma_seat_count" in text
        assert "ozma_encoder_sessions" in text

    def test_gpu_count(self):
        manager = _make_mock_manager(gpu_count=1)
        lines = _build_prometheus_metrics(manager)
        text = "".join(lines)

        assert "ozma_gpu_count" in text
        # Should report 1 GPU
        assert "ozma_gpu_count 1" in text

    def test_seat_count(self):
        manager = _make_mock_manager(seat_count=3)
        lines = _build_prometheus_metrics(manager)
        text = "".join(lines)

        assert "ozma_seat_count 3" in text

    def test_encoder_sessions(self):
        manager = _make_mock_manager(seat_count=2, with_encoder=True)
        lines = _build_prometheus_metrics(manager)
        text = "".join(lines)

        assert "ozma_encoder_sessions" in text

    def test_encoder_session_limit(self):
        manager = _make_mock_manager()
        lines = _build_prometheus_metrics(manager)
        text = "".join(lines)

        assert "ozma_encoder_session_limit" in text

    def test_seat_encoder_info(self):
        manager = _make_mock_manager(seat_count=1, with_encoder=True)
        lines = _build_prometheus_metrics(manager)
        text = "".join(lines)

        assert "ozma_seat_encoder" in text
        assert 'seat="seat-0"' in text

    def test_gpu_vram(self):
        manager = _make_mock_manager()
        lines = _build_prometheus_metrics(manager)
        text = "".join(lines)

        assert "ozma_gpu_vram_mb" in text
        assert "12288" in text

    def test_allocation_events(self):
        manager = _make_mock_manager(with_encoder=True)
        lines = _build_prometheus_metrics(manager)
        text = "".join(lines)

        assert "ozma_encoder_allocation_events_total" in text

    def test_no_encoder_allocator(self):
        manager = _make_mock_manager(with_encoder=False)
        lines = _build_prometheus_metrics(manager)
        text = "".join(lines)
        # Should still work, just report 0 sessions
        assert "ozma_seat_count" in text

    def test_with_game_launcher(self):
        manager = _make_mock_manager(with_games=True)
        lines = _build_prometheus_metrics(manager)
        text = "".join(lines)

        assert "ozma_game_library_size" in text
        assert "ozma_seat_game_running" in text

    def test_with_hotplug(self):
        manager = _make_mock_manager(with_hotplug=True)
        lines = _build_prometheus_metrics(manager)
        text = "".join(lines)

        assert "ozma_hotplug_known_groups" in text
        assert "ozma_hotplug_pending_removals" in text

    def test_metrics_are_valid_text(self):
        """All metrics should be valid Prometheus text format."""
        manager = _make_mock_manager(with_games=True, with_hotplug=True)
        lines = _build_prometheus_metrics(manager)
        text = "".join(lines)

        for line in text.strip().split("\n"):
            line = line.strip()
            if not line:
                continue
            # Lines should be either HELP, TYPE, or metric values
            assert (
                line.startswith("# HELP")
                or line.startswith("# TYPE")
                or line.startswith("ozma_")
            ), f"Invalid Prometheus line: {line}"


# ── API response formats ────────────────────────────────────────────────────

class TestAPIResponseFormats:
    """Test the monitoring HTTP server by starting it and making real requests."""

    def _start_server(self, manager):
        """Start monitoring server on a random port, return (server, port)."""
        server = MonitoringServer(manager, port=0)
        # Use port 0 to get a random free port
        server.start_sync()
        port = server._server.server_address[1]
        return server, port

    def _get(self, port, path):
        import urllib.request
        resp = urllib.request.urlopen(f"http://127.0.0.1:{port}{path}", timeout=5)
        return json.loads(resp.read())

    def test_handle_gpus(self):
        manager = _make_mock_manager()
        server, port = self._start_server(manager)
        try:
            data = self._get(port, "/api/v1/gpus")
            assert "gpus" in data
            assert len(data["gpus"]) == 1
            assert data["gpus"][0]["name"] == "NVIDIA GeForce RTX 4070"
        finally:
            server.stop_sync()

    def test_handle_encoders(self):
        manager = _make_mock_manager(seat_count=1, with_encoder=True)
        server, port = self._start_server(manager)
        try:
            data = self._get(port, "/api/v1/encoders")
            assert "allocations" in data
            assert "sessions" in data
        finally:
            server.stop_sync()

    def test_handle_encoders_no_allocator(self):
        manager = _make_mock_manager(with_encoder=False)
        server, port = self._start_server(manager)
        try:
            data = self._get(port, "/api/v1/encoders")
            assert data["allocations"] == []
        finally:
            server.stop_sync()

    def test_handle_encoder_history(self):
        manager = _make_mock_manager(with_encoder=True)
        server, port = self._start_server(manager)
        try:
            data = self._get(port, "/api/v1/encoders/history")
            assert "events" in data
        finally:
            server.stop_sync()

    def test_handle_seats(self):
        manager = _make_mock_manager(seat_count=2, with_encoder=True)
        server, port = self._start_server(manager)
        try:
            data = self._get(port, "/api/v1/seats")
            assert "seats" in data
            assert len(data["seats"]) == 2
        finally:
            server.stop_sync()

    def test_handle_health(self):
        manager = _make_mock_manager()
        server, port = self._start_server(manager)
        try:
            data = self._get(port, "/health")
            assert data["ok"] is True
        finally:
            server.stop_sync()

    def test_handle_hotplug_disabled(self):
        manager = _make_mock_manager(with_hotplug=False)
        server, port = self._start_server(manager)
        try:
            data = self._get(port, "/api/v1/hotplug")
            assert data["enabled"] is False
        finally:
            server.stop_sync()

    def test_handle_hotplug_enabled(self):
        manager = _make_mock_manager(with_hotplug=True)
        server, port = self._start_server(manager)
        try:
            data = self._get(port, "/api/v1/hotplug")
            assert data["enabled"] is True
        finally:
            server.stop_sync()

    def test_handle_games_no_launcher(self):
        manager = _make_mock_manager(with_games=False)
        server, port = self._start_server(manager)
        try:
            data = self._get(port, "/api/v1/games")
            assert data["games"] == []
        finally:
            server.stop_sync()
