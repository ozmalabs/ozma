# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Tests for agent.multiseat.webrtc_seat — per-seat WebRTC streaming."""

from __future__ import annotations

import asyncio
import json
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from agent.multiseat.seat import Seat
from agent.multiseat.display_backend import DisplayInfo


# ── Import guard ─────────────────────────────────────────────────────────────

class TestImportGuard:
    def test_module_importable(self):
        """webrtc_seat module should import even without aiortc."""
        from agent.multiseat.webrtc_seat import SeatWebRTCHandler
        assert SeatWebRTCHandler is not None

    def test_check_aiortc_caches_result(self):
        from agent.multiseat import webrtc_seat
        # Reset cache
        webrtc_seat._AIORTC_AVAILABLE = None
        with patch.dict("sys.modules", {"aiortc": None}):
            # Force ImportError
            with patch("builtins.__import__", side_effect=ImportError):
                result1 = webrtc_seat._check_aiortc()
        assert result1 is False
        # Cache should persist
        assert webrtc_seat._AIORTC_AVAILABLE is False
        # Reset for other tests
        webrtc_seat._AIORTC_AVAILABLE = None


# ── SeatWebRTCHandler ────────────────────────────────────────────────────────

class TestSeatWebRTCHandler:
    def _make_seat(self) -> Seat:
        seat = Seat(
            name="test-seat-0",
            seat_index=0,
            display_index=0,
            udp_port=7331,
            api_port=7382,
            capture_fps=30,
        )
        seat.display = DisplayInfo(
            index=0, name="HDMI-1", width=1920, height=1080,
        )
        seat.audio_sink = "ozma-test-seat-0"
        return seat

    def test_creation(self):
        from agent.multiseat.webrtc_seat import SeatWebRTCHandler
        seat = self._make_seat()
        handler = SeatWebRTCHandler(seat)
        assert handler.peer_count == 0
        assert handler._target_bitrate == 4_000_000

    def test_to_dict(self):
        from agent.multiseat.webrtc_seat import SeatWebRTCHandler
        seat = self._make_seat()
        handler = SeatWebRTCHandler(seat)
        d = handler.to_dict()
        assert "available" in d
        assert d["peers"] == 0
        assert d["target_bitrate"] == 4_000_000
        assert d["video_tracks"] == 0
        assert d["audio_tracks"] == 0

    def test_add_routes(self):
        from agent.multiseat.webrtc_seat import SeatWebRTCHandler
        from aiohttp import web

        seat = self._make_seat()
        handler = SeatWebRTCHandler(seat)
        app = web.Application()
        handler.add_routes(app)

        # Should have added two routes
        routes = [r.resource.canonical for r in app.router.routes()
                  if hasattr(r, 'resource')]
        assert "/webrtc/offer" in routes
        assert "/webrtc/bitrate" in routes

    @pytest.mark.asyncio
    async def test_handle_offer_no_aiortc(self):
        """When aiortc is not available, return 503."""
        from agent.multiseat.webrtc_seat import SeatWebRTCHandler
        import agent.multiseat.webrtc_seat as mod

        seat = self._make_seat()
        handler = SeatWebRTCHandler(seat)

        # Force aiortc unavailable
        original = mod._AIORTC_AVAILABLE
        mod._AIORTC_AVAILABLE = False

        mock_request = MagicMock()
        mock_request.json = AsyncMock(return_value={
            "sdp": "test", "type": "offer",
        })

        try:
            resp = await handler.handle_offer(mock_request)
            assert resp.status == 503
            data = json.loads(resp.body)
            assert "aiortc" in data["error"].lower()
        finally:
            mod._AIORTC_AVAILABLE = original

    @pytest.mark.asyncio
    async def test_handle_offer_missing_sdp(self):
        """Missing SDP in request should return 400."""
        from agent.multiseat.webrtc_seat import SeatWebRTCHandler
        import agent.multiseat.webrtc_seat as mod

        seat = self._make_seat()
        handler = SeatWebRTCHandler(seat)

        # Mock aiortc as available
        original = mod._AIORTC_AVAILABLE
        mod._AIORTC_AVAILABLE = True

        mock_request = MagicMock()
        mock_request.json = AsyncMock(return_value={"type": "offer"})

        try:
            resp = await handler.handle_offer(mock_request)
            assert resp.status == 400
            data = json.loads(resp.body)
            assert "sdp" in data["error"].lower()
        finally:
            mod._AIORTC_AVAILABLE = original

    @pytest.mark.asyncio
    async def test_handle_offer_invalid_json(self):
        """Invalid JSON body should return 400."""
        from agent.multiseat.webrtc_seat import SeatWebRTCHandler
        import agent.multiseat.webrtc_seat as mod

        seat = self._make_seat()
        handler = SeatWebRTCHandler(seat)

        original = mod._AIORTC_AVAILABLE
        mod._AIORTC_AVAILABLE = True

        mock_request = MagicMock()
        mock_request.json = AsyncMock(side_effect=json.JSONDecodeError("", "", 0))

        try:
            resp = await handler.handle_offer(mock_request)
            assert resp.status == 400
        finally:
            mod._AIORTC_AVAILABLE = original

    @pytest.mark.asyncio
    async def test_handle_bitrate(self):
        """Bitrate endpoint should clamp and store the value."""
        from agent.multiseat.webrtc_seat import SeatWebRTCHandler

        seat = self._make_seat()
        handler = SeatWebRTCHandler(seat)

        mock_request = MagicMock()
        mock_request.json = AsyncMock(return_value={"bitrate": 8_000_000})

        resp = await handler.handle_bitrate(mock_request)
        data = json.loads(resp.body)
        assert data["ok"] is True
        assert data["bitrate"] == 8_000_000
        assert handler._target_bitrate == 8_000_000

    @pytest.mark.asyncio
    async def test_handle_bitrate_clamping(self):
        """Bitrate should be clamped to [500k, 50M]."""
        from agent.multiseat.webrtc_seat import SeatWebRTCHandler

        seat = self._make_seat()
        handler = SeatWebRTCHandler(seat)

        # Too low
        mock_request = MagicMock()
        mock_request.json = AsyncMock(return_value={"bitrate": 100})
        resp = await handler.handle_bitrate(mock_request)
        data = json.loads(resp.body)
        assert data["bitrate"] == 500_000

        # Too high
        mock_request.json = AsyncMock(return_value={"bitrate": 999_999_999})
        resp = await handler.handle_bitrate(mock_request)
        data = json.loads(resp.body)
        assert data["bitrate"] == 50_000_000

    @pytest.mark.asyncio
    async def test_handle_bitrate_invalid_json(self):
        from agent.multiseat.webrtc_seat import SeatWebRTCHandler

        seat = self._make_seat()
        handler = SeatWebRTCHandler(seat)

        mock_request = MagicMock()
        mock_request.json = AsyncMock(side_effect=ValueError)

        resp = await handler.handle_bitrate(mock_request)
        assert resp.status == 400

    @pytest.mark.asyncio
    async def test_cleanup_empty(self):
        """Cleanup with no peers should be a no-op."""
        from agent.multiseat.webrtc_seat import SeatWebRTCHandler

        seat = self._make_seat()
        handler = SeatWebRTCHandler(seat)
        await handler.cleanup()  # should not raise
        assert handler.peer_count == 0

    @pytest.mark.asyncio
    async def test_cleanup_closes_peers(self):
        from agent.multiseat.webrtc_seat import SeatWebRTCHandler

        seat = self._make_seat()
        handler = SeatWebRTCHandler(seat)

        # Mock peer connections
        pc1 = AsyncMock()
        pc2 = AsyncMock()
        handler._pcs = [pc1, pc2]

        await handler.cleanup()
        pc1.close.assert_called_once()
        pc2.close.assert_called_once()
        assert handler.peer_count == 0

    @pytest.mark.asyncio
    async def test_cleanup_stops_tracks(self):
        from agent.multiseat.webrtc_seat import SeatWebRTCHandler, SeatVideoTrack, SeatAudioTrack

        seat = self._make_seat()
        handler = SeatWebRTCHandler(seat)

        mock_video = MagicMock(spec=SeatVideoTrack)
        mock_audio = MagicMock(spec=SeatAudioTrack)
        handler._video_tracks = [mock_video]
        handler._audio_tracks = [mock_audio]

        await handler.cleanup()
        mock_video.stop.assert_called_once()
        mock_audio.stop.assert_called_once()


