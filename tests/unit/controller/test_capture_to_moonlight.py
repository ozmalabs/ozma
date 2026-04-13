# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Unit tests for controller/gaming/capture_to_moonlight.py."""

from __future__ import annotations

import asyncio
import sys
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

sys.path.insert(0, str(Path(__file__).parents[3] / "controller"))

from gaming.capture_to_moonlight import (
    CaptureSession,
    CaptureToMoonlightManager,
    MAX_CONCURRENT_SESSIONS,
    create_capture_to_moonlight,
)
from gaming.gstreamer_pipeline import PipelineConfig


def run(coro):
    return asyncio.run(coro)


# ---------------------------------------------------------------------------
# CaptureSession
# ---------------------------------------------------------------------------

class TestCaptureSession:
    def test_duration_when_not_ended(self):
        session = CaptureSession(
            capture_source_id="hdmi-0",
            display_source=MagicMock(),
            capture_card=MagicMock(),
            active=True,
            started_at=100.0,
        )
        assert session.duration is None

    def test_duration_when_ended(self):
        session = CaptureSession(
            capture_source_id="hdmi-0",
            display_source=MagicMock(),
            capture_card=MagicMock(),
            active=False,
            started_at=100.0,
            _ended_at=150.0,
        )
        assert session.duration == 50.0

    def test_ended_at_property(self):
        session = CaptureSession(
            capture_source_id="hdmi-0",
            display_source=MagicMock(),
            capture_card=MagicMock(),
            active=False,
            started_at=100.0,
            _ended_at=150.0,
        )
        assert session.ended_at == 150.0


# ---------------------------------------------------------------------------
# CaptureToMoonlightManager - initialization
# ---------------------------------------------------------------------------

class TestCaptureToMoonlightInit:
    def test_create_capture_to_moonlight_factory(self, tmp_path):
        display_capture = MagicMock()
        moonlight_protocol = MagicMock()

        manager = create_capture_to_moonlight(display_capture, moonlight_protocol, tmp_path)

        assert manager._display_capture == display_capture
        assert manager._moonlight == moonlight_protocol
        assert manager._data_dir == tmp_path
        assert manager._running is False


# ---------------------------------------------------------------------------
# CaptureToMoonlightManager - validation
# ---------------------------------------------------------------------------

class TestPipelineConfigValidation:
    def test_valid_config(self):
        config = PipelineConfig(
            name="test",
            input_width=1920,
            input_height=1080,
            input_framerate=60,
            bitrate_kbps=10000,
        )
        is_valid, error = config.validate()
        assert is_valid is True
        assert error is None

    def test_invalid_width_too_small(self):
        config = PipelineConfig(
            input_width=100,
            input_height=1080,
            input_framerate=60,
            bitrate_kbps=10000,
        )
        is_valid, error = config.validate()
        assert is_valid is False
        assert "Invalid input width" in error

    def test_invalid_width_too_large(self):
        config = PipelineConfig(
            input_width=8000,
            input_height=1080,
            input_framerate=60,
            bitrate_kbps=10000,
        )
        is_valid, error = config.validate()
        assert is_valid is False
        assert "Invalid input width" in error

    def test_invalid_height_too_small(self):
        config = PipelineConfig(
            input_width=1920,
            input_height=100,
            input_framerate=60,
            bitrate_kbps=10000,
        )
        is_valid, error = config.validate()
        assert is_valid is False
        assert "Invalid input height" in error

    def test_invalid_framerate_too_low(self):
        config = PipelineConfig(
            input_width=1920,
            input_height=1080,
            input_framerate=0,
            bitrate_kbps=10000,
        )
        is_valid, error = config.validate()
        assert is_valid is False
        assert "Invalid framerate" in error

    def test_invalid_framerate_too_high(self):
        config = PipelineConfig(
            input_width=1920,
            input_height=1080,
            input_framerate=300,
            bitrate_kbps=10000,
        )
        is_valid, error = config.validate()
        assert is_valid is False
        assert "Invalid framerate" in error

    def test_invalid_bitrate_too_low(self):
        config = PipelineConfig(
            input_width=1920,
            input_height=1080,
            input_framerate=60,
            bitrate_kbps=500,
        )
        is_valid, error = config.validate()
        assert is_valid is False
        assert "Invalid bitrate" in error

    def test_invalid_bitrate_too_high(self):
        config = PipelineConfig(
            input_width=1920,
            input_height=1080,
            input_framerate=60,
            bitrate_kbps=200000,
        )
        is_valid, error = config.validate()
        assert is_valid is False
        assert "Invalid bitrate" in error

    def test_invalid_encoder(self):
        config = PipelineConfig(
            input_width=1920,
            input_height=1080,
            input_framerate=60,
            bitrate_kbps=10000,
            encoder="invalid",
        )
        is_valid, error = config.validate()
        assert is_valid is False
        assert "Invalid encoder" in error

    def test_invalid_codec(self):
        config = PipelineConfig(
            input_width=1920,
            input_height=1080,
            input_framerate=60,
            bitrate_kbps=10000,
            codec="invalid",
        )
        is_valid, error = config.validate()
        assert is_valid is False
        assert "Invalid codec" in error

    def test_invalid_fec_percentage(self):
        config = PipelineConfig(
            input_width=1920,
            input_height=1080,
            input_framerate=60,
            bitrate_kbps=10000,
            enable_fec=True,
            fec_percentage=60,
        )
        is_valid, error = config.validate()
        assert is_valid is False
        assert "Invalid FEC percentage" in error


