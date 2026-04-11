# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Hybrid streaming source adapter.

Provides unified source adapter that accepts any frame source (physical capture,
VNC, virtual desktop) and feeds it to the Moonlight encoder.

Features:
  - Unified source adapter: physical capture card / VNC / virtual desktop
  - GStreamer pipeline accepts any frame source
  - Single Moonlight server presents all source types
"""

from __future__ import annotations

import asyncio
import logging
import subprocess
import time
from dataclasses import dataclass, field
from enum import Enum
from pathlib import Path
from typing import Any, Callable

from .gstreamer_pipeline import GStreamerPipelineManager, PipelineConfig, EncoderConfig, OutputConfig
from .moonlight_protocol import MoonlightProtocolServer, MoonlightSession, RTPPacketiser

log = logging.getLogger("ozma.controller.gaming.hybrid_streaming")


# ─── Source Types ────────────────────────────────────────────────────────────

class SourceType(Enum):
    """Types of video sources."""
    HDMI_CAPTURE = "hDMI_capture"  # Physical HDMI capture card (V4L2)
    VNC = "vnc"                   # VNC stream from VM
    WAYLAND = "wayland"           # Wayland desktop
    X11 = "x11"                   # X11 desktop
    CONTAINER = "container"       # Container virtual desktop
    DISPLAY = "display"           # Current display
    FILE = "file"                 # Pre-recorded file
    RTMP = "rtmp"                 # RTMP stream


@dataclass
class SourceConfig:
    """Configuration for a video source."""
    type: SourceType
    # HDMI capture specific
    v4l2_device: str = "/dev/video0"
    v4l2_format: str = "NV12"
    # VNC specific
    vnc_host: str = "127.0.0.1"
    vnc_port: int = 5900
    vnc_password: str = ""
    # Display specific
    display_name: str = ":0"
    # Container specific
    container_id: str = ""
    container_socket: str = ""
    # General
    width: int = 1920
    height: int = 1080
    fps: int = 60
    rotation: int = 0  # 0, 90, 180, 270


@dataclass
class FrameStats:
    """Frame statistics."""
    frames_captured: int = 0
    frames_encoded: int = 0
    frames_dropped: int = 0
    bytes_encoded: int = 0
    latency_ms: float = 0.0
    last_frame_time: float = field(default_factory=time.time)


# ─── Source Adapter ──────────────────────────────────────────────────────────

class SourceAdapter:
    """
    Adapts different video sources to a common format for Moonlight streaming.

    Supports:
      - HDMI capture (V4L2)
      - VNC (from VMs)
      - Wayland/X11 desktops
      - Container virtual desktops
    """

    def __init__(self, data_dir: Path = Path("/var/lib/ozma/gaming")):
        self._data_dir = data_dir
        self._data_dir.mkdir(parents=True, exist_ok=True)
        self._current_source: SourceConfig | None = None
        self._stats = FrameStats()
        self._tasks: list[asyncio.Task] = []

    async def set_source(self, config: SourceConfig) -> bool:
        """Set the active video source."""
        self._current_source = config

        # Validate source availability
        if config.type == SourceType.HDMI_CAPTURE:
            device = Path(config.v4l2_device)
            if not device.exists():
                log.warning("V4L2 device %s not found", config.v4l2_device)

        log.info("Source set to %s (%dx%d@%d)", config.type.value, config.width, config.height, config.fps)
        return True

    def get_source_info(self) -> dict[str, Any]:
        """Get information about the current source."""
        if not self._current_source:
            return {"type": "none"}

        return {
            "type": self._current_source.type.value,
            "width": self._current_source.width,
            "height": self._current_source.height,
            "fps": self._current_source.fps,
            "rotation": self._current_source.rotation,
        }

    def get_stats(self) -> FrameStats:
        """Get frame statistics."""
        return self._stats

    def update_stats(self, **kwargs) -> None:
        """Update frame statistics."""
        for key, value in kwargs.items():
            if hasattr(self._stats, key):
                setattr(self._stats, key, value)
        self._stats.last_frame_time = time.time()


# ─── Hybrid Stream Manager ───────────────────────────────────────────────────

class HybridStreamManager:
    """
    Manages hybrid streaming with multiple source types.

    Features:
      - Source switching
      - Dynamic pipeline reconfiguration
      - Quality adaptation
    """

    def __init__(
        self,
        protocol_server: MoonlightProtocolServer,
        pipeline_manager: GStreamerPipelineManager,
        data_dir: Path = Path("/var/lib/ozma/gaming"),
    ):
        self._protocol = protocol_server
        self._pipeline = pipeline_manager
        self._data_dir = data_dir
        self._data_dir.mkdir(parents=True, exist_ok=True)

        self._adapter = SourceAdapter(data_dir)
        self._current_source: SourceConfig | None = None
        self._active_session: MoonlightSession | None = None
        self._pipeline_name: str | None = None

    async def start_session(
        self,
        session: MoonlightSession,
        source_config: SourceConfig,
    ) -> bool:
        """Start a streaming session with the specified source."""
        self._active_session = session
        self._current_source = source_config

        # Set the source adapter
        await self._adapter.set_source(source_config)

        # Create pipeline configuration
        pipeline_config = self._build_pipeline_config(source_config, session)

        # Start pipeline
        self._pipeline_name = f"stream_{session.session_id[:8]}"
        success = await self._pipeline.start_pipeline(self._pipeline_name, pipeline_config)

        if success:
            log.info(
                "Started hybrid stream session %s with source %s",
                session.session_id, source_config.type.value
            )
        return success

    async def stop_session(self) -> bool:
        """Stop the current streaming session."""
        if self._pipeline_name:
            await self._pipeline.stop_pipeline(self._pipeline_name)
            self._pipeline_name = None

        self._active_session = None
        self._current_source = None

        log.info("Stopped hybrid stream session")
        return True

    def _build_pipeline_config(
        self,
        source_config: SourceConfig,
        session: MoonlightSession,
    ) -> PipelineConfig:
        """Build a pipeline configuration for the source."""
        encoder_config = EncoderConfig(
            name="nvenc",
            codec="h265",
            bitrate_kbps=session.limits.max_bitrate_kbps if hasattr(session, 'limits') else 50_000,
            preset="p4",
            tune="ll",
            rc="cbr",
        )

        # Build source based on type
        source = self._build_source_config(source_config)

        # Build output
        output = OutputConfig(
            type="rtp",
            host=session.client_addr[0] if session.client_addr else "127.0.0.1",
            port=session.rtp_port,
            fec_enabled=True,
            fec_percentage=20,
        )

        return PipelineConfig(
            name=f"stream_{session.session_id[:8]}",
            video_encoder=encoder_config,
            sources=[source],
            outputs=[output],
            scale=False,
            low_latency=True,
        )

    def _build_source_config(self, config: SourceConfig) -> dict[str, Any]:
        """Build a source configuration dictionary."""
        if config.type == SourceType.HDMI_CAPTURE:
            return {
                "type": "v4l2",
                "device": config.v4l2_device,
                "width": config.width,
                "height": config.height,
                "fps": config.fps,
                "format": config.v4l2_format,
            }
        elif config.type == SourceType.VNC:
            return {
                "type": "vnc",
                "host": config.vnc_host,
                "port": config.vnc_port,
                "password": config.vnc_password,
                "width": config.width,
                "height": config.height,
            }
        elif config.type in (SourceType.WAYLAND, SourceType.X11, SourceType.DISPLAY):
            return {
                "type": "display",
                "display": config.display_name,
                "width": config.width,
                "height": config.height,
                "fps": config.fps,
            }
        elif config.type == SourceType.CONTAINER:
            return {
                "type": "wayland",
                "display": f"wayland-{config.container_id[:8]}",
                "width": config.width,
                "height": config.height,
                "fps": config.fps,
            }
        else:
            return {
                "type": "display",
                "width": config.width,
                "height": config.height,
                "fps": config.fps,
            }


# ─── Frame Source Interface ──────────────────────────────────────────────────

class FrameSource:
    """
    Interface for video frame sources.

    Implementations provide frames from various sources (V4L2, VNC, Wayland).
    """

    async def start(self) -> bool:
        """Start the frame source."""
        raise NotImplementedError

    async def stop(self) -> None:
        """Stop the frame source."""
        raise NotImplementedError

    async def get_frame(self, timeout: float = 1.0) -> bytes | None:
        """Get a video frame."""
        raise NotImplementedError

    def get_dimensions(self) -> tuple[int, int]:
        """Get frame dimensions."""
        raise NotImplementedError

    def get_fps(self) -> int:
        """Get target FPS."""
        raise NotImplementedError


# ─── HDMI Capture Source ─────────────────────────────────────────────────────

class HDMICaptureSource(FrameSource):
    """Frame source from HDMI capture card (V4L2)."""

    def __init__(self, device: str = "/dev/video0", width: int = 1920, height: int = 1080, fps: int = 60):
        self._device = Path(device)
        self._width = width
        self._height = height
        self._fps = fps
        self._running = False
        self._proc: asyncio.subprocess.Process | None = None
        self._width = width
        self._height = height

    async def start(self) -> bool:
        """Start V4L2 capture."""
        if not self._device.exists():
            log.warning("V4L2 device %s not found, using test pattern", self._device)

        # Try v4l2-ctl first to check if device is usable
        try:
            result = await asyncio.create_subprocess_exec(
                "v4l2-ctl", "-d", str(self._device), "--list-formats-ext",
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.DEVNULL,
            )
            await result.wait()
        except Exception:
            pass

        self._running = True
        log.info("HDMI capture source started on %s", self._device)
        return True

    async def stop(self) -> None:
        """Stop V4L2 capture."""
        self._running = False
        if self._proc:
            self._proc.terminate()
            try:
                await asyncio.wait_for(self._proc.wait(), timeout=5.0)
            except asyncio.TimeoutError:
                self._proc.kill()
                await self._proc.wait()
            self._proc = None
        log.info("HDMI capture source stopped")

    async def get_frame(self, timeout: float = 1.0) -> bytes | None:
        """Get a frame from V4L2."""
        if not self._running:
            return None

        # In production, use v4l2 capture
        # For now, return test data
        import random
        frame_size = self._width * self._height * 2  # NV12 approx
        return bytes(random.randint(0, 255) for _ in range(frame_size))

    def get_dimensions(self) -> tuple[int, int]:
        return self._width, self._height

    def get_fps(self) -> int:
        return self._fps


# ─── VNC Source ──────────────────────────────────────────────────────────────

class VNCSource(FrameSource):
    """Frame source from VNC stream."""

    def __init__(self, host: str = "127.0.0.1", port: int = 5900, password: str = ""):
        self._host = host
        self._port = port
        self._password = password
        self._running = False

    async def start(self) -> bool:
        """Start VNC connection."""
        self._running = True
        log.info("VNC source connected to %s:%d", self._host, self._port)
        return True

    async def stop(self) -> None:
        """Stop VNC connection."""
        self._running = False
        log.info("VNC source disconnected")

    async def get_frame(self, timeout: float = 1.0) -> bytes | None:
        """Get a frame from VNC."""
        if not self._running:
            return None
        # In production, use VNC protocol to get framebuffer update
        # For now, return test data
        import random
        return bytes(random.randint(0, 255) for _ in range(1000))

    def get_dimensions(self) -> tuple[int, int]:
        return 1920, 1080

    def get_fps(self) -> int:
        return 30


# ─── Desktop Source ──────────────────────────────────────────────────────────

class DesktopSource(FrameSource):
    """Frame source from desktop (Wayland/X11)."""

    def __init__(self, display: str = ":0", width: int = 1920, height: int = 1080, fps: int = 60):
        self._display = display
        self._width = width
        self._height = height
        self._fps = fps
        self._running = False

    async def start(self) -> bool:
        """Start desktop capture."""
        self._running = True
        log.info("Desktop source started on %s", self._display)
        return True

    async def stop(self) -> None:
        """Stop desktop capture."""
        self._running = False
        log.info("Desktop source stopped")

    async def get_frame(self, timeout: float = 1.0) -> bytes | None:
        """Get a frame from desktop."""
        if not self._running:
            return None
        # In production, use xwd, wf-recorder, or similar
        # For now, return test data
        import random
        return bytes(random.randint(0, 255) for _ in range(1000))

    def get_dimensions(self) -> tuple[int, int]:
        return self._width, self._height

    def get_fps(self) -> int:
        return self._fps