# ── SeatVideoTrack ───────────────────────────────────────────────────────────

class TestSeatVideoTrack:
    def test_creation(self):
        from agent.multiseat.webrtc_seat import SeatVideoTrack
        seat = Seat(name="s0", seat_index=0, display_index=0,
                    udp_port=7331, api_port=7382, capture_fps=30)
        track = SeatVideoTrack(seat, fps=30)
        assert track.kind == "video"
        assert track._fps == 30

    def test_get_capture_region_with_display(self):
        from agent.multiseat.webrtc_seat import SeatVideoTrack
        seat = Seat(name="s0", seat_index=0, display_index=0,
                    udp_port=7331, api_port=7382)
        seat.display = DisplayInfo(
            index=0, name="DP-1", width=2560, height=1440,
            x_offset=1920, y_offset=0,
        )
        track = SeatVideoTrack(seat)
        w, h, x, y, _ = track._get_capture_region()
        assert w == 2560
        assert h == 1440
        assert x == 1920
        assert y == 0

    def test_get_capture_region_no_display(self):
        from agent.multiseat.webrtc_seat import SeatVideoTrack
        seat = Seat(name="s0", seat_index=0, display_index=0,
                    udp_port=7331, api_port=7382,
                    capture_width=3840, capture_height=2160)
        track = SeatVideoTrack(seat)
        w, h, x, y, _ = track._get_capture_region()
        assert w == 3840
        assert h == 2160
        assert x == 0
        assert y == 0

    def test_stop(self):
        from agent.multiseat.webrtc_seat import SeatVideoTrack
        seat = Seat(name="s0", seat_index=0, display_index=0,
                    udp_port=7331, api_port=7382)
        track = SeatVideoTrack(seat)

        mock_proc = MagicMock()
        track._capture_proc = mock_proc

        track.stop()
        assert track._stopped is True
        mock_proc.terminate.assert_called_once()