# ---------------------------------------------------------------------------
# CaptureToMoonlightManager - start/stop
# ---------------------------------------------------------------------------

class TestStartStop:
    def _make_manager(self, tmp_path):
        display_capture = MagicMock()
        moonlight_protocol = MagicMock()
        return CaptureToMoonlightManager(display_capture, moonlight_protocol, tmp_path)

    async def test_start_sets_running(self, tmp_path):
        manager = self._make_manager(tmp_path)
        await manager.start()
        assert manager._running is True

    async def test_stop_sets_not_running(self, tmp_path):
        manager = self._make_manager(tmp_path)
        await manager.start()
        await manager.stop()
        assert manager._running is False

    async def test_stop_cleans_up_sessions(self, tmp_path):
        manager = self._make_manager(tmp_path)
        await manager.start()

        # Mock a session with proper async methods
        mock_session = MagicMock()
        mock_session.capture_source_id = "hdmi-0"

        # Mock the input handler
        mock_input_handler = MagicMock()
        mock_input_handler.stop = AsyncMock()
        mock_session.input_handler = mock_input_handler

        # Mock the pipeline
        mock_pipeline = MagicMock()
        mock_pipeline.stop = AsyncMock()
        mock_session.pipeline = mock_pipeline

        # Mock the moonlight session
        mock_moonlight_session = MagicMock()
        mock_moonlight_session.session_id = "session-1"
        mock_session.moonlight_session = mock_moonlight_session

        manager._capture_sessions["hdmi-0"] = mock_session

        # Mock the moonlight protocol end_session method
        manager._moonlight.end_session = AsyncMock()

        # Mock the pipeline manager remove_pipeline method
        manager._pipeline_manager.remove_pipeline = AsyncMock()
        manager._pipeline_manager.stop_all = AsyncMock()

        await manager.stop()
        assert len(manager._capture_sessions) == 0


# ---------------------------------------------------------------------------
# CaptureToMoonlightManager - list_moonlight_apps
# ---------------------------------------------------------------------------

class TestListMoonlightApps:
    def _make_manager(self, tmp_path):
        display_capture = MagicMock()
        moonlight_protocol = MagicMock()
        return CaptureToMoonlightManager(display_capture, moonlight_protocol, tmp_path)

    async def test_lists_capture_cards(self, tmp_path):
        manager = self._make_manager(tmp_path)
        await manager.start()

        # Mock display capture with a capture card
        mock_card = MagicMock()
        mock_card.path = "/dev/video0"
        mock_card.name = "HDMI Capture Card"
        mock_card.resolutions = []
        mock_card.max_width = 1920
        mock_card.max_height = 1080
        mock_card.max_fps = 60

        mock_display_source = MagicMock()
        mock_display_source.card = mock_card
        mock_display_source.id = "hdmi-0"

        with patch("gaming.capture_to_moonlight.Path") as mock_path_cls:
            mock_path_cls.return_value.exists.return_value = True
            with patch.object(manager._display_capture, 'get_sources', return_value={
                "hdmi-0": mock_display_source,
            }):
                with patch.object(manager._display_capture, 'get_source', return_value=mock_display_source):
                    apps = await manager.list_moonlight_apps()

                    assert len(apps) == 1
                    assert apps[0]["id"] == "capture:hdmi-0"
                    assert apps[0]["name"] == "HDMI Capture: HDMI Capture Card"
                    assert apps[0]["capture_source_id"] == "hdmi-0"
                    assert apps[0]["max_sessions"] == MAX_CONCURRENT_SESSIONS

    async def test_excludes_missing_device(self, tmp_path):
        manager = self._make_manager(tmp_path)
        await manager.start()

        # Mock display capture with a card but missing device
        mock_card = MagicMock()
        mock_card.path = "/dev/video0"
        mock_card.name = "Missing Card"

        mock_display_source = MagicMock()
        mock_display_source.card = mock_card

        with patch.object(manager._display_capture, 'get_sources', return_value={
            "hdmi-0": mock_display_source,
        }):
            with patch.object(manager._display_capture, 'get_source', return_value=mock_display_source):
                apps = await manager.list_moonlight_apps()

                # Should not include cards with missing device files
                assert len(apps) == 0

    async def test_respects_max_sessions(self, tmp_path):
        manager = self._make_manager(tmp_path)
        await manager.start()

        mock_card = MagicMock()
        mock_card.path = "/dev/video0"
        mock_card.name = "HDMI Card"
        mock_card.resolutions = []
        mock_card.max_width = 1920
        mock_card.max_height = 1080
        mock_card.max_fps = 60

        mock_display_source = MagicMock()
        mock_display_source.card = mock_card

        with patch.object(manager._display_capture, 'get_sources', return_value={
            "hdmi-0": mock_display_source,
        }):
            with patch.object(manager._display_capture, 'get_source', return_value=mock_display_source):
                # Create max sessions
                for i in range(MAX_CONCURRENT_SESSIONS):
                    session = MagicMock()
                    session.capture_source_id = "hdmi-0"
                    session.active = True
                    session.clients = [f"client-{i}"]
                    manager._capture_sessions[f"session-{i}"] = session

                apps = await manager.list_moonlight_apps()
                assert len(apps) == 0


