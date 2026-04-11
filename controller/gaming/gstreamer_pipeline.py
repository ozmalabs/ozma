# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
GStreamer encoding pipeline for Moonlight streaming.

Provides configurable video/audio encoding pipelines with hardware
acceleration support (VAAPI, NVENC, QuickSync) and RTP/FEC output.

Pipeline structure:

  Source → Video Filter → Encoder → RTP Packetiser → FEC → Network

Features:
  - Hardware encoders: NVENC, VAAPI, QuickSync, V4L2M2M
  - Software fallback: libx264, libx265, libaom-av1
  - Forward Error Correction (FEC)
  - Configurable via TOML/JSON
  - Gamescope integration hooks (XWayland + FSR + HDR)

Configuration examples:

  [pipeline]
  type = "hardware"
  video_encoder = "nvenc"
  audio_encoder = "opus"

  [pipeline.sources]
  display = { type = "x11", display = ":0" }
  capture = { type = "v4l2", device = "/dev/video0" }

  [pipeline.outputs]
  moonlight = { type = "rtp", host = "127.0.0.1", port = 47994 }

See: https://gstreamer.freedesktop.org/documentation/
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
import shlex
import shutil
import subprocess
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Optional

log = logging.getLogger("ozma.controller.gaming.gstreamer_pipeline")

# ── Constants ────────────────────────────────────────────────────────────────

# Hardware encoder capabilities
HARDWARE_ENCODERS = {
    "nvenc": {"name": "NVENC", "codecs": ["h264", "hevc", "av1"], "device": "cuda"},
    "vaapi": {"name": "VAAPI", "codecs": ["h264", "hevc"], "device": "dri"},
    "qsv": {"name": "Quick Sync", "codecs": ["h264", "hevc", "av1"], "device": "dri"},
    "v4l2m2m": {"name": "V4L2 M2M", "codecs": ["h264", "hevc"], "device": "video"},
}

SOFTWARE_ENCODERS = {
    "libx264": {"name": "libx264", "codecs": ["h264"]},
    "libx265": {"name": "libx265", "codecs": ["hevc"]},
    "libaom-av1": {"name": "libaom-av1", "codecs": ["av1"]},
    "svt-av1": {"name": "SVT-AV1", "codecs": ["av1"]},
}

# Default resolutions and FPS
DEFAULT_RESOLUTIONS = {
    "720p": {"width": 1280, "height": 720, "fps": 60},
    "1080p": {"width": 1920, "height": 1080, "fps": 60},
    "1440p": {"width": 2560, "height": 1440, "fps": 60},
    "4k": {"width": 3840, "height": 2160, "fps": 60},
}

# Default encoder profiles
DEFAULT_ENCODER_PROFILES = {
    "nvenc": {"preset": "p4", "tune": "ll", "rc": "cbr"},
    "vaapi": {"qp": 24},
    "qsv": {"preset": "veryfast"},
    "software": {"preset": "ultrafast", "tune": "zerolatency"},
}

# ─── Data Models ─────────────────────────────────────────────────────────────

