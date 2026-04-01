# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Hardware encoder detection.

Probes the local machine for available video encoders in priority order:
  H.265: hevc_nvenc > hevc_vaapi > hevc_v4l2m2m > libx265
  H.264: h264_nvenc > h264_vaapi > h264_v4l2m2m > libx264

Detection is done by running a short null test encode with ffmpeg; if the
encoder initialises cleanly the codec is considered available.

Also detects V4L2 capture devices and their supported formats/resolutions.
"""

from __future__ import annotations

import logging
import os
import subprocess
import tempfile
from dataclasses import dataclass, field
from pathlib import Path
from typing import Sequence

log = logging.getLogger("ozma.node.hw_detect")


@dataclass
class EncoderConfig:
    name: str                    # human-readable label
    codec: str                   # "h265" or "h264"
    ffmpeg_encoder: str          # ffmpeg -c:v value
    input_flags: list[str]       # flags before -i (hwaccel setup)
    encode_flags: list[str]      # flags after -c:v (encoder params)
    is_hardware: bool
    vaapi_device: str | None = None  # e.g. /dev/dri/renderD128


@dataclass
class CaptureDevice:
    path: str                    # e.g. /dev/video0
    name: str
    formats: list[str]           # e.g. ['MJPG', 'YUYV']
    max_width: int
    max_height: int
    has_audio: bool = False
    audio_device: str | None = None  # ALSA device, e.g. "hw:3,0"


def _run_test_encode(input_flags: list[str], encoder: str, extra_vf: str = "") -> bool:
    """Return True if ffmpeg can initialise the given encoder."""
    vf = "format=yuv420p"
    if extra_vf:
        vf = extra_vf + "," + vf
    cmd = [
        "ffmpeg", "-hide_banner", "-loglevel", "error",
        *input_flags,
        "-f", "lavfi", "-i", "color=black:size=64x64:rate=1",
        "-vf", vf,
        "-c:v", encoder,
        "-frames:v", "1",
        "-f", "null", "-",
    ]
    try:
        result = subprocess.run(cmd, capture_output=True, timeout=10)
        return result.returncode == 0
    except (subprocess.TimeoutExpired, FileNotFoundError):
        return False


def _find_vaapi_device() -> str | None:
    """Return the first available VAAPI render node."""
    for path in sorted(Path("/dev/dri").glob("renderD*")):
        if os.access(str(path), os.R_OK | os.W_OK):
            return str(path)
    return None


def _probe_vaapi(device: str, codec: str) -> bool:
    encoder = f"{codec}_vaapi"
    vf = f"format=nv12,hwupload"
    return _run_test_encode(
        ["-vaapi_device", device, "-hwaccel", "vaapi", "-hwaccel_output_format", "vaapi"],
        encoder,
        extra_vf="",
    ) or _run_test_encode(
        ["-vaapi_device", device],
        encoder,
        extra_vf=f"format=nv12,hwupload",
    )


def _probe_nvenc(codec: str) -> bool:
    return _run_test_encode(
        ["-hwaccel", "cuda", "-hwaccel_output_format", "cuda"],
        f"{codec}_nvenc",
    ) or _run_test_encode([], f"{codec}_nvenc")


def _probe_v4l2m2m(codec: str) -> bool:
    return _run_test_encode([], f"{codec}_v4l2m2m")


def _probe_software(codec: str) -> bool:
    encoder = "libx265" if codec == "hevc" else "libx264"
    return _run_test_encode([], encoder)


def detect_encoder(prefer_hevc: bool = True) -> EncoderConfig:
    """
    Probe and return the best available encoder.
    Tries H.265 first if prefer_hevc=True, falls back to H.264.
    Within each codec level, priority is: NVENC > VAAPI > V4L2M2M > software.
    """
    codec_order = ["hevc", "h264"] if prefer_hevc else ["h264", "hevc"]
    vaapi_dev = _find_vaapi_device()

    for codec in codec_order:
        sw_encoder = "libx265" if codec == "hevc" else "libx264"
        codec_label = "H.265" if codec == "hevc" else "H.264"

        # NVENC
        if _probe_nvenc(codec):
            enc = f"{codec}_nvenc"
            log.info("Selected encoder: %s (NVENC hardware)", enc)
            return EncoderConfig(
                name=f"{codec_label}/NVENC",
                codec=codec,
                ffmpeg_encoder=enc,
                input_flags=["-hwaccel", "cuda"],
                encode_flags=[
                    "-preset", "p4",
                    "-tune", "ll",
                    "-rc", "cbr",
                    "-b:v", "4M",
                    "-maxrate", "6M",
                    "-bufsize", "8M",
                ],
                is_hardware=True,
            )

        # VAAPI
        if vaapi_dev and _probe_vaapi(vaapi_dev, codec):
            enc = f"{codec}_vaapi"
            log.info("Selected encoder: %s via %s (VAAPI hardware)", enc, vaapi_dev)
            return EncoderConfig(
                name=f"{codec_label}/VAAPI",
                codec=codec,
                ffmpeg_encoder=enc,
                input_flags=["-vaapi_device", vaapi_dev],
                encode_flags=[
                    "-vf", "format=nv12,hwupload",
                    "-qp", "24",
                    "-b:v", "0",
                ],
                is_hardware=True,
                vaapi_device=vaapi_dev,
            )

        # V4L2 M2M (common on RPi and SoC boards)
        if _probe_v4l2m2m(codec):
            enc = f"{codec}_v4l2m2m"
            log.info("Selected encoder: %s (V4L2 M2M hardware)", enc)
            return EncoderConfig(
                name=f"{codec_label}/V4L2M2M",
                codec=codec,
                ffmpeg_encoder=enc,
                input_flags=[],
                encode_flags=["-b:v", "4M"],
                is_hardware=True,
            )

    # Software fallback — try in codec_order
    for codec in codec_order:
        if _probe_software(codec):
            sw = "libx265" if codec == "hevc" else "libx264"
            codec_label = "H.265" if codec == "hevc" else "H.264"
            log.info("Selected encoder: %s (software fallback)", sw)
            return EncoderConfig(
                name=f"{codec_label}/software",
                codec=codec,
                ffmpeg_encoder=sw,
                input_flags=[],
                encode_flags=["-preset", "veryfast", "-crf", "23"],
                is_hardware=False,
            )

    raise RuntimeError("No usable video encoder found (not even libx264/libx265)")


def detect_capture_devices() -> list[CaptureDevice]:
    """
    Return all V4L2 capture devices with their supported formats and max resolution.
    Skips M2M (encoder/decoder) devices.
    """
    devices: list[CaptureDevice] = []

    for dev_path in sorted(Path("/dev").glob("video*")):
        path = str(dev_path)
        try:
            result = subprocess.run(
                ["v4l2-ctl", "-d", path, "--info", "--list-formats-ext"],
                capture_output=True, text=True, timeout=5,
            )
            out = result.stdout
            if "V4L2_CAP_VIDEO_CAPTURE" not in out and "Video Capture" not in out:
                continue
            # Skip M2M (encoder) devices
            if "V4L2_CAP_VIDEO_M2M" in out or "mem2mem" in out.lower():
                continue

            name_line = next((l for l in out.splitlines() if "Card type" in l), "")
            name = name_line.split(":", 1)[-1].strip() if name_line else path

            formats: list[str] = []
            max_w = max_h = 0
            for line in out.splitlines():
                line = line.strip()
                if line.startswith("[") and "'" in line:
                    fmt = line.split("'")[1]
                    if fmt not in formats:
                        formats.append(fmt)
                elif line.startswith("Size: Discrete"):
                    try:
                        dims = line.split()[-1]
                        w, h = (int(x) for x in dims.split("x"))
                        if w * h > max_w * max_h:
                            max_w, max_h = w, h
                    except (ValueError, IndexError):
                        pass

            if not formats:
                continue

            devices.append(CaptureDevice(
                path=path,
                name=name,
                formats=formats,
                max_width=max_w or 1920,
                max_height=max_h or 1080,
            ))
        except (subprocess.TimeoutExpired, FileNotFoundError, OSError):
            continue

    return devices


def best_capture_device(devices: list[CaptureDevice]) -> CaptureDevice | None:
    """
    Pick the best capture device.
    Prefers devices supporting MJPG (lower USB bandwidth) at highest resolution.
    """
    if not devices:
        return None
    # Prefer MJPG > YUYV > others; tie-break on resolution
    def score(d: CaptureDevice) -> tuple:
        fmt_score = 2 if "MJPG" in d.formats else (1 if "YUYV" in d.formats else 0)
        return (fmt_score, d.max_width * d.max_height)
    return max(devices, key=score)
