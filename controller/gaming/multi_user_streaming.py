# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Multi-user streaming with isolated sessions.

Provides concurrent isolated streaming sessions for multiple users.

Features:
  - Concurrent isolated sessions (N users, N virtual desktops, N input/audio sets)
  - Session lifecycle: create → stream → pause → resume → destroy
  - Resource limits per session (configurable)
"""

from __future__ import annotations

import asyncio
import logging
import time
from dataclasses import dataclass, field
from enum import Enum
from pathlib import Path
from typing import Any

from .headless_wayland import VirtualCompositor, VirtualCompositorManager, WaylandConfig
from .virtual_audio import VirtualAudioManager, VirtualPipeWireSink, AudioConfig
from .virtual_input import VirtualInputManager, VirtualGamepad
from .moonlight_protocol import (
    MoonlightProtocolServer, MoonlightClient, MoonlightSession,
    AesGcmContext, ENETProtocol
)
from .gstreamer_pipeline import GStreamerPipelineManager, PipelineConfig

log = logging.getLogger("ozma.controller.gaming.multi_user_streaming")


# ─── Session State ───────────────────────────────────────────────────────────

class SessionState(Enum):
    """Streaming session states."""
    CREATED = "created"       # Session created, resources allocated
    PAUSED = "paused"         # Session paused, resources held
    STREAMING = "streaming"   # Session actively streaming
    TERMINATED = "terminated" # Session terminated, resources cleaned up


@dataclass
class SessionLimits:
    """Resource limits for a session."""
    max_bitrate_kbps: int = 50_000
    max_resolution: str = "3840x2160"
    max_fps: int = 60
    max_sessions_per_user: int = 3
    cpu_limit_percent: int = 50
    memory_limit_mb: int = 2048
    gpu_limit_percent: int = 100
    timeout_minutes: int = 60


@dataclass
class SessionInfo:
    """Information about a streaming session."""
    session_id: str
    user_id: str
    state: SessionState = SessionState.CREATED
    client: MoonlightClient | None = None
    client_addr: tuple[str, int] | None = None
    created_at: float = field(default_factory=time.time)
    started_at: float | None = None
    paused_at: float | None = None
    last_activity: float = field(default_factory=time.time)
    limits: SessionLimits = field(default_factory=SessionLimits)
    
    # Resources
    wayland_session: VirtualCompositor | None = None
    audio_sink: VirtualPipeWireSink | None = None
    input_device: VirtualGamepad | None = None
    gstreamer_pipeline: str | None = None
    rtp_session: MoonlightSession | None = None
    
    # Statistics
    frames_encoded: int = 0
    bytes_transferred: int = 0
    encode_errors: int = 0


# ─── Session Manager ─────────────────────────────────────────────────────────

class MultiUserStreamingManager:
    """
    Manages multiple concurrent streaming sessions with full isolation.

    Features:
      - Per-session virtual desktop (Wayland)
      - Per-session audio sink
      - Per-session input devices
      - Resource limits enforcement
      - Session lifecycle management
    """

    def __init__(
        self,
        protocol_server: MoonlightProtocolServer | None = None,
        data_dir: Path = Path("/var/lib/ozma/gaming"),
    ):
        self._protocol = protocol_server
        self._data_dir = data_dir
        self._data_dir.mkdir(parents=True, exist_ok=True)

        # Sub-managers
        self._wayland_mgr = VirtualCompositorManager(data_dir)
        self._audio_mgr = VirtualAudioManager(data_dir)
        self._input_mgr = VirtualInputManager(data_dir)
        self._pipeline_mgr = GStreamerPipelineManager(data_dir)

        # Sessions
        self._sessions: dict[str, SessionInfo] = {}
        self._user_sessions: dict[str, list[str]] = {}

        # Resource tracking
        self._total_cpu_percent = 0
        self._total_memory_mb = 0
        self._total_bitrate_kbps = 0

        # Load persisted state
        self._load_state()

    async def start(self) -> None:
        """Start the multi-user streaming manager."""
        await self._wayland_mgr.start()
        await self._audio_mgr.start()
        await self._input_mgr.start()
        await self._pipeline_mgr.start()

        log.info("MultiUserStreamingManager started")

    async def stop(self) -> None:
        """Stop the manager and cleanup all sessions."""
        # Stop all sessions
        for session_id in list(self._sessions.keys()):
            await self.terminate_session(session_id)

        await self._pipeline_mgr.stop()
        await self._input_mgr.stop()
        await self._audio_mgr.stop()
        await self._wayland_mgr.stop()

        self._save_state()
        log.info("MultiUserStreamingManager stopped")

    def _load_state(self) -> None:
        """Load session state from disk."""
        state_file = self._data_dir / "sessions.json"
        if state_file.exists():
            try:
                import json
                data = json.loads(state_file.read_text())
                for session_id, session_data in data.get("sessions", {}).items():
                    self._sessions[session_id] = SessionInfo(**session_data)
                log.info("Loaded %d sessions from disk", len(self._sessions))
            except Exception as e:
                log.error("Failed to load session state: %s", e)

    def _save_state(self) -> None:
        """Save session state to disk."""
        state_file = self._data_dir / "sessions.json"
        try:
            import json
            data = {
                "sessions": {
                    sid: {
                        "session_id": s.session_id,
                        "user_id": s.user_id,
                        "state": s.state.value,
                        "created_at": s.created_at,
                        "started_at": s.started_at,
                        "last_activity": s.last_activity,
                        "frames_encoded": s.frames_encoded,
                        "bytes_transferred": s.bytes_transferred,
                    }
                    for sid, s in self._sessions.items()
                },
                "last_save": time.time(),
            }
            state_file.write_text(json.dumps(data, indent=2))
        except Exception as e:
            log.error("Failed to save session state: %s", e)

    async def create_session(
        self,
        session_id: str,
        user_id: str,
        client: MoonlightClient,
        client_addr: tuple[str, int],
        limits: SessionLimits | None = None,
    ) -> SessionInfo | None:
        """Create a new streaming session."""
        if session_id in self._sessions:
            log.warning("Session %s already exists", session_id)
            return None

        # Check user session limit
        user_session_count = len(self._user_sessions.get(user_id, []))
        if user_session_count >= limits.max_sessions_per_user if limits else 3:
            log.warning("User %s has too many sessions", user_id)
            return None

        session_limits = limits or SessionLimits()

        # Check resource limits
        if self._total_cpu_percent + 20 > session_limits.cpu_limit_percent:
            log.warning("CPU limit exceeded")
            return None

        session = SessionInfo(
            session_id=session_id,
            user_id=user_id,
            client=client,
            client_addr=client_addr,
            limits=session_limits,
        )
        self._sessions[session_id] = session
        self._user_sessions.setdefault(user_id, []).append(session_id)

        # Allocate resources
        try:
            # Virtual desktop
            wayland_config = WaylandConfig(
                session_id=session_id,
                width=1920, height=1080,
                xwayland=True,
            )
            session.wayland_session = await self._wayland_mgr.create_session(session_id, wayland_config)

            # Audio sink
            audio_config = AudioConfig(session_id=session_id)
            session.audio_sink = await self._audio_mgr.create_session(session_id, audio_config)

            # Input devices
            session.input_device = await self._input_mgr.create_session(session_id)

            # Update resource tracking
            self._total_cpu_percent += 20
            self._total_memory_mb += 512
            self._total_bitrate_kbps += session_limits.max_bitrate_kbps

            session.state = SessionState.CREATED
            session.last_activity = time.time()

            log.info(
                "Created session %s for user %s on %s",
                session_id, user_id, client_addr[0]
            )
            return session
        except Exception as e:
            log.error("Failed to allocate resources for session %s: %s", session_id, e)
            await self.terminate_session(session_id)
            return None

    async def start_streaming(self, session_id: str) -> bool:
        """Start streaming for a session."""
        session = self._sessions.get(session_id)
        if not session:
            return False

        if session.state == SessionState.STREAMING:
            return True

        # Update state
        session.started_at = time.time()
        session.state = SessionState.STREAMING

        # Create RTP session
        if self._protocol and session.client and session.client_addr:
            session.rtp_session = self._protocol.create_session(
                session.client, session.client_addr
            )

        # Start GStreamer pipeline
        from .gstreamer_pipeline import EncoderConfig, SourceConfig, OutputConfig
        pipeline_config = PipelineConfig(
            name=f"stream-{session_id[:8]}",
            video_encoder=EncoderConfig(
                name="nvenc",
                codec="h265",
                bitrate_kbps=session.limits.max_bitrate_kbps,
            ),
            sources=[SourceConfig(
                type="display",
                width=1920,
                height=1080,
                fps=60,
            )],
            outputs=[OutputConfig(
                type="rtp",
                host=session.client_addr[0] if session.client_addr else "127.0.0.1",
                port=session.rtp_session.rtp_port if session.rtp_session else 47994,
            )],
        )

        session.gstreamer_pipeline = await self._pipeline_mgr.start_pipeline(
            f"stream-{session_id[:8]}", pipeline_config
        )

        session.last_activity = time.time()
        log.info("Session %s started streaming", session_id)
        return True

    async def pause_session(self, session_id: str) -> bool:
        """Pause a streaming session."""
        session = self._sessions.get(session_id)
        if not session or session.state != SessionState.STREAMING:
            return False

        session.state = SessionState.PAUSED
        session.paused_at = time.time()
        session.last_activity = time.time()

        log.info("Session %s paused", session_id)
        return True

    async def resume_session(self, session_id: str) -> bool:
        """Resume a paused session."""
        session = self._sessions.get(session_id)
        if not session or session.state != SessionState.PAUSED:
            return False

        session.state = SessionState.STREAMING
        session.started_at = time.time()  # Reset for new session tracking
        session.paused_at = None
        session.last_activity = time.time()

        log.info("Session %s resumed", session_id)
        return True

    async def terminate_session(self, session_id: str) -> bool:
        """Terminate a streaming session and cleanup resources."""
        session = self._sessions.get(session_id)
        if not session:
            return False

        # Stop streaming
        await self._pipeline_mgr.stop_pipeline(f"stream-{session_id[:8]}")

        # Remove RTP session
        if session.rtp_session and self._protocol:
            self._protocol.remove_session(session_id)

        # Cleanup resources
        if session.wayland_session:
            await self._wayland_mgr.stop_session(session_id)
            session.wayland_session = None

        if session.audio_sink:
            await self._audio_mgr.destroy_session(session_id)
            session.audio_sink = None

        if session.input_device:
            await self._input_mgr.destroy_session(session_id)
            session.input_device = None

        # Update resource tracking
        self._total_cpu_percent -= 20
        self._total_memory_mb -= 512
        self._total_bitrate_kbps -= session.limits.max_bitrate_kbps

        # Remove session
        del self._sessions[session_id]
        if session_id in self._user_sessions.get(session.user_id, []):
            self._user_sessions[session.user_id].remove(session_id)

        session.state = SessionState.TERMINATED
        session.last_activity = time.time()

        log.info("Session %s terminated", session_id)
        return True

    def get_session(self, session_id: str) -> SessionInfo | None:
        """Get a session by ID."""
        return self._sessions.get(session_id)

    def get_sessions_for_user(self, user_id: str) -> list[SessionInfo]:
        """Get all sessions for a user."""
        session_ids = self._user_sessions.get(user_id, [])
        return [self._sessions[sid] for sid in session_ids if sid in self._sessions]

    def get_active_sessions(self) -> list[SessionInfo]:
        """Get all active (streaming or paused) sessions."""
        return [
            s for s in self._sessions.values()
            if s.state in (SessionState.STREAMING, SessionState.PAUSED)
        ]

    def get_resource_usage(self) -> dict[str, Any]:
        """Get current resource usage."""
        return {
            "total_sessions": len(self._sessions),
            "active_sessions": len(self.get_active_sessions()),
            "cpu_percent": self._total_cpu_percent,
            "memory_mb": self._total_memory_mb,
            "bitrate_kbps": self._total_bitrate_kbps,
        }


# ─── Session Lifecycle Observer ──────────────────────────────────────────────

class SessionObserver:
    """
    Observes session lifecycle events and triggers actions.

    Can be used for:
      - Notification on session start/stop
      - Metrics collection
      - Auto-termination of idle sessions
    """

    def __init__(self, manager: MultiUserStreamingManager):
        self._manager = manager
        self._on_session_start: list[callable] = []
        self._on_session_stop: list[callable] = []
        self._on_session_pause: list[callable] = []
        self._on_session_resume: list[callable] = []

    def on_session_start(self, callback: callable) -> None:
        """Register callback for session start."""
        self._on_session_start.append(callback)

    def on_session_stop(self, callback: callable) -> None:
        """Register callback for session stop."""
        self._on_session_stop.append(callback)

    def on_session_pause(self, callback: callable) -> None:
        """Register callback for session pause."""
        self._on_session_pause.append(callback)

    def on_session_resume(self, callback: callable) -> None:
        """Register callback for session resume."""
        self._on_session_resume.append(callback)

    async def notify_start(self, session: SessionInfo) -> None:
        """Notify observers of session start."""
        for callback in self._on_session_start:
            try:
                await callback(session)
            except Exception as e:
                log.error("Session start callback error: %s", e)

    async def notify_stop(self, session: SessionInfo) -> None:
        """Notify observers of session stop."""
        for callback in self._on_session_stop:
            try:
                await callback(session)
            except Exception as e:
                log.error("Session stop callback error: %s", e)

    async def notify_pause(self, session: SessionInfo) -> None:
        """Notify observers of session pause."""
        for callback in self._on_session_pause:
            try:
                await callback(session)
            except Exception as e:
                log.error("Session pause callback error: %s", e)

    async def notify_resume(self, session: SessionInfo) -> None:
        """Notify observers of session resume."""
        for callback in self._on_session_resume:
            try:
                await callback(session)
            except Exception as e:
                log.error("Session resume callback error: %s", e)