# ---------------------------------------------------------------------------
# CaptureToMoonlightManager - launch_moonlight_app
# ---------------------------------------------------------------------------

class TestLaunchMoonlightApp:
    def _make_manager(self, tmp_path):
        display_capture = MagicMock()
        moonlight_protocol = MagicMock()
        return CaptureToMoonlightManager(display_capture, moonlight_protocol, tmp_path)

    async def test_launch_with_valid_app_id(self, tmp_path):
        manager = self._make_manager(tmp_path)
        await manager.start()

        mock_card = MagicMock()
        mock_card.path = "/dev/video0"
        mock_card.name = "HDMI Card"
        mock_card.resolutions = []
        mock_card.max_width = 1920
        mock_card.max_height = 1080
        mock_card.max_fps = 60

        mock_display_source = MagicMock()
        mock_display_source.card = mock_card
        mock_display_source.id = "hdmi-0"

        # Mock moonlight protocol methods
        mock_session = MagicMock()
        mock_session.session_id = "session-1"
        mock_session.stream_port = 47984
        mock_session.control_port = 47985
        manager._moonlight.create_session = AsyncMock(return_value=mock_session)
        manager._moonlight.register_input_handler = AsyncMock()

        # Mock pipeline manager — return a plain MagicMock so set_on_error/set_on_stats are sync
        mock_pipeline = MagicMock()
        mock_pipeline._running = True
        manager._pipeline_manager.create_pipeline = AsyncMock(return_value=mock_pipeline)

        with patch("gaming.capture_to_moonlight.Path") as mock_path_cls:
            mock_path_cls.return_value.exists.return_value = True
            with patch("gaming.capture_to_moonlight.MoonlightInputHandler") as mock_input_cls:
                mock_input = AsyncMock()
                mock_input.start = AsyncMock()
                mock_input_cls.return_value = mock_input
                with patch.object(manager._display_capture, 'get_sources', return_value={
                    "hdmi-0": mock_display_source,
                }):
                    with patch.object(manager._display_capture, 'get_source', return_value=mock_display_source):
                        result = await manager.launch_moonlight_app("capture:hdmi-0", "client-1")
                        # Session was created
                        assert result is True

    async def test_launch_with_invalid_app_id(self, tmp_path):
        manager = self._make_manager(tmp_path)
        await manager.start()

        result = await manager.launch_moonlight_app("invalid:app", "client-1")
        assert result is False

    async def test_launch_exceeds_max_sessions(self, tmp_path):
        manager = self._make_manager(tmp_path)
        await manager.start()

        mock_card = MagicMock()
        mock_card.path = "/dev/video0"
        mock_card.name = "HDMI Card"
        mock_card.resolutions = []
        mock_card.max_width = 1920
        mock_card.max_height = 1080
        mock_card.max_fps = 60

        mock_display_source = MagicMock()
        mock_display_source.card = mock_card

        with patch.object(manager._display_capture, 'get_sources', return_value={
            "hdmi-0": mock_display_source,
        }):
            with patch.object(manager._display_capture, 'get_source', return_value=mock_display_source):
                # Create max sessions
                for i in range(MAX_CONCURRENT_SESSIONS):
                    session = MagicMock()
                    session.capture_source_id = "hdmi-0"
                    session.active = True
                    session.clients = [f"client-{i}"]
                    manager._capture_sessions[f"session-{i}"] = session

                result = await manager.launch_moonlight_app("capture:hdmi-0", "client-new")
                assert result is False


