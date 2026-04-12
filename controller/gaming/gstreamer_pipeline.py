# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
GStreamer pipeline for Moonlight video encoding and RTP streaming.

Implements configurable video encoding pipelines:
  - VAAPI encoding (Intel iGPU)
  - NVENC encoding (NVIDIA GPU)
  - QuickSync encoding (Intel QSV)
  - Software encoding (libx264/libx265)
  - AV1 encoding (via rav1e/svt-av1)

Features:
  - TOML/JSON configurable pipeline strings (Wolf pattern)
  - Per-session pipeline instances
  - RTP packetiser + FEC integration
  - Gamescope integration hooks (XWayland + FSR + HDR)
  - DMA-BUF zero-copy path (stub for V2.0)

Architecture:
  Frame source (v4l2loopback, VNC, virtual desktop)
      │
      ├──→ GStreamer pipeline (encode → packetise → FEC)
      │         │
      │         ├──→ RTP over UDP (video)
      │         └──→ RTCP over UDP (feedback)
      │
      └──→ Moonlight protocol (session negotiation)
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
import shutil
import subprocess
import tempfile
from dataclasses import dataclass, field
from enum import Enum, auto
from pathlib import Path
from typing import Any, Callable

import toml

log = logging.getLogger("ozma.moonlight.gstreamer")

# Supported encoders
ENCODE_BACKENDS = {
    "nvenc": {"codec": "h264_nvenc", "h265": "hevc_nvenc", "av1": "av1_nvenc"},
    "vaapi": {"codec": "h264_vaapi", "h265": "hevc_vaapi"},
    "qsv": {"codec": "h264_qsv", "h265": "hevc_qsv", "av1": "av1_qsv"},
    "software": {"codec": "libx264", "h265": "libx265", "av1": "libaom-av1"},
}


class EncoderType(Enum):
    """Video encoder types."""
    NVENC = auto()
    VAAPI = auto()
    QSV = auto()
    SOFTWARE = auto()
    AV1_NVENC = auto()
    AV1_SOFTWARE = auto()


@dataclass
class PipelineConfig:
    """GStreamer pipeline configuration."""
    name: str = "default"

    # Input
    input_source: str = "v4l2"  # v4l2 | vnc | wayland | file
    input_device: str = "/dev/video0"
    input_format: str = "NV12"
    input_width: int = 1920
    input_height: int = 1080
    input_framerate: int = 60

    # Encoding
    encoder: str = "auto"  # auto | nvenc | vaapi | qsv | software
    codec: str = "auto"  # auto | h264 | h265 | av1
    bitrate_kbps: int = 10000
    gop_size: int = 60
    quality_preset: str = "p4"  # NVENC: p1-p7, vaapi: 1-7

    # Output
    rtp_destination: str = "127.0.0.1"
    rtp_port: int = 8000
    rtcp_port: int = 8001
    enable_fec: bool = True
    fec_percentage: int = 25

    # Gamescope integration
    gamescope_enabled: bool = False
    gamescope_xwayland: bool = True
    gamescope_fsr: bool = False
    gamescope_fsr_mode: str = "quality"  # quality | balanced | performance
    gamescope_hdr: bool = False

    # DMA-BUF zero-copy (V2.0)
    dmabuf_zero_copy: bool = False

    def to_dict(self) -> dict[str, Any]:
        return {
            "name": self.name,
            "input_source": self.input_source,
            "input_device": self.input_device,
            "input_format": self.input_format,
            "input_width": self.input_width,
            "input_height": self.input_height,
            "input_framerate": self.input_framerate,
            "encoder": self.encoder,
            "codec": self.codec,
            "bitrate_kbps": self.bitrate_kbps,
            "gop_size": self.gop_size,
            "quality_preset": self.quality_preset,
            "rtp_destination": self.rtp_destination,
            "rtp_port": self.rtp_port,
            "rtcp_port": self.rtcp_port,
            "enable_fec": self.enable_fec,
            "fec_percentage": self.fec_percentage,
            "gamescope_enabled": self.gamescope_enabled,
            "gamescope_xwayland": self.gamescope_xwayland,
            "gamescope_fsr": self.gamescope_fsr,
            "gamescope_fsr_mode": self.gamescope_fsr_mode,
            "gamescope_hdr": self.gamescope_hdr,
            "dmabuf_zero_copy": self.dmabuf_zero_copy,
        }

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> "PipelineConfig":
        return cls(**d)

    @classmethod
    def from_toml_file(cls, path: str) -> "PipelineConfig":
        """Load config from TOML file."""
        with open(path) as f:
            data = toml.load(f)
        return cls.from_dict(data)

    def to_toml_file(self, path: str) -> None:
        """Save config to TOML file."""
        with open(path, "w") as f:
            toml.dump(self.to_dict(), f)


