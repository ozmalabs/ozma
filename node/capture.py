# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Video + audio capture with hardware-accelerated encoding → HLS output.

Pipeline:
  V4L2 capture device
    └─ ffmpeg (HW encoder selected by hw_detect)
         ├─ video: H.265 or H.264 → HLS segments + manifest
         └─ audio: AAC → muxed into same HLS segments

The HLS manifest is served by the node's HTTP server at /stream/stream.m3u8.
Segment files land in a configurable output directory (default: /tmp/ozma-stream/).

Audio is optional — if no audio device is found or encoding fails, the stream
continues video-only.
"""

from __future__ import annotations

import asyncio
import logging
import shutil
from pathlib import Path
from typing import TYPE_CHECKING

from hw_detect import EncoderConfig, CaptureDevice

if TYPE_CHECKING:
    pass

log = logging.getLogger("ozma.node.capture")


class MediaCapture:
    """
    Manages the ffmpeg capture-and-encode subprocess.

    Call start() to launch, stop() to terminate.
    The HLS manifest appears at {out_dir}/stream.m3u8 once ffmpeg
    has written the first segment (~2 s after start).
    """

    def __init__(
        self,
        capture_device: CaptureDevice,
        encoder: EncoderConfig,
        out_dir: Path,
        *,
        capture_width: int | None = None,
        capture_height: int | None = None,
        capture_fps: int = 30,
        stream_width: int | None = None,
        stream_height: int | None = None,
        hls_segment_secs: float = 1.0,
        hls_list_size: int = 4,
        audio_device: str | None = None,  # ALSA hw:X,Y
    ) -> None:
        self._device = capture_device
        self._encoder = encoder
        self._out_dir = out_dir
        self._cap_w = capture_width or capture_device.max_width
        self._cap_h = capture_height or capture_device.max_height
        self._cap_fps = capture_fps
        # Stream output resolution: default to capture size, clamped to 1920 wide
        out_w = stream_width or min(self._cap_w, 1920)
        out_h = stream_height or (out_w * self._cap_h // self._cap_w & ~1)
        self._out_w = out_w
        self._out_h = out_h
        self._hls_seg = hls_segment_secs
        self._hls_list = hls_list_size
        self._audio_device = audio_device or capture_device.audio_device
        self._uac2_device: str | None = None   # set via add_uac2_output() before start()
        self._proc: asyncio.subprocess.Process | None = None
        self._task: asyncio.Task | None = None
        self._stop = asyncio.Event()

    def add_uac2_output(self, alsa_device: str) -> None:
        """
        Add a UAC2 ALSA output alongside the HLS stream.

        Must be called before start().  The same audio source is written
        as raw PCM to the UAC2 gadget's ALSA playback interface (host sees
        this as a microphone/capture source) — no extra ALSA reader needed.
        """
        self._uac2_device = alsa_device

    @property
    def manifest_path(self) -> Path:
        return self._out_dir / "stream.m3u8"

    @property
    def active(self) -> bool:
        return self._proc is not None and self._proc.returncode is None

    async def start(self) -> None:
        self._out_dir.mkdir(parents=True, exist_ok=True)
        self._stop.clear()
        self._task = asyncio.create_task(self._run_with_backoff(), name="media-capture")
        log.info(
            "MediaCapture starting: %s → %dx%d  encoder=%s",
            self._device.path, self._out_w, self._out_h, self._encoder.name,
        )

    async def stop(self) -> None:
        self._stop.set()
        if self._proc and self._proc.returncode is None:
            self._proc.terminate()
            try:
                await asyncio.wait_for(self._proc.wait(), timeout=5.0)
            except asyncio.TimeoutError:
                self._proc.kill()
        if self._task:
            self._task.cancel()
            try:
                await self._task
            except asyncio.CancelledError:
                pass

    # ── internal ──────────────────────────────────────────────────────────────

    def _build_ffmpeg_cmd(self) -> list[str]:
        enc = self._encoder
        cap = self._device

        # --- Input pixel format preference ---
        # MJPG saves USB bandwidth on external capture cards.
        # YUYV is uncompressed and universally supported.
        pix_fmt_args: list[str] = []
        if "MJPG" in cap.formats:
            pix_fmt_args = ["-input_format", "mjpeg"]
        elif "NV12" in cap.formats:
            pix_fmt_args = ["-input_format", "nv12"]

        # --- Video input ---
        video_in = [
            *enc.input_flags,
            "-f", "v4l2",
            *pix_fmt_args,
            "-video_size", f"{self._cap_w}x{self._cap_h}",
            "-framerate", str(self._cap_fps),
            "-i", cap.path,
        ]

        # --- Audio input (optional) ---
        audio_in: list[str] = []
        audio_map: list[str] = []
        audio_encode: list[str] = []
        if self._audio_device:
            audio_in = [
                "-f", "alsa",
                "-channels", "2",
                "-sample_rate", "48000",
                "-i", self._audio_device,
            ]
            audio_map = ["-map", "0:v:0", "-map", "1:a:0"]
            audio_encode = [
                "-c:a", "aac",
                "-b:a", "128k",
                "-ar", "48000",
                "-ac", "2",
            ]
        else:
            audio_map = ["-map", "0:v:0"]

        # --- Scale filter ---
        # For VAAPI the hwupload vf is already in encode_flags; skip explicit scale.
        if enc.vaapi_device:
            # VAAPI: format+hwupload is in encode_flags; add scale_vaapi for resize
            if self._out_w != self._cap_w or self._out_h != self._cap_h:
                vf_flags = [
                    "-vf",
                    f"format=nv12,hwupload,scale_vaapi={self._out_w}:{self._out_h}",
                ]
            else:
                vf_flags = ["-vf", "format=nv12,hwupload"]
            encode_flags = [f for f in enc.encode_flags if f not in ("-vf", "format=nv12,hwupload")]
        elif self._out_w != self._cap_w or self._out_h != self._cap_h:
            vf_flags = ["-vf", f"scale={self._out_w}:{self._out_h},format=yuv420p"]
            encode_flags = enc.encode_flags
        else:
            vf_flags = ["-vf", "format=yuv420p"]
            encode_flags = enc.encode_flags

        # --- HLS output ---
        hls_out = [
            "-f", "hls",
            "-hls_time", str(self._hls_seg),
            "-hls_list_size", str(self._hls_list),
            "-hls_flags", "delete_segments+independent_segments+append_list",
            "-hls_segment_filename", str(self._out_dir / "seg_%05d.ts"),
            str(self._out_dir / "stream.m3u8"),
        ]

        # --- UAC2 audio output (optional) ---
        # Writes a second copy of the audio as raw PCM to the USB Audio gadget's
        # ALSA playback interface.  The host receives it as a capture/mic source.
        # Using the same ffmpeg invocation avoids opening the ALSA source twice.
        uac2_out: list[str] = []
        if self._uac2_device and self._audio_device:
            # Map only the audio stream to the UAC2 output
            uac2_out = [
                "-map", "1:a:0",
                "-c:a", "pcm_s16le",
                "-ar", "48000",
                "-ac", "2",
                "-f", "alsa",
                self._uac2_device,
            ]

        return [
            "ffmpeg", "-y", "-hide_banner",
            "-loglevel", "warning",
            *video_in,
            *audio_in,
            *audio_map,
            "-c:v", enc.ffmpeg_encoder,
            *encode_flags,
            *vf_flags,
            *audio_encode,
            *hls_out,
            *uac2_out,
        ]

    async def _run_with_backoff(self) -> None:
        backoff = 2.0
        while not self._stop.is_set():
            try:
                await self._capture_loop()
            except asyncio.CancelledError:
                return
            except Exception as e:
                log.warning("Capture error: %s — retry in %.0fs", e, backoff)
            try:
                await asyncio.wait_for(asyncio.shield(self._stop.wait()), timeout=backoff)
                return
            except asyncio.TimeoutError:
                backoff = min(backoff * 2, 30.0)

    async def _capture_loop(self) -> None:
        cmd = self._build_ffmpeg_cmd()
        log.debug("ffmpeg cmd: %s", " ".join(cmd))

        self._proc = await asyncio.create_subprocess_exec(
            *cmd,
            stdout=asyncio.subprocess.DEVNULL,
            stderr=asyncio.subprocess.PIPE,
        )
        log.info("ffmpeg capture started (pid=%d)", self._proc.pid)

        # Drain stderr in background so the pipe doesn't fill up
        async def drain_stderr() -> None:
            assert self._proc and self._proc.stderr
            async for line in self._proc.stderr:
                txt = line.decode(errors="replace").rstrip()
                if txt:
                    log.debug("ffmpeg: %s", txt)

        drain_task = asyncio.create_task(drain_stderr(), name="capture-stderr")
        try:
            rc = await self._proc.wait()
            if rc != 0 and not self._stop.is_set():
                raise RuntimeError(f"ffmpeg exited with code {rc}")
        finally:
            drain_task.cancel()
            try:
                await drain_task
            except asyncio.CancelledError:
                pass
            self._proc = None