@dataclass
class EncoderConfig:
    """Encoder configuration."""
    name: str  # Encoder name (nvenc, vaapi, libx264, etc.)
    codec: str  # h264, hevc, av1
    preset: str = ""  # Encoder preset
    tune: str = ""  # Encoder tune
    rc: str = "cbr"  # Rate control: cbr, vbr, cq
    bitrate_kbps: int = 10000
    max_bitrate_kbps: int = 0  # 0 = same as bitrate
    bufsize_kbps: int = 0  # 0 = auto
    qp: int = 24  # Quality parameter for CQ mode
    profile: str = ""  # Encoder profile
    level: str = ""  # Encoder level
    hardware: bool = True  # Hardware encoder
    device: str = ""  # Hardware device path

    @property
    def encoder_name(self) -> str:
        """Get the GStreamer encoder element name."""
        if self.codec == "h264":
            if self.name == "nvenc":
                return "h264_nvenc"
            elif self.name == "vaapi":
                return "h264_vaapi"
            elif self.name == "qsv":
                return "h264_qsv"
            elif self.name == "v4l2m2m":
                return "h264_v4l2m2menc"
            else:
                return "libx264"
        elif self.codec == "hevc":
            if self.name == "nvenc":
                return "hevc_nvenc"
            elif self.name == "vaapi":
                return "hevc_vaapi"
            elif self.name == "qsv":
                return "hevc_qsv"
            elif self.name == "v4l2m2m":
                return "h265_v4l2m2menc"
            else:
                return "libx265"
        elif self.codec == "av1":
            if self.name == "qsv":
                return "av1_qsv"
            else:
                return "libaom-av1"
        return "identity"

    @property
    def caps_string(self) -> str:
        """Get the caps string for the encoder output."""
        return f"video/x-{self.codec}, profile=(string)main, width=(int)1920, height=(int)1080, framerate=(fraction)60/1"


@dataclass
class SourceConfig:
    """Video source configuration."""
    type: str = "display"  # display, v4l2, file, rtmp
    display: str = ""
    device: str = ""
    width: int = 1920
    height: int = 1080
    fps: int = 60
    format: str = "NV12"  # Pixel format
    capture_method: str = "auto"  # kms, x11, wayland, v4l2

    def to_gst_string(self) -> str:
        """Generate GStreamer source element string."""
        if self.type == "display":
            if self.display:
                return f"ximagesrc display={self.display} ! videoconvert"
            else:
                return "ximagesrc ! videoconvert"
        elif self.type == "v4l2":
            return f"v4l2src device={self.device} ! videoconvert"
        elif self.type == "file":
            return f"filesrc location={self.device} ! decodebin"
        elif self.type == "rtmp":
            return f"rtmpsrc location={self.device} ! decodebin"
        return "autovideosrc ! videoconvert"


@dataclass
class OutputConfig:
    """Output configuration."""
    type: str = "rtp"  # rtp, file, rtmp, webrtc
    host: str = "127.0.0.1"
    port: int = 47994
    port_rtcp: int = 47995
    payload_type: int = 96
    ssrc: int = 0
    fec_enabled: bool = True
    fec_percentage: int = 20  # 20% redundancy

    def to_gst_string(self) -> str:
        """Generate GStreamer output element string."""
        if self.type == "rtp":
            fec_str = "fec enc=true method=rs2093 percentage=20 ! " if self.fec_enabled else ""
            return (
                f"{fec_str}"
                f"rtp{self.payload_type}pay ! "
                f"udpsink host={self.host} port={self.port}"
            )
        elif self.type == "file":
            return f"filesink location={self.host}"
        elif self.type == "rtmp":
            return f"rtmpsink location={self.host}"
        return "fakesink"


