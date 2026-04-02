# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Selectable and tunable codecs for all video streaming paths.

Every video pipeline in ozma (capture cards, distributed RTP, replay
buffer, session recording, remote desktop, broadcast) can use any
supported codec with tunable quality parameters.

Supported codecs:

  H.264 (AVC):
    HW: NVENC, VAAPI, QSV, V4L2M2M, VideoToolbox (macOS)
    SW: libx264
    Use: widest browser compatibility, lower CPU decode

  H.265 (HEVC):
    HW: NVENC, VAAPI, QSV, V4L2M2M
    SW: libx265
    Use: 50% better compression than H.264 at same quality

  AV1:
    HW: NVENC (RTX 40+), VAAPI (Intel Arc+), QSV (Alder Lake+)
    SW: libaom-av1, libsvtav1 (faster), librav1e
    Use: best compression, royalty-free, growing browser support

  VP9:
    HW: VAAPI
    SW: libvpx-vp9
    Use: good compression, wide WebRTC support

  MJPEG:
    HW: V4L2M2M (many SoCs)
    SW: mjpeg
    Use: lowest latency (no inter-frame dependency), high bandwidth

  NDI (Network Device Interface):
    Via NDI SDK or ndi-tools
    Use: broadcast standard, zero-config discovery, very low latency
    Typically runs alongside HLS/MJPEG, not instead of

  Raw / Uncompressed:
    For local capture card → compositor (no encoding needed)

Tunable parameters per codec:
  bitrate         — target bitrate (e.g., "8M", "30M")
  quality         — CRF/CQ/QP value (lower = better quality)
  preset          — encode speed/quality tradeoff
  profile         — codec profile (baseline, main, high)
  keyint          — keyframe interval (frames)
  bframes         — B-frame count (0 for lowest latency)
  latency_mode    — "realtime" (no B-frames, 1-frame VBV) or "quality"
  pixel_format    — nv12, yuv420p, yuv444p
  max_width       — scale down if wider
  max_fps         — cap framerate

Configuration:
  Per-source codec config in controls.yaml or via API:

    codecs:
      default:
        codec: h265
        bitrate: "8M"
        preset: p4
        latency_mode: realtime

      hdmi-0:
        codec: h264
        bitrate: "15M"
        quality: 23
        preset: ultrafast

      rtp-server:
        codec: av1
        bitrate: "4M"
        preset: 8

      ndi:
        enabled: true
        name: "Ozma Output"
        groups: "ozma"
