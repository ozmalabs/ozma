# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Screen capture for desktop soft nodes.

Captures the local display and makes it available as:
  1. An HLS stream (for the controller's web UI)
  2. MJPEG frames (for low-latency preview)
  3. Single JPEG snapshots (for OCR)

Capture backends (auto-detected in order):
  Linux:
    1. PipeWire screen capture (Wayland + X11, best quality)
    2. wf-recorder (Wayland, wlroots compositors)
    3. ffmpeg x11grab (X11 only, legacy)
  macOS:
    1. ffmpeg avfoundation (screen capture)
  Windows:
    1. ffmpeg gdigrab (screen capture)

The capture runs as an ffmpeg subprocess outputting HLS segments.
Same format as hardware capture — the controller treats it identically.
"""

from __future__ import annotations

import asyncio
import logging
import os
import shutil
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.softnode.screen_capture")


class ScreenCaptureBackend:
    """Captures the local screen via ffmpeg and outputs HLS + MJPEG."""

    def __init__(self, output_dir: str = "/tmp/ozma-screen-capture",
                 width: int = 1920, height: int = 1080, fps: int = 15) -> None:
        self._output_dir = Path(output_dir)
        self._width = width
        self._height = height
        self._fps = fps
        self._proc: asyncio.subprocess.Process | None = None
        self._active = False
        self._backend = ""

    @property
    def active(self) -> bool:
        return self._proc is not None and self._proc.returncode is None

    @property
    def stream_path(self) -> str:
        return str(self._output_dir / "stream.m3u8")

    @property
    def backend(self) -> str:
        return self._backend

    async def start(self) -> bool:
        """Auto-detect capture backend and start capturing."""
        if not shutil.which("ffmpeg"):
            log.warning("ffmpeg not found — screen capture disabled")
            return False

        self._output_dir.mkdir(parents=True, exist_ok=True)

        import platform
        system = platform.system()

        if system == "Linux":
            # Try PipeWire first (works on both Wayland and X11)
            if await self._try_pipewire():
                return True
            # Fall back to X11 grab
            if os.environ.get("DISPLAY"):
                if await self._try_x11grab():
                    return True
            log.warning("No screen capture backend available on Linux")
            return False

        elif system == "Darwin":
            return await self._try_avfoundation()

        elif system == "Windows":
            # DXcam (DXGI Desktop Duplication) → gdigrab fallback
            if await self._try_dxcam():
                return True
            return await self._try_gdigrab()

        return False

    async def stop(self) -> None:
        if self._proc and self._proc.returncode is None:
            self._proc.terminate()
            try:
                await asyncio.wait_for(self._proc.wait(), timeout=5.0)
            except asyncio.TimeoutError:
                self._proc.kill()
        self._proc = None
        self._active = False

    async def _try_pipewire(self) -> bool:
        """Capture via PipeWire (Wayland + X11)."""
        # Check if PipeWire screen capture portal is available
        if not shutil.which("pw-cli"):
            return False

        cmd = [
            "ffmpeg", "-hide_banner", "-loglevel", "warning", "-y",
            "-f", "pipewire", "-framerate", str(self._fps),
            "-i", "default",
            "-vf", f"scale={self._width}:{self._height}",
            "-c:v", "libx264", "-preset", "ultrafast", "-tune", "zerolatency",
            "-crf", "28",
            "-f", "hls",
            "-hls_time", "1",
            "-hls_list_size", "4",
            "-hls_flags", "delete_segments+independent_segments",
            "-hls_segment_filename", str(self._output_dir / "seg_%05d.ts"),
            str(self._output_dir / "stream.m3u8"),
        ]
        return await self._start_ffmpeg(cmd, "pipewire")

    async def _try_x11grab(self) -> bool:
        """Capture via X11 screen grab."""
        display = os.environ.get("DISPLAY", ":0")
        cmd = [
            "ffmpeg", "-hide_banner", "-loglevel", "warning", "-y",
            "-f", "x11grab",
            "-framerate", str(self._fps),
            "-video_size", f"{self._width}x{self._height}",
            "-i", display,
            "-c:v", "libx264", "-preset", "ultrafast", "-tune", "zerolatency",
            "-crf", "28",
            "-f", "hls",
            "-hls_time", "1",
            "-hls_list_size", "4",
            "-hls_flags", "delete_segments+independent_segments",
            "-hls_segment_filename", str(self._output_dir / "seg_%05d.ts"),
            str(self._output_dir / "stream.m3u8"),
        ]
        return await self._start_ffmpeg(cmd, "x11grab")

    async def _try_avfoundation(self) -> bool:
        """Capture via macOS AVFoundation."""
        cmd = [
            "ffmpeg", "-hide_banner", "-loglevel", "warning", "-y",
            "-f", "avfoundation",
            "-framerate", str(self._fps),
            "-capture_cursor", "1",
            "-i", "1:none",  # screen index 1, no audio
            "-vf", f"scale={self._width}:{self._height}",
            "-c:v", "libx264", "-preset", "ultrafast", "-tune", "zerolatency",
            "-crf", "28",
            "-f", "hls",
            "-hls_time", "1",
            "-hls_list_size", "4",
            "-hls_flags", "delete_segments+independent_segments",
            "-hls_segment_filename", str(self._output_dir / "seg_%05d.ts"),
            str(self._output_dir / "stream.m3u8"),
        ]
        return await self._start_ffmpeg(cmd, "avfoundation")

    async def _try_dxcam(self) -> bool:
        """
        Capture via DXcam (DXGI Desktop Duplication API).

        Much faster than gdigrab — captures directly from GPU, works with
        fullscreen D3D games, 240fps capable. Outputs frames which we
        pipe to ffmpeg for HLS encoding.

        Requires: pip install dxcam
        """
        try:
            import dxcam  # noqa: F401
        except ImportError:
            log.debug("DXcam not installed, trying gdigrab")
            return False

        # DXcam captures frames as numpy arrays. We pipe raw frames to ffmpeg.
        import dxcam as _dxcam
        self._output_dir.mkdir(parents=True, exist_ok=True)

        camera = _dxcam.create(output_idx=0, output_color="BGR")
        if not camera:
            return False

        # Start ffmpeg reading raw video from stdin
        cmd = [
            "ffmpeg", "-hide_banner", "-loglevel", "warning", "-y",
            "-f", "rawvideo", "-pix_fmt", "bgr24",
            "-s", f"{self._width}x{self._height}",
            "-r", str(self._fps),
            "-i", "pipe:0",
            "-c:v", "libx264", "-preset", "ultrafast", "-tune", "zerolatency",
            "-crf", "28",
            "-f", "hls",
            "-hls_time", "1",
            "-hls_list_size", "4",
            "-hls_flags", "delete_segments+independent_segments",
            "-hls_segment_filename", str(self._output_dir / "seg_%05d.ts"),
            str(self._output_dir / "stream.m3u8"),
        ]

        try:
            import subprocess as _sp
            self._proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdin=asyncio.subprocess.PIPE,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.DEVNULL,
            )

            async def _feed_frames():
                import time
                import numpy as np
                interval = 1.0 / self._fps
                while self._proc and self._proc.returncode is None:
                    frame = camera.grab()
                    if frame is not None:
                        # Resize if needed
                        if frame.shape[1] != self._width or frame.shape[0] != self._height:
                            import cv2
                            frame = cv2.resize(frame, (self._width, self._height))
                        try:
                            self._proc.stdin.write(frame.tobytes())
                            await self._proc.stdin.drain()
                        except Exception:
                            break
                    await asyncio.sleep(interval)
                camera.release()

            asyncio.create_task(_feed_frames(), name="dxcam-feed")
            self._backend_name = "dxcam"
            log.info("Screen capture active: dxcam (DXGI) (%dx%d@%dfps) → %s",
                     self._width, self._height, self._fps, self._output_dir)
            return True
        except Exception as e:
            log.debug("DXcam capture failed: %s", e)
            return False

    async def _try_gdigrab(self) -> bool:
        """Capture via Windows GDI grab."""
        cmd = [
            "ffmpeg", "-hide_banner", "-loglevel", "warning", "-y",
            "-f", "gdigrab",
            "-framerate", str(self._fps),
            "-i", "desktop",
            "-vf", f"scale={self._width}:{self._height}",
            "-c:v", "libx264", "-preset", "ultrafast", "-tune", "zerolatency",
            "-crf", "28",
            "-f", "hls",
            "-hls_time", "1",
            "-hls_list_size", "4",
            "-hls_flags", "delete_segments+independent_segments",
            "-hls_segment_filename", str(self._output_dir / "seg_%05d.ts"),
            str(self._output_dir / "stream.m3u8"),
        ]
        return await self._start_ffmpeg(cmd, "gdigrab")

    async def _start_ffmpeg(self, cmd: list[str], backend: str) -> bool:
        try:
            self._proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.PIPE,
            )
            # Wait briefly to see if it crashes immediately
            await asyncio.sleep(1.0)
            if self._proc.returncode is not None:
                stderr = await self._proc.stderr.read()
                log.debug("Screen capture %s failed: %s", backend, stderr.decode()[:200])
                return False

            self._backend = backend
            self._active = True
            log.info("Screen capture active: %s (%dx%d@%dfps) → %s",
                     backend, self._width, self._height, self._fps, self._output_dir)
            asyncio.create_task(self._monitor(), name="screen-capture-log")
            return True
        except Exception as e:
            log.debug("Screen capture %s failed: %s", backend, e)
            return False

    async def _monitor(self) -> None:
        if not self._proc or not self._proc.stderr:
            return
        try:
            async for line in self._proc.stderr:
                text = line.decode(errors="replace").rstrip()
                if text:
                    log.debug("Screen capture: %s", text)
        except Exception:
            pass
        self._active = False

    async def snapshot(self) -> bytes | None:
        """Capture a single JPEG frame of the current screen."""
        import platform
        system = platform.system()

        if system == "Linux" and os.environ.get("DISPLAY"):
            cmd = [
                "ffmpeg", "-hide_banner", "-loglevel", "error",
                "-f", "x11grab", "-framerate", "1",
                "-video_size", f"{self._width}x{self._height}",
                "-i", os.environ["DISPLAY"],
                "-frames:v", "1", "-f", "image2pipe", "-vcodec", "mjpeg", "pipe:1",
            ]
        elif system == "Darwin":
            cmd = [
                "ffmpeg", "-hide_banner", "-loglevel", "error",
                "-f", "avfoundation", "-framerate", "1",
                "-i", "1:none",
                "-frames:v", "1", "-f", "image2pipe", "-vcodec", "mjpeg", "pipe:1",
            ]
        elif system == "Windows":
            cmd = [
                "ffmpeg", "-hide_banner", "-loglevel", "error",
                "-f", "gdigrab", "-framerate", "1",
                "-i", "desktop",
                "-frames:v", "1", "-f", "image2pipe", "-vcodec", "mjpeg", "pipe:1",
            ]
        else:
            return None

        try:
            proc = await asyncio.create_subprocess_exec(
                *cmd, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.DEVNULL,
            )
            stdout, _ = await asyncio.wait_for(proc.communicate(), timeout=10)
            return stdout if stdout else None
        except Exception:
            return None

    def to_dict(self) -> dict:
        return {
            "active": self.active,
            "backend": self._backend,
            "resolution": f"{self._width}x{self._height}",
            "fps": self._fps,
            "stream_path": self.stream_path if self.active else None,
        }
