# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Per-output Windows screen capture for multi-seat.

Captures a specific display output using DXGI Desktop Duplication (via dxcam)
or falls back to ffmpeg gdigrab with crop coordinates. Each seat gets its
own capture instance targeting its assigned display.

The capture pipeline:
  dxcam (preferred) → raw frames → ffmpeg stdin → HLS/MJPEG
  gdigrab (fallback) → ffmpeg -f gdigrab with offset_x/offset_y → HLS

Both paths accept encoder_args from the EncoderAllocator (e.g. NVENC, QSV).
"""

from __future__ import annotations

import asyncio
import logging
import shutil
import sys
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.agent.multiseat.capture_windows")


class DXCamSeatCapture:
    """
    Per-seat capture via dxcam (DXGI Desktop Duplication).

    dxcam captures frames as numpy arrays directly from the GPU's output.
    Works with fullscreen D3D/Vulkan games at up to 240fps.

    The adapter_index and output_index must match the DXGI enumeration
    from WindowsDisplayBackend — this ensures we capture the correct
    physical monitor.
    """

    def __init__(
        self,
        output_idx: int,
        device_idx: int = 0,
        width: int = 1920,
        height: int = 1080,
        fps: int = 30,
    ) -> None:
        self._output_idx = output_idx
        self._device_idx = device_idx
        self._width = width
        self._height = height
        self._fps = fps
        self._camera: Any = None
        self._ffmpeg_proc: asyncio.subprocess.Process | None = None
        self._feed_task: asyncio.Task | None = None
        self._output_dir: Path | None = None
        self._running = False

    @staticmethod
    def available() -> bool:
        """Check if dxcam is importable."""
        try:
            import dxcam  # noqa: F401
            return True
        except ImportError:
            return False

    async def start(
        self,
        output_dir: str,
        encoder_args: list[str] | None = None,
    ) -> bool:
        """
        Start capturing and encoding to HLS.

        Args:
            output_dir: Directory for HLS segments and playlist.
            encoder_args: ffmpeg encoder args (e.g. ["-c:v", "h264_nvenc", ...]).
                          Defaults to libx264 ultrafast if not provided.

        Returns True if capture started successfully.
        """
        try:
            import dxcam as _dxcam
        except ImportError:
            log.warning("dxcam not installed — cannot start DXCam capture")
            return False

        self._output_dir = Path(output_dir)
        self._output_dir.mkdir(parents=True, exist_ok=True)

        # Create dxcam camera for the specific adapter + output
        try:
            self._camera = _dxcam.create(
                device_idx=self._device_idx,
                output_idx=self._output_idx,
                output_color="BGR",
            )
        except Exception as e:
            log.warning("dxcam.create(device=%d, output=%d) failed: %s",
                        self._device_idx, self._output_idx, e)
            return False

        if not self._camera:
            log.warning("dxcam returned None for device=%d output=%d",
                        self._device_idx, self._output_idx)
            return False

        # Build ffmpeg command: raw video from stdin → HLS
        cmd = [
            "ffmpeg", "-hide_banner", "-loglevel", "warning", "-y",
            "-f", "rawvideo", "-pix_fmt", "bgr24",
            "-s", f"{self._width}x{self._height}",
            "-r", str(self._fps),
            "-i", "pipe:0",
        ]

        if encoder_args:
            cmd.extend(encoder_args)
        else:
            cmd.extend([
                "-c:v", "libx264", "-preset", "ultrafast",
                "-tune", "zerolatency", "-crf", "28",
            ])

        cmd.extend([
            "-f", "hls",
            "-hls_time", "1",
            "-hls_list_size", "4",
            "-hls_flags", "delete_segments+independent_segments",
            "-hls_segment_filename", str(self._output_dir / "seg_%05d.ts"),
            str(self._output_dir / "stream.m3u8"),
        ])

        try:
            self._ffmpeg_proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdin=asyncio.subprocess.PIPE,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.DEVNULL,
            )
        except Exception as e:
            log.warning("Failed to start ffmpeg for dxcam capture: %s", e)
            return False

        self._running = True
        self._feed_task = asyncio.create_task(
            self._feed_frames(),
            name=f"dxcam-feed-{self._device_idx}:{self._output_idx}",
        )

        log.info("DXCam capture started: device=%d output=%d %dx%d@%dfps → %s",
                 self._device_idx, self._output_idx,
                 self._width, self._height, self._fps, output_dir)
        return True

    async def _feed_frames(self) -> None:
        """Grab frames from dxcam and pipe to ffmpeg stdin."""
        loop = asyncio.get_running_loop()
        interval = 1.0 / self._fps

        while self._running and self._ffmpeg_proc and self._ffmpeg_proc.returncode is None:
            try:
                # dxcam.grab() is blocking; run in executor
                frame = await loop.run_in_executor(None, self._camera.grab)

                if frame is None:
                    await asyncio.sleep(interval)
                    continue

                # Resize if the frame dimensions don't match target
                if frame.shape[1] != self._width or frame.shape[0] != self._height:
                    try:
                        import cv2
                        frame = cv2.resize(frame, (self._width, self._height))
                    except ImportError:
                        # Without cv2, skip mismatched frames
                        await asyncio.sleep(interval)
                        continue

                self._ffmpeg_proc.stdin.write(frame.tobytes())
                await self._ffmpeg_proc.stdin.drain()

            except (BrokenPipeError, ConnectionResetError):
                log.warning("ffmpeg pipe broken — stopping dxcam capture")
                break
            except asyncio.CancelledError:
                break
            except Exception as e:
                log.debug("DXCam frame error: %s", e)
                await asyncio.sleep(interval)

    async def stop(self) -> None:
        """Stop capture and clean up."""
        self._running = False

        if self._feed_task and not self._feed_task.done():
            self._feed_task.cancel()
            try:
                await self._feed_task
            except asyncio.CancelledError:
                pass
            self._feed_task = None

        if self._ffmpeg_proc and self._ffmpeg_proc.returncode is None:
            try:
                self._ffmpeg_proc.stdin.close()
            except Exception:
                pass
            self._ffmpeg_proc.terminate()
            try:
                await asyncio.wait_for(self._ffmpeg_proc.wait(), timeout=5.0)
            except asyncio.TimeoutError:
                self._ffmpeg_proc.kill()
            self._ffmpeg_proc = None

        if self._camera:
            try:
                self._camera.release()
            except Exception:
                pass
            self._camera = None

        log.info("DXCam capture stopped: device=%d output=%d",
                 self._device_idx, self._output_idx)

    async def snapshot(self) -> bytes | None:
        """Grab a single JPEG frame from this display."""
        if not self._camera:
            return None

        try:
            loop = asyncio.get_running_loop()
            frame = await loop.run_in_executor(None, self._camera.grab)
            if frame is None:
                return None

            try:
                import cv2
                if frame.shape[1] != self._width or frame.shape[0] != self._height:
                    frame = cv2.resize(frame, (self._width, self._height))
                _, buf = cv2.imencode(".jpg", frame, [cv2.IMWRITE_JPEG_QUALITY, 85])
                return buf.tobytes()
            except ImportError:
                # Without cv2, encode via ffmpeg one-shot
                return await self._snapshot_ffmpeg(frame)
        except Exception as e:
            log.debug("Snapshot failed: %s", e)
            return None

    async def _snapshot_ffmpeg(self, frame) -> bytes | None:
        """Encode a raw frame to JPEG via ffmpeg pipe."""
        cmd = [
            "ffmpeg", "-hide_banner", "-loglevel", "error",
            "-f", "rawvideo", "-pix_fmt", "bgr24",
            "-s", f"{frame.shape[1]}x{frame.shape[0]}",
            "-i", "pipe:0",
            "-frames:v", "1", "-f", "image2pipe", "-vcodec", "mjpeg", "pipe:1",
        ]
        try:
            proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdin=asyncio.subprocess.PIPE,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.DEVNULL,
            )
            stdout, _ = await asyncio.wait_for(
                proc.communicate(input=frame.tobytes()), timeout=10,
            )
            return stdout if stdout else None
        except Exception:
            return None


class GdigrabSeatCapture:
    """
    Fallback per-seat capture via ffmpeg gdigrab with desktop crop.

    Captures a rectangular region of the Windows virtual desktop at the
    specified offset and size. Works without any Python packages but is
    slower than DXGI Desktop Duplication and cannot capture fullscreen
    D3D/Vulkan applications.
    """

    def __init__(
        self,
        offset_x: int = 0,
        offset_y: int = 0,
        width: int = 1920,
        height: int = 1080,
        fps: int = 30,
    ) -> None:
        self._offset_x = offset_x
        self._offset_y = offset_y
        self._width = width
        self._height = height
        self._fps = fps
        self._proc: asyncio.subprocess.Process | None = None
        self._output_dir: Path | None = None

    async def start(
        self,
        output_dir: str,
        encoder_args: list[str] | None = None,
    ) -> bool:
        """
        Start gdigrab capture to HLS.

        Args:
            output_dir: Directory for HLS output.
            encoder_args: Encoder args for ffmpeg. Defaults to libx264 ultrafast.

        Returns True if capture started.
        """
        if not shutil.which("ffmpeg"):
            log.warning("ffmpeg not found — gdigrab capture unavailable")
            return False

        self._output_dir = Path(output_dir)
        self._output_dir.mkdir(parents=True, exist_ok=True)

        cmd = [
            "ffmpeg", "-hide_banner", "-loglevel", "warning", "-y",
            "-f", "gdigrab",
            "-framerate", str(self._fps),
            "-offset_x", str(self._offset_x),
            "-offset_y", str(self._offset_y),
            "-video_size", f"{self._width}x{self._height}",
            "-i", "desktop",
        ]

        if encoder_args:
            cmd.extend(encoder_args)
        else:
            cmd.extend([
                "-c:v", "libx264", "-preset", "ultrafast",
                "-tune", "zerolatency", "-crf", "28",
            ])

        cmd.extend([
            "-f", "hls",
            "-hls_time", "1",
            "-hls_list_size", "4",
            "-hls_flags", "delete_segments+independent_segments",
            "-hls_segment_filename", str(self._output_dir / "seg_%05d.ts"),
            str(self._output_dir / "stream.m3u8"),
        ])

        try:
            self._proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.PIPE,
            )

            await asyncio.sleep(1.0)
            if self._proc.returncode is not None:
                stderr = await self._proc.stderr.read()
                log.warning("gdigrab capture failed: %s", stderr.decode()[:200])
                self._proc = None
                return False

            log.info("gdigrab capture started: %dx%d@%dfps at +%d+%d → %s",
                     self._width, self._height, self._fps,
                     self._offset_x, self._offset_y, output_dir)
            return True

        except Exception as e:
            log.warning("Failed to start gdigrab capture: %s", e)
            return False

    async def stop(self) -> None:
        """Stop capture."""
        if self._proc and self._proc.returncode is None:
            self._proc.terminate()
            try:
                await asyncio.wait_for(self._proc.wait(), timeout=5.0)
            except asyncio.TimeoutError:
                self._proc.kill()
            self._proc = None
        log.info("gdigrab capture stopped")

    async def snapshot(self) -> bytes | None:
        """Grab a single JPEG frame via ffmpeg gdigrab."""
        cmd = [
            "ffmpeg", "-hide_banner", "-loglevel", "error",
            "-f", "gdigrab",
            "-framerate", "1",
            "-offset_x", str(self._offset_x),
            "-offset_y", str(self._offset_y),
            "-video_size", f"{self._width}x{self._height}",
            "-i", "desktop",
            "-frames:v", "1", "-f", "image2pipe", "-vcodec", "mjpeg", "pipe:1",
        ]
        try:
            proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.DEVNULL,
            )
            stdout, _ = await asyncio.wait_for(proc.communicate(), timeout=10)
            return stdout if stdout else None
        except Exception:
            return None


async def create_seat_capture(
    display_info: dict,
    fps: int = 30,
    width: int = 1920,
    height: int = 1080,
) -> DXCamSeatCapture | GdigrabSeatCapture:
    """
    Factory: create the best available capture backend for a display.

    Args:
        display_info: Dict from WindowsDisplayBackend.get_display_for_capture().
        fps: Target capture framerate.
        width: Target output width.
        height: Target output height.

    Returns a DXCamSeatCapture or GdigrabSeatCapture instance.
    """
    method = display_info.get("method", "gdigrab")

    if method == "dxgi" and DXCamSeatCapture.available():
        return DXCamSeatCapture(
            output_idx=display_info["output_index"],
            device_idx=display_info.get("adapter_index", 0),
            width=width,
            height=height,
            fps=fps,
        )

    # Fallback to gdigrab
    return GdigrabSeatCapture(
        offset_x=display_info.get("offset_x", 0),
        offset_y=display_info.get("offset_y", 0),
        width=width,
        height=height,
        fps=fps,
    )