# ── SeatAudioTrack ───────────────────────────────────────────────────────────

class TestSeatAudioTrack:
    def test_creation(self):
        from agent.multiseat.webrtc_seat import SeatAudioTrack
        seat = Seat(name="s0", seat_index=0, display_index=0,
                    udp_port=7331, api_port=7382)
        seat.audio_sink = "ozma-s0"
        track = SeatAudioTrack(seat)
        assert track.kind == "audio"
        assert track._rate == 48000
        assert track._channels == 2

    def test_get_monitor_source_with_sink(self):
        from agent.multiseat.webrtc_seat import SeatAudioTrack
        seat = Seat(name="s0", seat_index=0, display_index=0,
                    udp_port=7331, api_port=7382)
        seat.audio_sink = "ozma-s0"
        track = SeatAudioTrack(seat)
        assert track._get_monitor_source() == "ozma-s0.monitor"

    def test_get_monitor_source_no_sink(self):
        from agent.multiseat.webrtc_seat import SeatAudioTrack
        seat = Seat(name="s0", seat_index=0, display_index=0,
                    udp_port=7331, api_port=7382)
        track = SeatAudioTrack(seat)
        assert track._get_monitor_source() == "default.monitor"

    def test_stop(self):
        from agent.multiseat.webrtc_seat import SeatAudioTrack
        seat = Seat(name="s0", seat_index=0, display_index=0,
                    udp_port=7331, api_port=7382)
        track = SeatAudioTrack(seat)

        mock_proc = MagicMock()
        track._proc = mock_proc

        track.stop()
        assert track._stopped is True
        mock_proc.terminate.assert_called_once()


# ── Seat integration ─────────────────────────────────────────────────────────

class TestSeatWebRTCIntegration:
    def test_seat_init_webrtc_graceful(self):
        """_init_webrtc should not raise even if webrtc_seat imports fail."""
        seat = Seat(name="s0", seat_index=0, display_index=0,
                    udp_port=7331, api_port=7382)
        # This should work without error
        seat._init_webrtc()

    def test_seat_to_dict_includes_webrtc(self):
        seat = Seat(name="s0", seat_index=0, display_index=0,
                    udp_port=7331, api_port=7382)
        d = seat.to_dict()
        assert "webrtc" in d

    @pytest.mark.asyncio
    async def test_seat_stop_cleans_webrtc(self):
        seat = Seat(name="s0", seat_index=0, display_index=0,
                    udp_port=7331, api_port=7382)
        mock_webrtc = AsyncMock()
        seat._webrtc = mock_webrtc

        await seat.stop()
        mock_webrtc.cleanup.assert_called_once()