# ---------------------------------------------------------------------------
# CaptureToMoonlightManager - update_pipeline_config
# ---------------------------------------------------------------------------

class TestUpdatePipelineConfig:
    def _make_manager(self, tmp_path):
        display_capture = MagicMock()
        moonlight_protocol = MagicMock()
        return CaptureToMoonlightManager(display_capture, moonlight_protocol, tmp_path)

    async def test_update_with_valid_config(self, tmp_path):
        manager = self._make_manager(tmp_path)
        await manager.start()

        mock_card = MagicMock()
        mock_card.path = "/dev/video0"
        mock_card.name = "HDMI Card"
        mock_card.resolutions = []
        mock_card.max_width = 1920
        mock_card.max_height = 1080
        mock_card.max_fps = 60

        mock_display_source = MagicMock()
        mock_display_source.card = mock_card
        mock_display_source.id = "hdmi-0"

        # Mock session with pipeline
        mock_session = MagicMock()
        mock_session.capture_source_id = "hdmi-0"
        mock_session.active = True
        mock_session.pipeline = MagicMock()
        mock_session.pipeline.restart = AsyncMock(return_value=True)
        mock_session.pipeline_config = PipelineConfig(
            name="capture-hdmi-0",
            input_width=1920,
            input_height=1080,
            input_framerate=60,
            bitrate_kbps=10000,
        )
        mock_session.moonlight_session = MagicMock()
        mock_session.moonlight_session.session_id = "session-1"

        manager._capture_sessions["hdmi-0"] = mock_session

        result = await manager.update_pipeline_config("hdmi-0", bitrate_kbps=20000)
        assert result is True
        assert mock_session.pipeline_config.bitrate_kbps == 20000

    async def test_update_with_invalid_config(self, tmp_path):
        manager = self._make_manager(tmp_path)
        await manager.start()

        mock_session = MagicMock()
        mock_session.capture_source_id = "hdmi-0"
        mock_session.pipeline_config = PipelineConfig(
            name="capture-hdmi-0",
            input_width=1920,
            input_height=1080,
            input_framerate=60,
            bitrate_kbps=10000,
        )
        manager._capture_sessions["hdmi-0"] = mock_session

        # Try to update with invalid bitrate
        result = await manager.update_pipeline_config("hdmi-0", bitrate_kbps=500)
        assert result is False

    async def test_update_nonexistent_session(self, tmp_path):
        manager = self._make_manager(tmp_path)
        await manager.start()

        result = await manager.update_pipeline_config("nonexistent", bitrate_kbps=20000)
        assert result is False


# ---------------------------------------------------------------------------
# CaptureToMoonlightManager - start_capture_session
# ---------------------------------------------------------------------------

class TestStartCaptureSession:
    def _make_manager(self, tmp_path):
        display_capture = MagicMock()
        moonlight_protocol = MagicMock()
        return CaptureToMoonlightManager(display_capture, moonlight_protocol, tmp_path)

    async def test_start_with_invalid_capture_source(self, tmp_path):
        manager = self._make_manager(tmp_path)
        await manager.start()

        result = await manager.start_capture_session("invalid-source", "client-1")
        assert result is None

    async def test_start_with_invalid_client_id(self, tmp_path):
        manager = self._make_manager(tmp_path)
        await manager.start()

        mock_card = MagicMock()
        mock_card.path = "/dev/video0"
        mock_card.name = "HDMI Card"
        mock_card.resolutions = []
        mock_card.max_width = 1920
        mock_card.max_height = 1080
        mock_card.max_fps = 60

        mock_display_source = MagicMock()
        mock_display_source.card = mock_card

        with patch.object(manager._display_capture, 'get_sources', return_value={
            "hdmi-0": mock_display_source,
        }):
            with patch.object(manager._display_capture, 'get_source', return_value=mock_display_source):
                result = await manager.start_capture_session("hdmi-0", "")
                assert result is None

    async def test_start_with_missing_device(self, tmp_path):
        manager = self._make_manager(tmp_path)
        await manager.start()

        mock_card = MagicMock()
        mock_card.path = "/dev/video0"
        mock_card.name = "Missing Card"

        mock_display_source = MagicMock()
        mock_display_source.card = mock_card

        with patch.object(manager._display_capture, 'get_sources', return_value={
            "hdmi-0": mock_display_source,
        }):
            with patch.object(manager._display_capture, 'get_source', return_value=mock_display_source):
                result = await manager.start_capture_session("hdmi-0", "client-1")
                assert result is None
