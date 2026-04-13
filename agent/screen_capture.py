# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Screen capture for desktop soft nodes.

This module provides screen capture functionality using Rust-based implementation
with xcap for cross-platform screen capture, ffmpeg-sidecar for encoding,
and WebSocket streaming for real-time delivery to the controller.

The implementation supports:
- X11/Wayland on Linux via xcap
- XDG ScreenCast portal on Wayland via ashpd crate
- Windows DXGI Desktop Duplication
- macOS screen capture
- MJPEG encoding with ffmpeg-sidecar
- WebSocket streaming with <50ms frame latency
"""

import asyncio
import logging
import os
from pathlib import Path
from typing import Any, Optional

log = logging.getLogger("ozma.softnode.screen_capture")

try:
    # Import the Rust extension
    from ozma_agent.screen_capture import ScreenCaptureBackend as RustScreenCaptureBackend
    HAS_RUST_BACKEND = True
except ImportError:
    HAS_RUST_BACKEND = False
    log.warning("Rust screen capture backend not available, using Python fallback")


class ScreenCaptureBackend:
    """Captures the local screen via Rust extension and outputs MJPEG over WebSocket."""

    def __init__(self, output_dir: str = "/tmp/ozma-screen-capture",
                 width: int = 1920, height: int = 1080, fps: int = 15) -> None:
        if not HAS_RUST_BACKEND:
            raise RuntimeError("Rust screen capture backend not available")
            
        self._backend = RustScreenCaptureBackend(
            output_dir=output_dir,
            width=width,
            height=height,
            fps=fps
        )

    @property
    def active(self) -> bool:
        """Check if screen capture is currently active."""
        return self._backend.active()

    @property
    def stream_path(self) -> str:
        """Get the HLS stream path (compatibility property)."""
        return self._backend.stream_path()

    @property
    def backend(self) -> str:
        """Get the current capture backend name."""
        return self._backend.backend()

    async def start(self) -> bool:
        """Start screen capture.
        
        Returns:
            bool: True if capture started successfully, False otherwise.
        """
        try:
            # Run the blocking Rust start() method in a thread pool
            loop = asyncio.get_event_loop()
            result = await loop.run_in_executor(None, self._backend.start)
            if result:
                log.info("Screen capture started successfully with %s backend", self.backend)
            else:
                log.warning("Failed to start screen capture")
            return result
        except Exception as e:
            log.error("Error starting screen capture: %s", e)
            return False

    async def stop(self) -> None:
        """Stop screen capture."""
        try:
            # Run the blocking Rust stop() method in a thread pool
            loop = asyncio.get_event_loop()
            await loop.run_in_executor(None, self._backend.stop)
            log.info("Screen capture stopped")
        except Exception as e:
            log.error("Error stopping screen capture: %s", e)

    async def snapshot(self) -> Optional[bytes]:
        """Capture a single JPEG frame of the current screen.
        
        Returns:
            bytes: JPEG image data or None if capture failed.
        """
        try:
            # Run the blocking Rust snapshot() method in a thread pool
            loop = asyncio.get_event_loop()
            result = await loop.run_in_executor(None, self._backend.snapshot)
            return result
        except Exception as e:
            log.error("Error capturing snapshot: %s", e)
            return None

    def to_dict(self) -> dict:
        """Get screen capture status as dictionary.
        
        Returns:
            dict: Status information.
        """
        return {
            "active": self.active,
            "backend": self.backend,
            "resolution": f"{self._backend.width}x{self._backend.height}",
            "fps": self._backend.fps,
            "stream_path": self.stream_path if self.active else None,
        }
