# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
HDMI capture streaming to Moonlight protocol.

Implements Tier 3: capture_to_moonlight component.

Architecture:
  HDMI capture card (V4L2)
       │
       ├──→ display_capture.py (existing capture manager)
       │     └──→ HLS/MJPEG streams
       │
       └──→ GStreamer pipeline (new)
             └──→ encode (NVENC/VAAPI/QuickSync)
                   └──→ RTP packetiser → Moonlight protocol

The capture_to_moonlight module reuses the existing display_capture.py
for V4L2 capture but routes through GStreamer for Moonlight-compatible
encoding and RTP streaming.
"""

from __future__ import annotations

import asyncio
import logging
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from display_capture import DisplayCaptureManager, DisplaySource, CaptureCard

from .gstreamer_pipeline import (
    GStreamerPipeline,
    PipelineConfig,
    PipelineManager,
)
from .moonlight_protocol import (
    MoonlightProtocol,
    SessionData,
    PairingData,
    InputReport,
)
from .moonlight_input import MoonlightInputHandler

log = logging.getLogger("ozma.moonlight.capture")


@dataclass
class CaptureSession:
    """
    A streaming session that captures HDMI and streams to Moonlight.

    Each capture source (HDMI card) can have multiple concurrent sessions
    with different Moonlight clients.
    """
    capture_source_id: str
    display_source: DisplaySource
    capture_card: CaptureCard

    # Moonlight session
    moonlight_session: SessionData | None = None
    pairing: PairingData | None = None

    # GStreamer pipeline
    pipeline: GStreamerPipeline | None = None
    pipeline_config: PipelineConfig | None = None

    # Input handler
    input_handler: MoonlightInputHandler | None = None

    # Status
    active: bool = False
    started_at: float | None = None
    clients: list[str] = field(default_factory=list)  # Client IDs connected

    @property
    def duration(self) -> float | None:
        if self.started_at and self._ended_at:
            return self._ended_at - self.started_at
        return None

    _ended_at: float | None = None


class CaptureToMoonlightManager:
    """
    Manages HDMI capture streaming to Moonlight protocol.

    Integrates display_capture.py with GStreamer pipelines and Moonlight protocol.
    Each HDMI capture card can stream to multiple Moonlight clients simultaneously.
    """

    def __init__(
        self,
        display_capture: DisplayCaptureManager,
        moonlight_protocol: MoonlightProtocol,
        data_dir: Path,
    ) -> None:
        self._display_capture = display_capture
        self._moonlight = moonlight_protocol
        self._data_dir = data_dir
        self._data_dir.mkdir(parents=True, exist_ok=True)

        self._pipeline_manager = PipelineManager(data_dir / "gstreamer")
        self._capture_sessions: dict[str, CaptureSession] = {}
        self._input_handlers: dict[str, MoonlightInputHandler] = {}

        self._running = False
        self._tasks: list[asyncio.Task] = []

    async def start(self) -> None:
        """Start the capture-to-moonlight manager."""
        self._running = True

        # Start pipeline manager
        await self._pipeline_manager.stop_all()  # Clean up any existing

        # Scan for capture cards
        await self._rescan_capture_cards()

        log.info("Capture-to-Moonlight manager started")

    async def stop(self) -> None:
        """Stop the capture-to-moonlight manager."""
        self._running = False

        # Stop all capture sessions
        for session in self._capture_sessions.values():
            await self._stop_session(session.capture_source_id)

        # Stop pipeline manager
        await self._pipeline_manager.stop_all()

        # Cancel tasks
        for task in self._tasks:
            task.cancel()
        await asyncio.gather(*self._tasks, return_exceptions=True)

    async def _rescan_capture_cards(self) -> None:
        """Scan for capture cards and register them as Moonlight apps."""
        # This will be called periodically or on device change
        # For now, we just list available sources

    async def start_capture_session(
        self,
        capture_source_id: str,
        client_id: str,
    ) -> CaptureSession | None:
        """
        Start a streaming session for a capture source to a Moonlight client.

        Creates:
          1. A Moonlight session
          2. A GStreamer pipeline
          3. Registers as a Moonlight app
        """
        # Find the capture source
        display_source = self._display_capture.get_source(capture_source_id)
        if not display_source:
            log.error("Capture source not found: %s", capture_source_id)
            return None

        if not display_source.card:
            log.error("Capture source has no card info: %s", capture_source_id)
            return None

        # Create Moonlight session
        moonlight_session = await self._moonlight.create_session(client_id)

        # Create pipeline config based on capture card capabilities
        card = display_source.card
        pipeline_config = PipelineConfig(
            name=f"capture-{capture_source_id}",
            input_source="v4l2",
            input_device=card.path,
            input_width=card.max_width,
            input_height=card.max_height,
            input_framerate=min(card.max_fps, 60),
            encoder="auto",
            codec="h264",
            bitrate_kbps=10000,
            rtp_destination="127.0.0.1",
            rtp_port=moonlight_session.stream_port,
            rtcp_port=moonlight_session.control_port,
            enable_fec=True,
            gamescope_enabled=False,
        )

        # Create GStreamer pipeline
        pipeline = await self._pipeline_manager.create_pipeline(
            moonlight_session.session_id,
            pipeline_config,
        )

        # Create input handler
        input_handler = MoonlightInputHandler(client_id)
        await input_handler.start()

        # Register with Moonlight protocol
        await self._moonlight.register_input_handler(
            moonlight_session.session_id,
            lambda report: self._handle_input_report(report, input_handler),
        )

        # Create session
        session = CaptureSession(
            capture_source_id=capture_source_id,
            display_source=display_source,
            capture_card=card,
            moonlight_session=moonlight_session,
            pipeline=pipeline,
            pipeline_config=pipeline_config,
            input_handler=input_handler,
            active=True,
            started_at=asyncio.get_event_loop().time(),
            clients=[client_id],
        )

        self._capture_sessions[capture_source_id] = session
        self._input_handlers[client_id] = input_handler

        log.info(
            "Started capture session %s for client %s",
            capture_source_id, client_id
        )

        return session

    async def _stop_session(self, capture_source_id: str) -> None:
        """Stop a capture session."""
        if capture_source_id not in self._capture_sessions:
            return

        session = self._capture_sessions[capture_source_id]

        # Stop input handler
        if session.input_handler:
            await session.input_handler.stop()
            for client_id in list(self._input_handlers.keys()):
                if self._input_handlers[client_id] is session.input_handler:
                    del self._input_handlers[client_id]

        # Stop pipeline
        if session.pipeline:
            await self._pipeline_manager.remove_pipeline(
                session.moonlight_session.session_id
            )

        # End Moonlight session
        if session.moonlight_session:
            await self._moonlight.end_session(session.moonlight_session.session_id)

        session.active = False
        session._ended_at = asyncio.get_event_loop().time()

        del self._capture_sessions[capture_source_id]

        log.info("Stopped capture session %s", capture_source_id)

    def _handle_input_report(
        self,
        report: InputReport,
        input_handler: MoonlightInputHandler,
    ) -> None:
        """Handle input report from Moonlight protocol."""
        # Convert and inject into evdev
        asyncio.create_task(
            input_handler.handle_input_packet(self._encode_report(report))
        )

    def _encode_report(self, report: InputReport) -> bytes:
        """Encode input report to Moonlight protocol format."""
        # Simplified encoding - in production, use proper Moonlight protocol
        import json
        return json.dumps({
            "keyboard": report.keyboard,
            "mouse": report.mouse,
            "gamepad": report.gamepad,
            "touch": report.touch,
            "haptics": report.haptics,
        }).encode()

    async def update_pipeline_config(
        self,
        capture_source_id: str,
        **kwargs,
    ) -> bool:
        """Update pipeline configuration for a capture session."""
        if capture_source_id not in self._capture_sessions:
            return False

        session = self._capture_sessions[capture_source_id]
        if not session.pipeline_config:
            return False

        # Update config
        for key, value in kwargs.items():
            if hasattr(session.pipeline_config, key):
                setattr(session.pipeline_config, key, value)

        # Restart pipeline with new config
        if session.pipeline:
            return await session.pipeline.restart(session.pipeline_config)

        return False

    def list_capture_sources(self) -> list[dict[str, Any]]:
        """List all available capture sources."""
        sources = []
        for source_id, display_source in self._display_capture._sources.items():
            sources.append({
                "capture_source_id": source_id,
                "name": display_source.card.name if display_source.card else "Unknown",
                "path": display_source.card.path if display_source.card else "",
                "resolutions": [
                    {"width": r.width, "height": r.height, "fps": r.fps}
                    for r in display_source.card.resolutions if display_source.card
                ],
                "active_sessions": len([
                    s for s in self._capture_sessions.values()
                    if s.capture_source_id == source_id and s.active
                ]),
            })
        return sources

    def get_capture_session(
        self,
        capture_source_id: str,
    ) -> CaptureSession | None:
        """Get a specific capture session."""
        return self._capture_sessions.get(capture_source_id)

    def get_active_sessions(self) -> list[dict[str, Any]]:
        """Get all active capture sessions."""
        return [
            {
                "capture_source_id": s.capture_source_id,
                "capture_source_name": s.display_source.card.name if s.display_source.card else "Unknown",
                "client_ids": s.clients,
                "started_at": s.started_at,
                "duration": s.duration,
            }
            for s in self._capture_sessions.values()
            if s.active
        ]

    async def list_moonlight_apps(self) -> list[dict[str, Any]]:
        """
        List Moonlight apps (capture sources that can be streamed).

        Each capture card appears as a "Moonlight app" that clients can launch.
        """
        apps = []
        for source_id, display_source in self._display_capture._sources.items():
            card = display_source.card
            if not card:
                continue

            # Count active sessions
            active_sessions = len([
                s for s in self._capture_sessions.values()
                if s.capture_source_id == source_id and s.active
            ])

            apps.append({
                "id": f"capture:{source_id}",
                "name": f"HDMI Capture: {card.name}",
                "description": f"Stream HDMI input from {card.name}",
                "icon": "monitor",
                "capture_source_id": source_id,
                "resolutions": [
                    {"width": r.width, "height": r.height, "fps": r.fps}
                    for r in card.resolutions
                ],
                "fps": card.max_fps,
                "current_session_count": active_sessions,
                "max_sessions": 4,  # Concurrent streams allowed
            })

        return apps

    async def launch_moonlight_app(
        self,
        app_id: str,
        client_id: str,
    ) -> bool:
        """
        Launch a Moonlight app (start a capture stream).

        This is called when Moonlight client clicks "Play" on an app.
        """
        if not app_id.startswith("capture:"):
            return False

        capture_source_id = app_id.split(":", 1)[1]
        return bool(await self.start_capture_session(capture_source_id, client_id))

    async def quit_moonlight_app(
        self,
        app_id: str,
        client_id: str,
    ) -> bool:
        """
        Quit a Moonlight app (stop a capture stream).

        This is called when Moonlight client stops streaming.
        """
        if not app_id.startswith("capture:"):
            return False

        capture_source_id = app_id.split(":", 1)[1]
        session = self._capture_sessions.get(capture_source_id)

        if session:
            # Remove client from session
            if client_id in session.clients:
                session.clients.remove(client_id)

            # Stop session if no more clients
            if not session.clients:
                await self._stop_session(capture_source_id)

            return True

        return False


# ── Module-level initialization ──────────────────────────────────────────────

def create_capture_to_moonlight(
    display_capture: DisplayCaptureManager,
    moonlight_protocol: MoonlightProtocol,
    data_dir: Path,
) -> CaptureToMoonlightManager:
    """
    Factory function to create a CaptureToMoonlightManager.

    This is the integration point that wires display capture with Moonlight.
    """
    return CaptureToMoonlightManager(display_capture, moonlight_protocol, data_dir)