@dataclass
class PipelineConfig:
    """Complete pipeline configuration."""
    name: str = "default"
    pipeline_type: str = "hardware"  # hardware, software, custom
    video_encoder: EncoderConfig = field(default_factory=EncoderConfig)
    audio_encoder: str = "opus"  # opus, aac, vorbis
    audio_bitrate_kbps: int = 160
    sources: list[SourceConfig] = field(default_factory=list)
    outputs: list[OutputConfig] = field(default_factory=list)

    # Filters
    scale: bool = False
    scale_width: int = 1920
    scale_height: int = 1080
    crop: bool = False
    crop_left: int = 0
    crop_top: int = 0
    crop_width: int = 0
    crop_height: int = 0

    # Performance
    max_cpu_usage: int = 50
    low_latency: bool = True
    debug: bool = False

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> "PipelineConfig":
        """Create config from dictionary."""
        video_enc = d.get("video_encoder", {})
        enc = EncoderConfig(
            name=video_enc.get("name", "nvenc"),
            codec=video_enc.get("codec", "h264"),
            preset=video_enc.get("preset", ""),
            tune=video_enc.get("tune", ""),
            rc=video_enc.get("rc", "cbr"),
            bitrate_kbps=video_enc.get("bitrate_kbps", 10000),
            max_bitrate_kbps=video_enc.get("max_bitrate_kbps", 0),
            bufsize_kbps=video_enc.get("bufsize_kbps", 0),
            qp=video_enc.get("qp", 24),
            hardware=video_enc.get("hardware", True),
            device=video_enc.get("device", ""),
        )

        source_dicts = d.get("sources", [])
        sources = [SourceConfig(**src) for src in source_dicts] if source_dicts else [
            SourceConfig(width=d.get("width", 1920), height=d.get("height", 1080), fps=d.get("fps", 60))
        ]

        output_dicts = d.get("outputs", [])
        outputs = [OutputConfig(**out) for out in output_dicts] if output_dicts else [
            OutputConfig(host=d.get("host", "127.0.0.1"), port=d.get("port", 47994))
        ]

        return cls(
            name=d.get("name", "default"),
            pipeline_type=d.get("pipeline_type", "hardware"),
            video_encoder=enc,
            audio_encoder=d.get("audio_encoder", "opus"),
            audio_bitrate_kbps=d.get("audio_bitrate_kbps", 160),
            sources=sources,
            outputs=outputs,
            scale=d.get("scale", False),
            scale_width=d.get("scale_width", 1920),
            scale_height=d.get("scale_height", 1080),
            max_cpu_usage=d.get("max_cpu_usage", 50),
            low_latency=d.get("low_latency", True),
            debug=d.get("debug", False),
        )

    def to_dict(self) -> dict[str, Any]:
        """Convert to dictionary."""
        return {
            "name": self.name,
            "pipeline_type": self.pipeline_type,
            "video_encoder": {
                "name": self.video_encoder.name,
                "codec": self.video_encoder.codec,
                "preset": self.video_encoder.preset,
                "tune": self.video_encoder.tune,
                "rc": self.video_encoder.rc,
                "bitrate_kbps": self.video_encoder.bitrate_kbps,
                "max_bitrate_kbps": self.video_encoder.max_bitrate_kbps,
                "bufsize_kbps": self.video_encoder.bufsize_kbps,
                "qp": self.video_encoder.qp,
                "hardware": self.video_encoder.hardware,
                "device": self.video_encoder.device,
            },
            "audio_encoder": self.audio_encoder,
            "audio_bitrate_kbps": self.audio_bitrate_kbps,
            "sources": [s.__dict__ for s in self.sources],
            "outputs": [o.__dict__ for o in self.outputs],
            "scale": self.scale,
            "scale_width": self.scale_width,
            "scale_height": self.scale_height,
            "max_cpu_usage": self.max_cpu_usage,
            "low_latency": self.low_latency,
            "debug": self.debug,
        }


# ─── Hardware Detection ──────────────────────────────────────────────────────