"""

from __future__ import annotations

import asyncio
import logging
import shutil
import subprocess
import time
from dataclasses import dataclass, field
from typing import Any

log = logging.getLogger("ozma.codecs")


@dataclass
class CodecConfig:
    """Configuration for a video codec."""

    codec: str = "h264"           # h264, h265, av1, vp9, mjpeg, ndi, raw
    bitrate: str = "8M"
    quality: int = -1              # CRF/QP (-1 = use bitrate mode)
    preset: str = ""               # Encoder-specific preset
    profile: str = ""              # Codec profile
    keyint: int = 60               # Keyframe interval (frames)
    bframes: int = 0               # B-frames (0 for low latency)
    latency_mode: str = "realtime" # realtime, quality, balanced
    pixel_format: str = "nv12"
    max_width: int = 0             # 0 = no scaling
    max_fps: int = 0               # 0 = no cap
    hw_accel: str = "auto"         # auto, nvenc, vaapi, qsv, v4l2m2m, software

    def to_dict(self) -> dict[str, Any]:
        return {
            "codec": self.codec, "bitrate": self.bitrate,
            "quality": self.quality, "preset": self.preset,
            "profile": self.profile, "keyint": self.keyint,
            "bframes": self.bframes, "latency_mode": self.latency_mode,
            "hw_accel": self.hw_accel, "max_width": self.max_width,
        }

    @classmethod
    def from_dict(cls, d: dict) -> "CodecConfig":
        return cls(**{k: v for k, v in d.items() if hasattr(cls, k)})


@dataclass
class ResolvedEncoder:
    """A resolved encoder ready for ffmpeg."""
    name: str                      # Human-readable name
    ffmpeg_codec: str              # ffmpeg -c:v value
    ffmpeg_flags: list[str]        # Extra ffmpeg flags
    hw_device: str = ""            # VAAPI device path
    vf_prefix: str = ""            # Video filter prefix (hwupload, etc.)

    def to_dict(self) -> dict[str, Any]:
        return {"name": self.name, "codec": self.ffmpeg_codec, "hw_device": self.hw_device}


# ── Codec database ──────────────────────────────────────────────────────────

_CODEC_VARIANTS: dict[str, list[dict]] = {
    "h264": [
        {"name": "NVENC H.264", "codec": "h264_nvenc", "hw": "nvenc",
         "presets": {"realtime": ["-preset", "p4", "-tune", "ll", "-rc", "cbr"],
                     "quality": ["-preset", "p7", "-tune", "hq", "-rc", "vbr"],
                     "balanced": ["-preset", "p5", "-rc", "vbr"]}},
        {"name": "VAAPI H.264", "codec": "h264_vaapi", "hw": "vaapi",
         "presets": {"realtime": ["-qp", "24"], "quality": ["-qp", "20"], "balanced": ["-qp", "22"]},
         "hw_device": "/dev/dri/renderD128"},
        {"name": "QSV H.264", "codec": "h264_qsv", "hw": "qsv",
         "presets": {"realtime": ["-preset", "veryfast"], "quality": ["-preset", "medium"],
                     "balanced": ["-preset", "faster"]}},
        {"name": "V4L2M2M H.264", "codec": "h264_v4l2m2m", "hw": "v4l2m2m",
         "presets": {"realtime": [], "quality": [], "balanced": []}},
        {"name": "Software H.264", "codec": "libx264", "hw": "software",
         "presets": {"realtime": ["-preset", "ultrafast", "-tune", "zerolatency"],
                     "quality": ["-preset", "medium"], "balanced": ["-preset", "veryfast"]}},
    ],
    "h265": [
        {"name": "NVENC H.265", "codec": "hevc_nvenc", "hw": "nvenc",
         "presets": {"realtime": ["-preset", "p4", "-tune", "ll", "-rc", "cbr"],
                     "quality": ["-preset", "p7", "-tune", "hq"], "balanced": ["-preset", "p5"]}},
        {"name": "VAAPI H.265", "codec": "hevc_vaapi", "hw": "vaapi",
         "presets": {"realtime": ["-qp", "24"], "quality": ["-qp", "20"], "balanced": ["-qp", "22"]},
         "hw_device": "/dev/dri/renderD128"},
        {"name": "QSV H.265", "codec": "hevc_qsv", "hw": "qsv",
         "presets": {"realtime": ["-preset", "veryfast"], "quality": ["-preset", "medium"],
                     "balanced": ["-preset", "faster"]}},
        {"name": "V4L2M2M H.265", "codec": "hevc_v4l2m2m", "hw": "v4l2m2m",
         "presets": {"realtime": [], "quality": [], "balanced": []}},
        {"name": "Software H.265", "codec": "libx265", "hw": "software",
         "presets": {"realtime": ["-preset", "ultrafast", "-tune", "zerolatency"],
                     "quality": ["-preset", "medium"], "balanced": ["-preset", "veryfast"]}},
    ],
    "av1": [
        {"name": "NVENC AV1", "codec": "av1_nvenc", "hw": "nvenc",
         "presets": {"realtime": ["-preset", "p4", "-tune", "ll"],
                     "quality": ["-preset", "p7"], "balanced": ["-preset", "p5"]}},
        {"name": "VAAPI AV1", "codec": "av1_vaapi", "hw": "vaapi",
         "presets": {"realtime": ["-qp", "30"], "quality": ["-qp", "24"], "balanced": ["-qp", "27"]},
         "hw_device": "/dev/dri/renderD128"},
        {"name": "QSV AV1", "codec": "av1_qsv", "hw": "qsv",
         "presets": {"realtime": ["-preset", "veryfast"], "quality": ["-preset", "medium"],
                     "balanced": ["-preset", "faster"]}},
        {"name": "SVT-AV1", "codec": "libsvtav1", "hw": "software",
         "presets": {"realtime": ["-preset", "12"], "quality": ["-preset", "6"],
                     "balanced": ["-preset", "8"]}},
    ],
    "vp9": [
        {"name": "VAAPI VP9", "codec": "vp9_vaapi", "hw": "vaapi",
         "presets": {"realtime": ["-qp", "30"], "quality": ["-qp", "24"], "balanced": ["-qp", "27"]},
         "hw_device": "/dev/dri/renderD128"},
        {"name": "Software VP9", "codec": "libvpx-vp9", "hw": "software",
         "presets": {"realtime": ["-speed", "8", "-tile-columns", "2", "-frame-parallel", "1"],
                     "quality": ["-speed", "2"], "balanced": ["-speed", "4"]}},
    ],
    "mjpeg": [
        {"name": "Software MJPEG", "codec": "mjpeg", "hw": "software",
         "presets": {"realtime": ["-q:v", "5"], "quality": ["-q:v", "2"], "balanced": ["-q:v", "3"]}},
    ],
}


class CodecManager:
    """
    Manages codec selection and tuning for all video pipelines.

    Detects available hardware encoders, resolves codec configs to
    ffmpeg command-line arguments, and provides NDI integration.
    """

    def __init__(self) -> None:
        self._available: dict[str, list[str]] = {}  # codec_family → [available variants]
        self._ndi_available = False
        self._configs: dict[str, CodecConfig] = {"default": CodecConfig()}
        self._detect_available()

    def _detect_available(self) -> None:
        """Detect which codec variants are available on this system."""
        for family, variants in _CODEC_VARIANTS.items():
            available = []
            for v in variants:
                if self._test_encoder(v["codec"], v.get("hw_device", "")):
                    available.append(v["codec"])
            self._available[family] = available
            if available:
                log.debug("Codecs [%s]: %s", family, ", ".join(available))

        # NDI detection
        self._ndi_available = shutil.which("ndi-send") is not None or \
                               shutil.which("ffmpeg-ndi") is not None
        if self._ndi_available:
            log.info("NDI available")

    def _test_encoder(self, codec: str, hw_device: str = "") -> bool:
        """Test if an encoder works."""
        cmd = ["ffmpeg", "-hide_banner", "-loglevel", "error",
               "-f", "lavfi", "-i", "color=black:size=64x64:rate=1",
               "-frames:v", "1"]
        if hw_device and "vaapi" in codec:
            cmd = ["ffmpeg", "-hide_banner", "-loglevel", "error",
                   "-init_hw_device", f"vaapi=hw:{hw_device}",
                   "-filter_hw_device", "hw",
                   "-f", "lavfi", "-i", "color=black:size=64x64:rate=1",
                   "-vf", "format=nv12,hwupload",
                   "-frames:v", "1"]
        cmd.extend(["-c:v", codec, "-f", "null", "-"])
        try:
            return subprocess.run(cmd, capture_output=True, timeout=10).returncode == 0
        except Exception:
            return False

    # ── Codec resolution ─────────────────────────────────────────────────────

    def resolve(self, config: CodecConfig | None = None,
                source_id: str = "") -> ResolvedEncoder:
        """Resolve a codec config to ffmpeg encoder arguments."""
        cfg = config or self._configs.get(source_id, self._configs["default"])

        # OCR terminal mode — no ffmpeg encoder needed
        if cfg.codec == "ocr":
            return ResolvedEncoder(
                name="OCR Terminal",
                ffmpeg_codec="ocr",
                ffmpeg_flags=[],
                hw_device="",
                vf_prefix="",
            )

        # Find the best available variant for this codec family
        family = cfg.codec
        available = self._available.get(family, [])

        # Filter by hw_accel preference
        if cfg.hw_accel != "auto" and cfg.hw_accel != "software":
            preferred = [v for v in _CODEC_VARIANTS.get(family, [])
                         if v["hw"] == cfg.hw_accel and v["codec"] in available]
        else:
            preferred = [v for v in _CODEC_VARIANTS.get(family, [])
                         if v["codec"] in available]

        if not preferred:
            # Fallback to software H.264
            preferred = [_CODEC_VARIANTS["h264"][-1]]

        variant = preferred[0]
        preset_flags = variant.get("presets", {}).get(cfg.latency_mode, [])

        # Build flags
        flags = list(preset_flags)
        if cfg.quality >= 0 and "crf" not in str(preset_flags):
            if "nvenc" in variant["codec"]:
                flags.extend(["-cq", str(cfg.quality)])
            elif "vaapi" in variant["codec"]:
                flags.extend(["-qp", str(cfg.quality)])
            else:
                flags.extend(["-crf", str(cfg.quality)])
        elif cfg.bitrate:
            flags.extend(["-b:v", cfg.bitrate])

        if cfg.keyint > 0:
            flags.extend(["-g", str(cfg.keyint)])
        if cfg.bframes == 0:
            if "x264" in variant["codec"] or "x265" in variant["codec"]:
                flags.extend(["-bf", "0"])
        if cfg.profile:
            flags.extend(["-profile:v", cfg.profile])

        # Video filter for VAAPI
        vf = ""
        if "vaapi" in variant["codec"]:
            vf = "format=nv12,hwupload"
            if cfg.max_width > 0:
                vf = f"format=nv12,hwupload,scale_vaapi={cfg.max_width}:-2"

        return ResolvedEncoder(
            name=variant["name"],
            ffmpeg_codec=variant["codec"],
            ffmpeg_flags=flags,
            hw_device=variant.get("hw_device", ""),
            vf_prefix=vf,
        )

    def get_ffmpeg_args(self, config: CodecConfig | None = None,
                         source_id: str = "") -> list[str]:
        """Get complete ffmpeg encoder arguments for a codec config."""
        enc = self.resolve(config, source_id)
        args = []
        if enc.hw_device:
            args.extend(["-init_hw_device", f"vaapi=hw:{enc.hw_device}",
                         "-filter_hw_device", "hw"])
        if enc.vf_prefix:
            args.extend(["-vf", enc.vf_prefix])
        args.extend(["-c:v", enc.ffmpeg_codec])
        args.extend(enc.ffmpeg_flags)
        return args

    # ── Configuration ────────────────────────────────────────────────────────

    def set_config(self, source_id: str, config: CodecConfig) -> None:
        self._configs[source_id] = config

    def get_config(self, source_id: str) -> CodecConfig:
        return self._configs.get(source_id, self._configs["default"])

    def set_default(self, config: CodecConfig) -> None:
        self._configs["default"] = config

    # ── NDI ──────────────────────────────────────────────────────────────────

    @property
    def ndi_available(self) -> bool:
        return self._ndi_available

    def get_ndi_output_args(self, name: str = "Ozma", group: str = "") -> list[str]:
        """Get ffmpeg arguments for NDI output (requires ffmpeg-ndi or NDI SDK)."""
        args = ["-f", "libndi_newtek", "-ndi_name", name]
        if group:
            args.extend(["-ndi_groups", group])
        return args

    # ── Status ───────────────────────────────────────────────────────────────

    def list_available(self) -> dict[str, Any]:
        """List all available codecs and their hardware variants."""
        result = {}
        for family, variants in _CODEC_VARIANTS.items():
            available_names = []
            for v in variants:
                if v["codec"] in self._available.get(family, []):
                    available_names.append({"name": v["name"], "hw": v["hw"], "codec": v["codec"]})
            result[family] = available_names
        result["ndi"] = [{"name": "NDI", "available": self._ndi_available}]
        return result

    def list_configs(self) -> dict[str, dict]:
        return {k: v.to_dict() for k, v in self._configs.items()}

    # ── Async probe (runtime, cached) ────────────────────────────────────────

    async def probe_encoders_async(
        self,
        families: list[str] | None = None,
        force: bool = False,
    ) -> list["EncoderProbeResult"]:
        """
        Probe available encoders using async test encodes.

        Results are cached for 60 seconds. ``force=True`` bypasses the cache.
        Runs all probes concurrently.
        """
        now = time.monotonic()
        cache = getattr(self, "_probe_cache", None)
        if not force and cache and (now - cache["ts"] < 60.0):
            results = cache["results"]
            if families:
                results = [r for r in results if r.codec_family in families]
            return results

        targets = families or list(_CODEC_VARIANTS.keys())
        tasks = []
        for family in targets:
            for v in _CODEC_VARIANTS.get(family, []):
                tasks.append(_probe_encoder_async(v["codec"], v["hw"], family,
                                                   v.get("hw_device", "")))
        results: list[EncoderProbeResult] = await asyncio.gather(*tasks)

        # Update in-memory available list too
        for family in targets:
            avail = [r.encoder for r in results
                     if r.codec_family == family and r.available]
            self._available[family] = avail

        # Always include OCR terminal as a synthetic always-available encoder
        ocr_result = EncoderProbeResult(
            encoder="ocr-terminal",
            codec_family="ocr",
            hw_type="text",
            available=True,
            probe_ms=0,
        )
        results = list(results) + [ocr_result]

        self._probe_cache = {"ts": now, "results": results}  # type: ignore[attr-defined]
        return results

    def get_probe_cache(self) -> list["EncoderProbeResult"]:
        """Return cached probe results without re-probing."""
        cache = getattr(self, "_probe_cache", None)
        return cache["results"] if cache else []

    # ── Adaptive selection ────────────────────────────────────────────────────

    def adaptive_select(
        self,
        current: CodecConfig,
        cpu_pct: float,
        fps_actual: float = 0.0,
        viewer_count: int = 1,
    ) -> CodecConfig | None:
        """
        Suggest a codec config change based on system conditions.

        Returns a new ``CodecConfig`` if a change is recommended, else ``None``.

        Decision logic:
        - CPU > 85% + software encoder → switch to hardware if available
        - CPU > 95% → reduce bitrate 30%, lower resolution
        - CPU < 30% + hardware encoder already → no change (stay efficient)
        - FPS < 20 + using software → reduce bitrate or switch to hardware
        - No viewers → drop to minimal bitrate (but keep streaming)
        - Otherwise → no change
        """
        family = current.codec
        available = self._available.get(family, [])
        hw_available = [v for v in _CODEC_VARIANTS.get(family, [])
                        if v["hw"] != "software" and v["codec"] in available]
        is_software = current.hw_accel in ("software", "") or (
            current.hw_accel == "auto" and not hw_available
        )

        cfg = CodecConfig(
            codec=current.codec,
            bitrate=current.bitrate,
            quality=current.quality,
            preset=current.preset,
            latency_mode=current.latency_mode,
            hw_accel=current.hw_accel,
            max_width=current.max_width,
            keyint=current.keyint,
        )
        changed = False

        if viewer_count == 0:
            # No one watching — minimal quality
            cfg.bitrate = "512k"
            cfg.max_width = 1280
            changed = True

        elif cpu_pct > 95.0:
            # Emergency: reduce everything
            if hw_available and is_software:
                cfg.hw_accel = hw_available[0]["hw"]
                changed = True
            cfg.bitrate = _scale_bitrate(current.bitrate, 0.5)
            cfg.max_width = 1280
            cfg.latency_mode = "realtime"
            changed = True

        elif cpu_pct > 85.0 and is_software and hw_available:
            # High CPU + software → switch to hardware
            cfg.hw_accel = hw_available[0]["hw"]
            cfg.latency_mode = "realtime"
            changed = True

        elif fps_actual > 0 and fps_actual < 20.0:
            # FPS dropping → reduce bitrate
            cfg.bitrate = _scale_bitrate(current.bitrate, 0.7)
            if is_software and hw_available:
                cfg.hw_accel = hw_available[0]["hw"]
            changed = True

        elif cpu_pct < 25.0 and not is_software and not hw_available:
            # Light load, stuck on software (no HW found) → try upgrading quality
            cfg.quality = max(18, current.quality - 2) if current.quality > 0 else current.quality
            changed = True

        return cfg if changed else None


def _scale_bitrate(bitrate_str: str, factor: float) -> str:
    """Scale a bitrate string like '8M' or '2000k' by a factor."""
    s = bitrate_str.strip()
    if s.endswith("M"):
        val = float(s[:-1]) * factor
        return f"{val:.1f}M" if val >= 1 else f"{int(val * 1000)}k"
    elif s.endswith("k"):
        return f"{int(float(s[:-1]) * factor)}k"
    return bitrate_str  # can't parse, leave unchanged


@dataclass
class EncoderProbeResult:
    """Result of a runtime encoder availability probe."""
    encoder: str           # ffmpeg encoder name, e.g. "h264_nvenc"
    codec_family: str      # "h264", "h265", "av1", "vp9", "mjpeg"
    hw_type: str           # "nvenc", "vaapi", "qsv", "v4l2m2m", "software"
    available: bool
    probe_ms: float = 0.0
    error: str = ""

    def to_dict(self) -> dict[str, Any]:
        return {
            "encoder": self.encoder, "codec_family": self.codec_family,
            "hw_type": self.hw_type, "available": self.available,
            "probe_ms": round(self.probe_ms, 1), "error": self.error,
        }


async def _probe_encoder_async(
    codec: str, hw_type: str, family: str, hw_device: str = ""
) -> EncoderProbeResult:
    """Test-encode a tiny frame with the given encoder asynchronously."""
    t0 = time.monotonic()
    if "vaapi" in codec and hw_device:
        cmd = [
            "ffmpeg", "-hide_banner", "-loglevel", "error",
            "-init_hw_device", f"vaapi=hw:{hw_device}",
            "-filter_hw_device", "hw",
            "-f", "lavfi", "-i", "color=black:size=64x64:rate=1",
            "-vf", "format=nv12,hwupload",
            "-frames:v", "1", "-c:v", codec, "-f", "null", "-",
        ]
    else:
        cmd = [
            "ffmpeg", "-hide_banner", "-loglevel", "error",
            "-f", "lavfi", "-i", "color=black:size=64x64:rate=1",
            "-frames:v", "1", "-c:v", codec, "-f", "null", "-",
        ]
    try:
        proc = await asyncio.create_subprocess_exec(
            *cmd,
            stdout=asyncio.subprocess.DEVNULL,
            stderr=asyncio.subprocess.DEVNULL,
        )
        try:
            await asyncio.wait_for(proc.wait(), timeout=12.0)
        except asyncio.TimeoutError:
            proc.kill()
            await proc.wait()
            return EncoderProbeResult(codec, family, hw_type, False,
                                      (time.monotonic() - t0) * 1000, "timeout")
        ok = proc.returncode == 0
        return EncoderProbeResult(codec, family, hw_type, ok,
                                  (time.monotonic() - t0) * 1000,
                                  "" if ok else f"returncode={proc.returncode}")
    except Exception as e:
        return EncoderProbeResult(codec, family, hw_type, False,
                                  (time.monotonic() - t0) * 1000, str(e))
