# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Distributed video — H.265 encode + RTP transport from video nodes.

V1.0 Track B: for machines not within cable reach of the controller.
The video node captures HDMI via a USB capture card, encodes to H.265
using the SBC's hardware encoder (RK3588 MPP, VAAPI, etc.), and streams
via RTP/UDP to the controller.

The controller decodes and displays alongside local capture cards —
the scenario engine treats both identically.

Pipeline:
  /dev/videoN (V4L2 capture) → ffmpeg H.265 encode → RTP/UDP → controller
  Controller: ffmpeg RTP receive → decode → HLS segments → web UI

mDNS advertisement:
  video_rtp_port=<port>   — controller connects here for the RTP stream
  video_width=<w>         — current capture resolution
  video_height=<h>
  video_fps=<fps>

Typical latency budget:
  Capture: ~8ms, Encode: ~4ms, Network: <1ms, Decode: ~4ms, Display: ~8ms
  Total: ~25ms glass-to-glass
"""

from __future__ import annotations

import asyncio
import logging
import shutil
from dataclasses import dataclass
from typing import Any

log = logging.getLogger("ozma.node.video_stream")


@dataclass
class VideoStreamConfig:
    """Configuration for the video RTP stream."""
    capture_device: str = "/dev/video0"
    width: int = 1920
    height: int = 1080
    fps: int = 60
    rtp_port: int = 5004
    bitrate: str = "8M"
    encoder: str = ""  # auto-detect


class VideoStreamer:
    """
    Captures HDMI and streams H.265 via RTP/UDP.

    Usage on the node::

        streamer = VideoStreamer(config)
        await streamer.start()
        # Controller connects to RTP port and receives the stream
        await streamer.stop()
    """

    def __init__(self, config: VideoStreamConfig | None = None) -> None:
        self._config = config or VideoStreamConfig()
        self._proc: asyncio.subprocess.Process | None = None
        self._encoder = ""

    @property
    def active(self) -> bool:
        return self._proc is not None and self._proc.returncode is None

    @property
    def rtp_port(self) -> int:
        return self._config.rtp_port

    async def start(self) -> bool:
        """Start the capture → encode → RTP stream."""
        if not shutil.which("ffmpeg"):
            log.warning("ffmpeg not found — video streaming disabled")
            return False

        cfg = self._config
        self._encoder = self._detect_encoder()

        cmd = ["ffmpeg", "-hide_banner", "-loglevel", "warning", "-y"]

        # Input
        cmd.extend([
            "-f", "v4l2",
            "-video_size", f"{cfg.width}x{cfg.height}",
            "-framerate", str(cfg.fps),
            "-i", cfg.capture_device,
        ])

        # Encode
        if "vaapi" in self._encoder:
            cmd.extend([
                "-vaapi_device", "/dev/dri/renderD128",
                "-vf", "format=nv12,hwupload",
                "-c:v", self._encoder,
                "-qp", "24",
            ])
        elif "nvenc" in self._encoder:
            cmd.extend([
                "-c:v", self._encoder,
                "-preset", "p4", "-tune", "ll",
                "-b:v", cfg.bitrate,
            ])
        elif "v4l2m2m" in self._encoder:
            # RK3588 / Rockchip hardware encoder
            cmd.extend([
                "-c:v", self._encoder,
                "-b:v", cfg.bitrate,
            ])
        else:
            cmd.extend([
                "-c:v", "libx265",
                "-preset", "ultrafast",
                "-tune", "zerolatency",
                "-x265-params", "keyint=60:bframes=0",
                "-b:v", cfg.bitrate,
            ])

        # RTP output
        cmd.extend([
            "-f", "rtp",
            "-sdp_file", "/tmp/ozma-video-stream.sdp",
            f"rtp://0.0.0.0:{cfg.rtp_port}",
        ])

        try:
            self._proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.PIPE,
            )
            log.info("Video stream started: %s %dx%d@%dfps → RTP port %d (encoder: %s)",
                     cfg.capture_device, cfg.width, cfg.height, cfg.fps,
                     cfg.rtp_port, self._encoder)
            asyncio.create_task(self._monitor(), name="video-stream-monitor")
            return True
        except Exception as e:
            log.warning("Failed to start video stream: %s", e)
            return False

    async def stop(self) -> None:
        if self._proc and self._proc.returncode is None:
            self._proc.terminate()
            try:
                await asyncio.wait_for(self._proc.wait(), timeout=5.0)
            except asyncio.TimeoutError:
                self._proc.kill()

    def _detect_encoder(self) -> str:
        """Detect the best available H.265 encoder."""
        import subprocess
        candidates = [
            "hevc_v4l2m2m",  # Rockchip / RK3588
            "hevc_vaapi",
            "hevc_nvenc",
            "libx265",
        ]
        for enc in candidates:
            try:
                r = subprocess.run(
                    ["ffmpeg", "-hide_banner", "-loglevel", "error",
                     "-f", "lavfi", "-i", "color=black:size=64x64:rate=1",
                     "-frames:v", "1", "-c:v", enc, "-f", "null", "-"],
                    capture_output=True, timeout=10,
                )
                if r.returncode == 0:
                    return enc
            except (FileNotFoundError, subprocess.TimeoutExpired):
                continue
        return "libx265"

    async def _monitor(self) -> None:
        if not self._proc or not self._proc.stderr:
            return
        try:
            while True:
                line = await self._proc.stderr.readline()
                if not line:
                    break
                msg = line.decode(errors="replace").strip()
                if msg and "error" in msg.lower():
                    log.warning("Video stream: %s", msg)
        except Exception:
            pass
        log.warning("Video stream exited (rc=%s)", self._proc.returncode if self._proc else "?")

    def state_dict(self) -> dict[str, Any]:
        return {
            "active": self.active,
            "rtp_port": self._config.rtp_port,
            "width": self._config.width,
            "height": self._config.height,
            "fps": self._config.fps,
            "encoder": self._encoder,
        }
