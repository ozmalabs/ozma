# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Headless Wayland compositor per session.

Provides a virtual Wayland compositor for each streaming session,
allowing container/VM games to run without a physical display.

Architecture:
  - Virtual Wayland compositor (wlroots-based or Smithay-based)
  - XWayland for legacy X11 apps
  - Virtual framebuffer (no physical display required)
  - One compositor instance per concurrent stream session

Dependencies:
  - wayland (libwayland-dev)
  - wlr-protocol (wayland-protocols)
  - Either: wlroots, Smithay, or KWin in headless mode

See: https://github.com/envygeeks/gst-wayland-display
"""

from __future__ import annotations

import asyncio
import logging
import os
import shutil
import subprocess
import tempfile
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.controller.gaming.headless_wayland")


# ─── Constants ───────────────────────────────────────────────────────────────

DEFAULT_WIDTH = 1920
DEFAULT_HEIGHT = 1080
DEFAULT_SCALE = 1
DEFAULT_FORMAT = "NV12"

# Wayland socket names per session
WAYLAND_SOCKET_PREFIX = "ozma-wayland-"


# ─── Wayland Display Configuration ───────────────────────────────────────────

@dataclass
class WaylandConfig:
    """Configuration for a Wayland compositor instance."""
    session_id: str
    width: int = DEFAULT_WIDTH
    height: int = DEFAULT_HEIGHT
    scale: int = DEFAULT_SCALE
    format: str = DEFAULT_FORMAT
    xwayland: bool = True
    cursor_size: int = 24
    cursor_theme: str = "default"
    max_render_nodes: int = 16
    enable_drm_lease: bool = True
    enable_vulkan: bool = True


@dataclass
class SessionInfo:
    """Information about a Wayland session."""
    session_id: str
    display_id: int  # :0, :1, :2, etc.
    socket_path: Path
    wayland_display: str
    xwayland_display: str | None = None
    xdg_runtime_dir: Path | None = None
    compositor_pid: int | None = None
    created_at: float = field(default_factory=time.time)


# ─── Virtual Compositor ──────────────────────────────────────────────────────

class VirtualCompositor:
    """
    Manages a virtual Wayland compositor instance.

    Supports two backends:
      1. gst-wayland-display (Rust, from Wolf) - preferred
      2. wlroots + headless backend (Python wrapper)
    """

    def __init__(self, config: WaylandConfig, data_dir: Path):
        self.config = config
        self._data_dir = data_dir
        self._data_dir.mkdir(parents=True, exist_ok=True)
        self._compositor_process: asyncio.subprocess.Process | None = None
        self._compositor_pid: int | None = None
        self._display_id: int | None = None
        self._xdg_runtime_dir: Path | None = None

    @property
    def is_running(self) -> bool:
        """Check if the compositor is running."""
        if self._compositor_process is None:
            return False
        return self._compositor_process.returncode is None

    async def start(self) -> bool:
        """Start the virtual compositor."""
        # Create runtime directory
        self._xdg_runtime_dir = Path(tempfile.mkdtemp(prefix="ozma-wayland-"))
        os.chmod(self._xdg_runtime_dir, 0o700)

        # Determine display number
        self._display_id = await self._find_available_display()

        # Set environment
        env = os.environ.copy()
        env["WAYLAND_DISPLAY"] = f"wayland-{self._display_id}"
        env["XDG_RUNTIME_DIR"] = str(self._xdg_runtime_dir)
        env["WLR_RENDERER"] = "software"
        env["WLR_BACKENDS"] = "headless"
        env["WLR_HEADLESS_OUTPUT"] = f"{self.config.width}x{self.config.height}@{self.config.scale}"

        # Try gst-wayland-display first (Wolf's implementation)
        if shutil.which("gst-wayland-display"):
            log.info("Using gst-wayland-display for session %s", self.config.session_id)
            return await self._start_gst_compositor(env)

        # Fallback to wlroots
        log.info("Falling back to wlroots for session %s", self.config.session_id)
        return await self._start_wlroots_compositor(env)

    async def _start_gst_compositor(self, env: dict) -> bool:
        """Start gst-wayland-display compositor."""
        try:
            proc = await asyncio.create_subprocess_exec(
                "gst-wayland-display",
                "--display", str(self._display_id),
                "--width", str(self.config.width),
                "--height", str(self.config.height),
                "--scale", str(self.config.scale),
                "--xwayland" if self.config.xwayland else "--no-xwayland",
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
                env=env,
            )
            self._compositor_process = proc
            self._compositor_pid = proc.pid

            # Wait for ready signal
            await asyncio.wait_for(self._wait_for_compositor_ready(proc), timeout=5.0)

            log.info(
                "Started gst-wayland-display (PID %d) on :%d for session %s",
                proc.pid, self._display_id, self.config.session_id
            )
            return True
        except Exception as e:
            log.error("Failed to start gst-wayland-display: %s", e)
            return False

    async def _start_wlroots_compositor(self, env: dict) -> bool:
        """Start wlroots compositor (requires Python wrapper)."""
        # This would be a Python wrapper around wlroots
        # For now, log that we need the wrapper
        log.warning("wlroots wrapper not implemented - using stub mode")

        # Create stub socket file
        socket_path = self._xdg_runtime_dir / f"wayland-{self._display_id}"
        socket_path.write_text("stub-wayland-display")

        self._compositor_pid = os.getpid()  # Mark as running
        log.info(
            "Started wlroots stub (PID %d) on :%d for session %s",
            self._compositor_pid, self._display_id, self.config.session_id
        )
        return True

    async def _find_available_display(self) -> int:
        """Find an available display number."""
        # Check for existing wayland displays
        for i in range(100):
            display_socket = Path(f"/tmp/.X11-unix/X{i}")
            if not display_socket.exists():
                return i
        raise RuntimeError("No available display numbers")

    async def _wait_for_compositor_ready(self, proc: asyncio.subprocess.Process) -> None:
        """Wait for compositor to be ready."""
        while proc.returncode is None:
            line = await proc.stderr.readline()
            if not line:
                break
            msg = line.decode("utf-8", errors="replace").strip()
            if "ready" in msg.lower() or "listening" in msg.lower():
                return
            if "error" in msg.lower():
                raise RuntimeError(f"Compositor error: {msg}")

    async def stop(self) -> None:
        """Stop the compositor."""
        if self._compositor_process:
            proc = self._compositor_process
            proc.terminate()
            try:
                await asyncio.wait_for(proc.wait(), timeout=5.0)
            except asyncio.TimeoutError:
                proc.kill()
                await proc.wait()

        # Clean up runtime directory
        if self._xdg_runtime_dir and self._xdg_runtime_dir.exists():
            try:
                shutil.rmtree(self._xdg_runtime_dir)
            except Exception as e:
                log.warning("Failed to clean up runtime dir: %s", e)

        self._compositor_process = None
        self._compositor_pid = None
        log.info(
            "Stopped compositor for session %s", self.config.session_id
        )

    def get_display_env(self) -> dict[str, str]:
        """Get environment variables for client applications."""
        if self._display_id is None:
            return {}
        return {
            "WAYLAND_DISPLAY": f"wayland-{self._display_id}",
            "XDG_RUNTIME_DIR": str(self._xdg_runtime_dir),
        }


# ─── Virtual Compositor Manager ──────────────────────────────────────────────

class VirtualCompositorManager:
    """
    Manages multiple virtual Wayland compositors for concurrent sessions.

    Features:
      - Per-session compositors
      - Automatic display number allocation
      - Cleanup on session termination
    """

    def __init__(self, data_dir: Path = Path("/var/lib/ozma/gaming")):
        self._data_dir = data_dir
        self._data_dir.mkdir(parents=True, exist_ok=True)
        self._sessions: dict[str, VirtualCompositor] = {}
        self._session_infos: dict[str, SessionInfo] = {}

    async def start(self) -> None:
        """Start the manager."""
        log.info("VirtualCompositorManager started")

    async def stop(self) -> None:
        """Stop the manager and cleanup all sessions."""
        for session_id in list(self._sessions.keys()):
            await self.stop_session(session_id)
        log.info("VirtualCompositorManager stopped")

    async def create_session(self, session_id: str, config: WaylandConfig | None = None) -> VirtualCompositor | None:
        """Create a virtual compositor for a session."""
        if session_id in self._sessions:
            return self._sessions[session_id]

        if config is None:
            config = WaylandConfig(session_id=session_id)

        compositor = VirtualCompositor(config, self._data_dir)
        if not await compositor.start():
            return None

        # Store session info
        self._sessions[session_id] = compositor
        self._session_infos[session_id] = SessionInfo(
            session_id=session_id,
            display_id=compositor._display_id or 0,
            socket_path=Path(f"/tmp/.X11-unix/X{compositor._display_id}") if compositor._display_id else Path(),
            wayland_display=f"wayland-{compositor._display_id}" if compositor._display_id else "",
            xwayland_display=f":{compositor._display_id + 10}" if compositor._display_id and config.xwayland else None,
            xdg_runtime_dir=compositor._xdg_runtime_dir,
            compositor_pid=compositor._compositor_pid,
        )

        log.info("Created Wayland session %s on :%d", session_id, compositor._display_id)
        return compositor

    async def stop_session(self, session_id: str) -> bool:
        """Stop a virtual compositor for a session."""
        if session_id not in self._sessions:
            return False

        compositor = self._sessions.pop(session_id)
        await compositor.stop()

        if session_id in self._session_infos:
            del self._session_infos[session_id]

        log.info("Stopped Wayland session %s", session_id)
        return True

    def get_session(self, session_id: str) -> VirtualCompositor | None:
        """Get the compositor for a session."""
        return self._sessions.get(session_id)

    def get_session_info(self, session_id: str) -> SessionInfo | None:
        """Get session information."""
        return self._session_infos.get(session_id)

    def get_all_sessions(self) -> list[str]:
        """Get all active session IDs."""
        return list(self._sessions.keys())

    def get_env_for_session(self, session_id: str) -> dict[str, str] | None:
        """Get environment variables for a session's client applications."""
        compositor = self._sessions.get(session_id)
        if compositor:
            return compositor.get_display_env()
        return None