class GStreamerPipeline:
    """
    Manages a GStreamer pipeline for video encoding and RTP streaming.

    Supports dynamic pipeline reconfiguration without restart.
    """

    def __init__(self, config: PipelineConfig | None = None) -> None:
        self._config = config or PipelineConfig()
        self._pipeline: asyncio.subprocess.Process | None = None
        self._running = False
        self._task: asyncio.Task | None = None
        self._encoder: str | None = None
        self._h265_encoder: str | None = None
        self._av1_encoder: str | None = None

        # Pipeline state
        self._frames_encoded = 0
        self._frames_dropped = 0
        self._bitrate_actual = 0.0
        self._latency_ms = 0.0

        # Callbacks
        self._on_error: Callable[[str], None] | None = None
        self._on_stats: Callable[[dict], None] | None = None

    async def start(self) -> bool:
        """Start the GStreamer pipeline."""
        if self._running:
            return True

        # Detect encoder if auto
        encoder = self._config.encoder
        if encoder == "auto":
            encoder = await _detect_encoder()
            log.info("Auto-detected encoder: %s", encoder)

        self._config.encoder = encoder

        # Get encoder names
        backend = ENCODE_BACKENDS.get(encoder, ENCODE_BACKENDS["software"])
        self._encoder = backend.get("codec", "libx264")
        self._h265_encoder = backend.get("h265", "libx265")

        # Select codec
        codec = self._config.codec
        if codec == "auto":
            codec = "h264"

        # Build pipeline
        pipeline_str = self._build_pipeline()

        log.info("Starting GStreamer pipeline: %s", pipeline_str[:100] + "...")

        try:
            self._pipeline = await asyncio.create_subprocess_exec(
                "gst-launch-1.0",
                "-e",
                pipeline_str,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            self._running = True

            # Monitor pipeline
            self._task = asyncio.create_task(
                self._monitor_pipeline(),
                name="gstreamer-pipeline"
            )

            return True
        except Exception as e:
            # Clean up any partially created resources
            log.error("Failed to start GStreamer pipeline: %s", e)
            # Kill the process if it was created but monitoring failed
            if self._pipeline:
                try:
                    self._pipeline.terminate()
                    await asyncio.wait_for(self._pipeline.wait(), timeout=2.0)
                except Exception:
                    self._pipeline.kill()
                    await self._pipeline.wait()
                self._pipeline = None
            self._running = False
            return False

    async def stop(self) -> None:
        """Stop the GStreamer pipeline."""
        self._running = False

        if self._pipeline:
            self._pipeline.terminate()
            try:
                await asyncio.wait_for(self._pipeline.wait(), timeout=5.0)
            except asyncio.TimeoutError:
                self._pipeline.kill()
                await self._pipeline.wait()
            self._pipeline = None

        if self._task:
            self._task.cancel()
            try:
                await self._task
            except asyncio.CancelledError:
                pass
            self._task = None

    async def restart(self, config: PipelineConfig | None = None) -> bool:
        """Restart the pipeline with a new configuration."""
        await self.stop()
        if config:
            self._config = config
        return await self.start()

    def _build_pipeline(self) -> str:
        """Build the GStreamer pipeline string."""
        cfg = self._config

        # Select encoder based on codec
        encoder = self._encoder
        if cfg.codec == "h265":
            encoder = self._h265_encoder or encoder

        # Build pipeline components
        components = []

        # Input source
        input_src = self._build_input_source()
        components.append(input_src)

        # Video filter chain
        filter_chain = self._build_filter_chain()
        if filter_chain:
            components.append(filter_chain)

        # Encoder
        encoder_caps = self._build_encoder_caps(encoder)
        components.append(encoder_caps)

        # RTP payloader
        rtp_pay = self._build_rtp_payloader(encoder)
        components.append(rtp_pay)

        # FEC
        if cfg.enable_fec:
            fec = self._build_fec()
            if fec:
                components.append(fec)

        # RTP sink
        rtp_sink = self._build_rtp_sink()
        components.append(rtp_sink)

        # RTCP sink (optional)
        if cfg.rtcp_port:
            rtcp_sink = self._build_rtcp_sink()
            if rtcp_sink:
                components.append(rtcp_sink)

        return " ! ".join(components)

    def _build_input_source(self) -> str:
        """Build the input source element."""
        cfg = self._config

        if cfg.input_source == "v4l2":
            # V4L2 input (for HDMI capture)
            return (
                f"v4l2src device={cfg.input_device} ! "
                f"video/x-raw, format={cfg.input_format}, "
                f"width={cfg.input_width}, height={cfg.input_height}, "
                f"framerate={cfg.input_framerate}/1"
            )
        elif cfg.input_source == "vnc":
            # VNC framebuffer (for VMs)
            return (
                f"vncsrc location={cfg.input_device} ! "
                f"video/x-raw, format=BGR, "
                f"width={cfg.input_width}, height={cfg.input_height}, "
                f"framerate={cfg.input_framerate}/1 ! "
                "videoconvert ! video/x-raw, format=NV12"
            )
        elif cfg.input_source == "wayland":
            # Wayland compositor capture
            return (
                "waylandsink ! "
                f"video/x-raw, format=NV12, "
                f"width={cfg.input_width}, height={cfg.input_height}, "
                f"framerate={cfg.input_framerate}/1"
            )
        elif cfg.input_source == "file":
            # File input (for testing)
            return f"filesrc location={cfg.input_device} ! decodebin"

        return " videotestsrc is-live=true ! video/x-raw, format=NV12, width=1920, height=1080, framerate=60/1"

    def _build_filter_chain(self) -> str:
        """Build video filter chain (scaling, colorspace, etc.)."""
        cfg = self._config
        filters = []

        # Scale if needed
        if cfg.input_width > 1920 or cfg.input_height > 1080:
            # Calculate scaled size while preserving aspect
            ratio = cfg.input_width / cfg.input_height
            if ratio > 1920 / 1080:
                w, h = 1920, int(1920 / ratio)
            else:
                w, h = int(1080 * ratio), 1080
            # Ensure even dimensions for encoder
            w, h = w - (w % 2), h - (h % 2)
            filters.append(f"videoscale ! video/x-raw, width={w}, height={h}")

        # Convert colorspace if needed
        if cfg.input_format != "NV12":
            filters.append("videoconvert ! video/x-raw, format=NV12")

        # Gamescope FSR upscaling
        if cfg.gamescope_enabled and cfg.gamescope_fsr:
            # FSR is typically a shader in gamescope, but we can add scaling
            filters.append("videoscale ! video/x-raw, width=1920, height=1080")

        return " ! ".join(filters) if filters else ""

    def _build_encoder_caps(self, encoder: str) -> str:
        """Build encoder configuration."""
        cfg = self._config
        bitrate = cfg.bitrate_kbps * 1000

        if "vaapi" in encoder:
            return (
                f"videoconvert ! "
                f"video/x-raw, format=nv12 ! "
                f"vaapih264enc rate-control=cbr bitrate={bitrate // 1000} gop-size={cfg.gop_size} ! "
                "video/x-h264, profile=main"
            )
        elif "nvenc" in encoder:
            return (
                f"videoconvert ! "
                f"video/x-raw, format=nv12 ! "
                f"nvh264enc bitrate={bitrate // 1000} gop-size={cfg.gop_size} preset={cfg.quality_preset} ! "
                "video/x-h264, profile=main"
            )
        elif "qsv" in encoder:
            return (
                f"videoconvert ! "
                f"video/x-raw, format=nv12 ! "
                f"qsvh264enc bitrate={bitrate // 1000} gop-size={cfg.gop_size} ! "
                "video/x-h264, profile=main"
            )
        elif "av1" in encoder.lower() or "aom" in encoder.lower():
            return (
                f"videoconvert ! "
                f"video/x-raw, format=nv12 ! "
                f"{encoder} bitrate={bitrate // 1000} passes=2 ! "
                "video/x-av1, profile=main"
            )
        else:
            # Software encoder
            return (
                f"videoconvert ! "
                f"video/x-raw, format=yuv420p ! "
                f"{encoder} bitrate={bitrate} speed-preset={cfg.quality_preset} gop-size={cfg.gop_size} ! "
                "video/x-h264, profile=main"
            )

    def _build_rtp_payloader(self, encoder: str) -> str:
        """Build RTP payloader for the encoded stream."""
        if "h265" in encoder or "hevc" in encoder:
            return "rtph265pay config-interval=1 pt=97 ! rtcpmux"
        elif "av1" in encoder.lower() or "aom" in encoder.lower():
            return "rtpav1pay config-interval=1 pt=98 ! rtcpmux"
        else:
            return "rtph264pay config-interval=1 pt=96 ! rtcpmux"

    def _build_fec(self) -> str | None:
        """Build Forward Error Correction element (optional)."""
        cfg = self._config
        if not cfg.enable_fec:
            return None

        # Use rtpjitterbuffer for FEC (simplified)
        # In production, use rtpmsbc or similar for proper FEC
        percentage = cfg.fec_percentage
        return (
            f"rtpjitterbuffer latency=100 adaptive=true ! "
            f"rtpfecpay percentage={percentage} ! "
            f"rtpjitterbuffer latency=100 adaptive=true"
        )

    def _build_rtp_sink(self) -> str:
        """Build RTP UDP sink."""
        cfg = self._config
        return (
            f"udpsink host={cfg.rtp_destination} port={cfg.rtp_port} "
            "sync=false async=false"
        )

    def _build_rtcp_sink(self) -> str:
        """Build RTCP UDP sink."""
        cfg = self._config
        return (
            f"udpsink host={cfg.rtp_destination} port={cfg.rtcp_port} "
            "sync=false async=false"
        )

    async def _monitor_pipeline(self) -> None:
        """Monitor pipeline stderr for errors and stats."""
        if not self._pipeline or not self._pipeline.stderr:
            return

        try:
            while self._running:
                line = await self._pipeline.stderr.readline()
                if not line:
                    break

                msg = line.decode("utf-8", errors="replace").strip()

                # Check for errors
                if "ERROR" in msg.upper() or "failed" in msg.lower():
                    log.error("GStreamer pipeline error: %s", msg)
                    if self._on_error:
                        self._on_error(msg)

                # Parse stats (simplified)
                if "bitrate" in msg:
                    # Extract bitrate from message
                    try:
                        import re
                        match = re.search(r"bitrate:\s*([\d.]+)\s*(\w+)", msg, re.IGNORECASE)
                        if match:
                            value = float(match.group(1))
                            unit = match.group(2).lower()
                            if unit == "kbit/s" or unit == "kbps":
                                self._bitrate_actual = value
                            elif unit == "mbit/s" or unit == "mbps":
                                self._bitrate_actual = value * 1000
                    except Exception:
                        pass

                # Check for latency stats
                if "latency" in msg:
                    try:
                        import re
                        match = re.search(r"latency:\s*([\d.]+)\s*ms", msg, re.IGNORECASE)
                        if match:
                            self._latency_ms = float(match.group(1))
                    except Exception:
                        pass

                # Check for frame stats
                if "frames encoded" in msg:
                    try:
                        import re
                        match = re.search(r"(\d+)\s+frames\s+encoded", msg, re.IGNORECASE)
                        if match:
                            self._frames_encoded = int(match.group(1))
                    except Exception:
                        pass

                # Check for dropped frames
                if "frames dropped" in msg:
                    try:
                        import re
                        match = re.search(r"(\d+)\s+frames\s+dropped", msg, re.IGNORECASE)
                        if match:
                            self._frames_dropped = int(match.group(1))
                    except Exception:
                        pass

                # Call stats callback if set
                if self._on_stats:
                    self._on_stats({
                        "frames_encoded": self._frames_encoded,
                        "frames_dropped": self._frames_dropped,
                        "bitrate_kbps": self._bitrate_actual / 1000,
                        "latency_ms": self._latency_ms,
                    })

        except asyncio.CancelledError:
            pass
        except Exception as e:
            log.error("Pipeline monitor error: %s", e)

    def set_on_error(self, callback: Callable[[str], None]) -> None:
        """Set error callback."""
        self._on_error = callback

    def set_on_stats(self, callback: Callable[[dict], None]) -> None:
        """Set stats callback."""
        self._on_stats = callback

    def get_stats(self) -> dict[str, Any]:
        """Get current pipeline stats."""
        return {
            "frames_encoded": self._frames_encoded,
            "frames_dropped": self._frames_dropped,
            "bitrate_kbps": self._bitrate_actual / 1000,
            "latency_ms": self._latency_ms,
            "running": self._running,
        }


# ── Encoder detection ────────────────────────────────────────────────────────

async def _detect_encoder() -> str:
    """Detect the best available hardware encoder."""
    # Check for NVIDIA
    if shutil.which("nvidia-smi"):
        result = await asyncio.create_subprocess_exec(
            "nvidia-smi", "--query-gpu=name", "--format=csv,noheader",
            stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.DEVNULL,
        )
        out, _ = await result.communicate()
        if result.returncode == 0 and out.strip():
            log.info("Encoder detection: NVENC detected")
            return "nvenc"

    # Check for Intel QSV/VAAPI
    if Path("/dev/dri").exists():
        # Check for Intel GPU
        for render in Path("/dev/dri").glob("renderD*"):
            # Try to test VAAPI
            result = await asyncio.create_subprocess_exec(
                "ffmpeg", "-hide_banner", "-hwaccel", "vaapi",
                "-hwaccel_device", str(render),
                "-f", "lavfi", "-i", "nullsrc=s=16x16:d=0.01",
                "-vf", "format=nv12,hwupload",
                "-vcodec", "h264_vaapi", "-f", "null", "-",
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.DEVNULL,
            )
            await result.communicate()
            if result.returncode == 0:
                # Check if it's Intel or AMD
                vendor = "unknown"
                try:
                    # Try to get GPU vendor
                    result2 = await asyncio.create_subprocess_exec(
                        "lspci", "-v", "-s", str(render),
                        stdout=asyncio.subprocess.PIPE,
                        stderr=asyncio.subprocess.DEVNULL,
                    )
                    out2, _ = await result2.communicate()
                    if out2:
                        vendor = out2.decode().lower()
                except Exception:
                    pass

                if "intel" in vendor or "igpu" in vendor:
                    log.info("Encoder detection: Intel QSV/VAAPI detected")
                    return "qsv"
                log.info("Encoder detection: VAAPI detected")
                return "vaapi"

    log.info("Encoder detection: software fallback")
    return "software"


async def _test_encoder(encoder: str, codec: str = "h264") -> bool:
    """Test if an encoder works with a null encode."""
    encoders = ENCODE_BACKENDS.get(encoder, {})
    enc_name = encoders.get(codec, encoders.get("codec", "libx264"))

    if "vaapi" in enc_name or "qsv" in enc_name:
        # Hardware encoding test
        cmd = [
            "ffmpeg", "-hide_banner", "-loglevel", "error",
            "-f", "lavfi", "-i", "color=black:size=64x64:rate=1",
            "-frames:v", "1", "-c:v", enc_name,
        ]
        # Add hwaccel for VAAPI
        if "vaapi" in enc_name:
            cmd = [
                "ffmpeg", "-hide_banner", "-loglevel", "error",
                "-init_hw_device", "vaapi=hw:/dev/dri/renderD128",
                "-filter_hw_device", "hw",
                "-f", "lavfi", "-i", "color=black:size=64x64:rate=1",
                "-vf", "format=nv12,hwupload",
                "-frames:v", "1", "-c:v", enc_name,
            ]
    else:
        # Software encoding test
        cmd = [
            "ffmpeg", "-hide_banner", "-loglevel", "error",
            "-f", "lavfi", "-i", "color=black:size=64x64:rate=1",
            "-frames:v", "1", "-c:v", enc_name,
        ]

    cmd.extend(["-f", "null", "-"])

    try:
        proc = await asyncio.create_subprocess_exec(*cmd)
        rc = await proc.wait()
        return rc == 0
    except Exception:
        return False


# ── Gamescope integration hooks ──────────────────────────────────────────────

class GamescopeIntegration:
    """
    Handles Gamescope-specific integration for Moonlight streaming.

    Provides hooks for:
      - XWayland integration
      - FSR upscaling
      - HDR passthrough
    """

    def __init__(self, display: str = ":99") -> None:
        self._display = display
        self._xwayland_proc: asyncio.subprocess.Process | None = None
        self._gamescope_proc: asyncio.subprocess.Process | None = None

    async def start_xwayland(self) -> str:
        """Start XWayland server."""
        self._xwayland_proc = await asyncio.create_subprocess_exec(
            "Xwayland", self._display, "-rootless", "-noreset",
            "-accessx", "4",
            stdout=asyncio.subprocess.DEVNULL,
            stderr=asyncio.subprocess.PIPE,
        )
        # Wait for XWayland to start
        await asyncio.sleep(1)
        return self._display

    async def stop_xwayland(self) -> None:
        """Stop XWayland server."""
        if self._xwayland_proc:
            self._xwayland_proc.terminate()
            try:
                await asyncio.wait_for(self._xwayland_proc.wait(), timeout=5.0)
            except asyncio.TimeoutError:
                self._xwayland_proc.kill()
                await self._xwayland_proc.wait()
            self._xwayland_proc = None

    async def start_gamescope(
        self,
        width: int = 1920,
        height: int = 1080,
        fps: int = 60,
        fsr_enabled: bool = False,
        fsr_mode: str = "quality",
        hdr_enabled: bool = False,
    ) -> asyncio.subprocess.Process:
        """Start Gamescope for XWayland gaming."""
        cmd = [
            "gamescope",
            "--width", str(width),
            "--height", str(height),
            "--fps", str(fps),
            "--xwayland",
            "--xwayland-display", self._display,
        ]

        if fsr_enabled:
            cmd.extend(["--fsr", fsr_mode])

        if hdr_enabled:
            cmd.extend(["--hdr-enabled"])

        self._gamescope_proc = await asyncio.create_subprocess_exec(
            *cmd,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )

        # Wait for Gamescope to start
        await asyncio.sleep(2)

        return self._gamescope_proc

    async def stop_gamescope(self) -> None:
        """Stop Gamescope."""
        if self._gamescope_proc:
            self._gamescope_proc.terminate()
            try:
                await asyncio.wait_for(self._gamescope_proc.wait(), timeout=5.0)
            except asyncio.TimeoutError:
                self._gamescope_proc.kill()
                await self._gamescope_proc.wait()
            self._gamescope_proc = None


# ── Pipeline manager ─────────────────────────────────────────────────────────

class PipelineManager:
    """
    Manages multiple GStreamer pipeline instances.

    Each stream session gets its own pipeline instance.
    """

    def __init__(self, data_dir: Path) -> None:
        self._data_dir = data_dir
        self._data_dir.mkdir(parents=True, exist_ok=True)

        self._pipelines: dict[str, GStreamerPipeline] = {}
        self._configs: dict[str, PipelineConfig] = {}

    async def create_pipeline(
        self,
        session_id: str,
        config: PipelineConfig | None = None,
    ) -> GStreamerPipeline:
        """Create and start a new pipeline for a session."""
        cfg = config or PipelineConfig()
        cfg.name = session_id

        pipeline = GStreamerPipeline(cfg)
        if await pipeline.start():
            self._pipelines[session_id] = pipeline
            self._configs[session_id] = cfg
            log.info("Created pipeline for session %s", session_id)
        else:
            log.error("Failed to create pipeline for session %s", session_id)

        return pipeline

    async def get_pipeline(self, session_id: str) -> GStreamerPipeline | None:
        """Get an existing pipeline by session ID."""
        return self._pipelines.get(session_id)

    async def remove_pipeline(self, session_id: str) -> None:
        """Remove and stop a pipeline."""
        if session_id in self._pipelines:
            await self._pipelines[session_id].stop()
            del self._pipelines[session_id]
            del self._configs[session_id]
            log.info("Removed pipeline for session %s", session_id)

    async def stop_all(self) -> None:
        """Stop all pipelines."""
        for pipeline in self._pipelines.values():
            await pipeline.stop()
        self._pipelines.clear()
        self._configs.clear()