class HardwareDetector:
    """Detects available hardware encoders."""

    def __init__(self):
        self._nvenc_available = False
        self._vaapi_available = False
        self._qsv_available = False
        self._v4l2m2m_available = False

    async def detect(self) -> dict[str, bool]:
        """Detect available hardware encoders."""
        results = {
            "nvenc": False,
            "vaapi": False,
            "qsv": False,
            "v4l2m2m": False,
        }

        # Check NVENC
        if shutil.which("nvidia-smi"):
            try:
                result = await asyncio.create_subprocess_exec(
                    "nvidia-smi", "--query-gpu=name", "--format=csv,noheader",
                    stdout=asyncio.subprocess.PIPE,
                    stderr=asyncio.subprocess.PIPE,
                )
                _, _ = await result.communicate()
                if result.returncode == 0:
                    results["nvenc"] = True
            except Exception:
                pass

        # Check VAAPI
        for dev in Path("/dev/dri").glob("renderD*"):
            try:
                result = await asyncio.create_subprocess_exec(
                    "ffmpeg", "-hide_banner", "-hwaccel", "vaapi",
                    "-hwaccel_device", str(dev),
                    "-f", "lavfi", "-i", "nullsrc=s=64x64:d=0.01",
                    "-vf", "format=nv12,hwupload",
                    "-vcodec", "h264_vaapi", "-f", "null", "-",
                    stdout=asyncio.subprocess.DEVNULL,
                    stderr=asyncio.subprocess.DEVNULL,
                )
                await result.communicate()
                if result.returncode == 0:
                    results["vaapi"] = True
                    # Check if Intel QSV
                    try:
                        vendor = Path("/sys/class/drm").joinpath(dev.name.replace("render", "card0")).joinpath("device/vendor").read_text().strip()
                        if vendor == "0x8086":  # Intel
                            results["qsv"] = True
                    except Exception:
                        pass
                    break
            except Exception:
                pass

        # Check V4L2 M2M
        if Path("/dev/v4l2").exists() or Path("/dev/video").exists():
            try:
                result = await asyncio.create_subprocess_exec(
                    "v4l2-ctl", "--list-devices",
                    stdout=asyncio.subprocess.PIPE,
                    stderr=asyncio.subprocess.PIPE,
                )
                out, _ = await result.communicate()
                if b"M2M" in out or b"m2m" in out.lower():
                    results["v4l2m2m"] = True
            except Exception:
                pass

        self._nvenc_available = results["nvenc"]
        self._vaapi_available = results["vaapi"]
        self._qsv_available = results["qsv"]
        self._v4l2m2m_available = results["v4l2m2m"]

        log.info("Hardware encoder detection: %s", results)
        return results

    def get_best_encoder(self, codec: str = "h264") -> tuple[str, bool]:
        """Get the best available encoder for a codec."""
        if codec == "h264":
            if self._nvenc_available:
                return "nvenc", True
            if self._qsv_available:
                return "qsv", True
            if self._vaapi_available:
                return "vaapi", True
            if self._v4l2m2m_available:
                return "v4l2m2m", True
            return "libx264", False

        elif codec == "hevc":
            if self._nvenc_available:
                return "nvenc", True
            if self._qsv_available:
                return "qsv", True
            if self._vaapi_available:
                return "vaapi", True
            if self._v4l2m2m_available:
                return "v4l2m2m", True
            return "libx265", False

        elif codec == "av1":
            if self._qsv_available:
                return "qsv", True
            return "libaom-av1", False

        return "identity", False


# ─── GStreamer Pipeline Manager ──────────────────────────────────────────────

class GStreamerPipelineManager:
    """
    Manages GStreamer encoding pipelines for Moonlight streaming.

    Supports multiple pipeline types:
      - Hardware-accelerated (NVENC, VAAPI, QSV)
      - Software fallback (libx264, libx265)
      - Custom pipelines via configuration
    """

    def __init__(
        self,
        data_dir: Path = Path("/var/lib/ozma/gaming"),
        codec_manager: Any = None,
    ):
        self._data_dir = data_dir
        self._data_dir.mkdir(parents=True, exist_ok=True)
        self._codec_manager = codec_manager
        self._detector = HardwareDetector()
        self._pipelines: dict[str, asyncio.subprocess.Process] = {}
        self._configs: dict[str, PipelineConfig] = {}

        # Default configuration
        self._default_config = PipelineConfig(
            name="default",
            video_encoder=EncoderConfig(name="nvenc", codec="h264"),
            sources=[SourceConfig(width=1920, height=1080, fps=60)],
            outputs=[OutputConfig(host="127.0.0.1", port=47994)],
            low_latency=True,
        )

    async def start(self) -> None:
        """Start the pipeline manager."""
        # Detect available hardware
        encoders = await self._detector.detect()

        # Load saved configurations
        await self._load_configs()

        log.info("GStreamerPipelineManager started with hardware: %s", encoders)

    async def stop(self) -> None:
        """Stop all pipelines."""
        for name, proc in list(self._pipelines.items()):
            if proc.returncode is None:
                proc.terminate()
                try:
                    await asyncio.wait_for(proc.wait(), timeout=5.0)
                except asyncio.TimeoutError:
                    proc.kill()
                    await proc.wait()
            self._pipelines.pop(name, None)

        log.info("GStreamerPipelineManager stopped")

    async def _load_configs(self) -> None:
        """Load saved pipeline configurations."""
        config_file = self._data_dir / "pipelines.json"
        if config_file.exists():
            try:
                data = json.loads(config_file.read_text())
                for name, cfg_dict in data.get("pipelines", {}).items():
                    self._configs[name] = PipelineConfig.from_dict(cfg_dict)
                log.info("Loaded %d pipeline configurations", len(self._configs))
            except Exception as e:
                log.error("Failed to load pipeline configs: %s", e)

    async def _save_configs(self) -> None:
        """Save pipeline configurations."""
        config_file = self._data_dir / "pipelines.json"
        try:
            data = {
                "pipelines": {name: cfg.to_dict() for name, cfg in self._configs.items()},
                "last_save": time.time(),
            }
            config_file.write_text(json.dumps(data, indent=2))
        except Exception as e:
            log.error("Failed to save pipeline configs: %s", e)

    def get_default_config(self) -> PipelineConfig:
        """Get the default pipeline configuration."""
        return self._default_config

    def set_default_config(self, config: PipelineConfig) -> None:
        """Set the default pipeline configuration."""
        self._default_config = config
        self._save_configs()

    def get_config(self, name: str) -> PipelineConfig | None:
        """Get a pipeline configuration by name."""
        return self._configs.get(name)

    def set_config(self, name: str, config: PipelineConfig) -> None:
        """Set a pipeline configuration."""
        self._configs[name] = config
        self._save_configs()

    def delete_config(self, name: str) -> bool:
        """Delete a pipeline configuration."""
        if name in self._configs:
            del self._configs[name]
            self._save_configs()
            return True
        return False

    def list_configs(self) -> list[dict]:
        """List all pipeline configurations."""
        return [
            {"name": name, **cfg.to_dict()}
            for name, cfg in self._configs.items()
        ]

    def generate_pipeline_string(self, config: PipelineConfig) -> str:
        """Generate a GStreamer pipeline string from configuration."""
        # Build source chain
        source_strs = []
        for src in config.sources:
            source_strs.append(src.to_gst_string())

        source_chain = " ! ".join(source_strs) if source_strs else "autovideosrc ! videoconvert"

        # Build video filter chain
        filters = []
        if config.scale:
            filters.append(f"videoscale ! video/x-raw,width={config.scale_width},height={config.scale_height}")
        if config.crop:
            filters.append(f"videocrop left={config.crop_left} top={config.crop_top} right={config.crop_width} bottom={config.crop_height}")

        filter_chain = " ! ".join(filters) if filters else ""

        # Build encoder chain
        enc = config.video_encoder
        encoder_str = f"{enc.encoder_name}"

        encoder_caps = f"video/x-{enc.codec}"
        if enc.profile:
            encoder_caps += f", profile=(string){enc.profile}"
        if enc.level:
            encoder_caps += f", level=(string){enc.level}"

        # Encoder properties
        props = []
        if enc.preset:
            props.append(f"preset={enc.preset}")
        if enc.tune:
            props.append(f"tune={enc.tune}")
        if enc.rc:
            props.append(f"rate-control={enc.rc}")
        props.append(f"bitrate={enc.bitrate_kbps * 1000}")

        encoder_str += " ! " + encoder_caps
        if props:
            encoder_str += " ! " + " ! ".join(props)

        # Build output chain
        output_strs = []
        for out in config.outputs:
            output_strs.append(out.to_gst_string())

        output_chain = " ! ".join(output_strs) if output_strs else "fakesink"

        # Build full pipeline
        pipeline = f"{source_chain} ! {filter_chain} ! {encoder_str} ! {output_chain}"

        # Add audio if configured
        if config.audio_encoder:
            audio_sources = [s for s in config.sources if s.type == "autoaudiosrc"]
            if audio_sources:
                audio_chain = " ! ".join([s.to_gst_string() for s in audio_sources])
                audio_encoder = f"audioconvert ! {config.audio_encoder} ! audio/x-{config.audio_encoder},rate=48000,channels=2"
                audio_output = f"rtp{97}pay ! udpsink host={config.outputs[0].host} port={config.outputs[0].port + 2}"
                pipeline += f" audiosrc ! {audio_chain} ! {audio_encoder} ! {audio_output}"

        return pipeline

    async def start_pipeline(self, name: str, config: PipelineConfig) -> bool:
        """Start a pipeline with the given configuration."""
        # Stop existing pipeline
        await self.stop_pipeline(name)

        # Generate pipeline string
        pipeline = self.generate_pipeline_string(config)

        # Log pipeline for debugging
        if config.debug:
            log.info("Pipeline for '%s': %s", name, pipeline)

        # Check for required elements
        if not shutil.which("gst-launch-1.0"):
            log.error("gst-launch-1.0 not found - GStreamer not installed")
            return False

        # Start the pipeline
        try:
            proc = await asyncio.create_subprocess_exec(
                "gst-launch-1.0", "-v", "--",
                *shlex.split(pipeline),
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            self._pipelines[name] = proc

            # Monitor pipeline
            asyncio.create_task(
                self._monitor_pipeline(name, proc, config),
                name=f"gstreamer-{name}"
            )

            log.info("Started pipeline '%s'", name)
            return True
        except Exception as e:
            log.error("Failed to start pipeline '%s': %s", name, e)
            return False

    async def stop_pipeline(self, name: str) -> None:
        """Stop a pipeline."""
        if name in self._pipelines:
            proc = self._pipelines[name]
            if proc.returncode is None:
                proc.terminate()
                try:
                    await asyncio.wait_for(proc.wait(), timeout=5.0)
                except asyncio.TimeoutError:
                    proc.kill()
                    await proc.wait()
            del self._pipelines[name]
            log.info("Stopped pipeline '%s'", name)

    async def _monitor_pipeline(
        self, name: str, proc: asyncio.subprocess.Process, config: PipelineConfig
    ) -> None:
        """Monitor a pipeline's stderr output."""
        try:
            while proc.returncode is None:
                line = await proc.stderr.readline()
                if not line:
                    break
                msg = line.decode("utf-8", errors="replace").strip()
                if msg:
                    if "ERROR" in msg.upper():
                        log.error("Pipeline '%s' error: %s", name, msg)
                    elif config.debug:
                        log.debug("Pipeline '%s': %s", name, msg)
        except asyncio.CancelledError:
            pass

    # ── Convenience methods ───────────────────────────────────────────────────

    async def start_default_pipeline(self) -> bool:
        """Start the default pipeline."""
        return await self.start_pipeline("default", self._default_config)

    async def start_hardware_pipeline(self, codec: str = "h264") -> bool:
        """Start a hardware-accelerated pipeline."""
        encoder, _ = self._detector.get_best_encoder(codec)
        config = PipelineConfig(
            name=f"hardware-{codec}",
            video_encoder=EncoderConfig(name=encoder, codec=codec),
            sources=[SourceConfig(width=1920, height=1080, fps=60)],
            outputs=[OutputConfig(host="127.0.0.1", port=47994)],
            low_latency=True,
        )
        return await self.start_pipeline(config.name, config)

    async def start_software_pipeline(self, codec: str = "h264") -> bool:
        """Start a software-encoded pipeline."""
        config = PipelineConfig(
            name=f"software-{codec}",
            pipeline_type="software",
            video_encoder=EncoderConfig(name=f"lib{codec}", codec=codec, hardware=False),
            sources=[SourceConfig(width=1920, height=1080, fps=60)],
            outputs=[OutputConfig(host="127.0.0.1", port=47994)],
            low_latency=True,
        )
        return await self.start_pipeline(config.name, config)
