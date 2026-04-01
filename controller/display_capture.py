# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Display capture for Controller-as-KVM mode.

Captures HDMI video from V4L2 capture cards plugged into the controller,
encodes to HLS + MJPEG, and makes them available as display sources for
the compositor and web UI.

This is Track A of V1.0 — local capture, no network encode/decode chain.
Each capture card appears as a named display source that scenarios can
bind to.

Architecture:

  HDMI capture card (/dev/videoN)
       │
       └──→ ffmpeg (V4L2 input → HW encode → HLS + MJPEG)
              │
              ├──→ HLS manifest + segments → static/captures/{source_id}/
              │     → HLS.js in web UI for playback
              │
              └──→ MJPEG frames → async generator
                    → low-latency fallback for the web UI
                    → direct texture for compositor (future)

Capture sources are registered with the scenario engine.  A scenario's
``capture_source`` field binds it to a specific capture card:

  {"id": "work", "capture_source": "hdmi-0", ...}

On scenario switch, the web UI switches which HLS stream is displayed
fullscreen.

Hardware detection reuses patterns from node/hw_detect.py but runs on
the controller.  Supports USB capture cards, PCIe cards (Blackmagic,
Magewell), and V4L2 loopback devices.
"""

from __future__ import annotations

import asyncio
import logging
import re
import shutil
import subprocess
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, AsyncIterator

log = logging.getLogger("ozma.display_capture")

CAPTURE_DIR = Path(__file__).parent / "static" / "captures"
DEFAULT_FPS = 60
DEFAULT_MAX_WIDTH = 1920      # Scale down if wider (saves encode bandwidth)
HLS_TIME = 1.0               # Segment duration in seconds
HLS_LIST_SIZE = 4             # Segments in playlist
MJPEG_QUALITY = 80


@dataclass
class Resolution:
    """A supported resolution with aspect ratio detection."""

    width: int
    height: int
    fps: int = 60

    @property
    def aspect_ratio(self) -> str:
        """Human-readable aspect ratio string."""
        from math import gcd
        g = gcd(self.width, self.height)
        w, h = self.width // g, self.height // g
        # Normalise common ratios
        known = {(16, 9): "16:9", (16, 10): "16:10", (4, 3): "4:3", (5, 4): "5:4",
                 (21, 9): "21:9", (64, 27): "21:9", (43, 18): "21:9",
                 (32, 9): "32:9", (32, 10): "32:10", (3, 2): "3:2", (1, 1): "1:1"}
        return known.get((w, h), f"{w}:{h}")

    @property
    def aspect_float(self) -> float:
        return self.width / max(self.height, 1)

    def to_dict(self) -> dict[str, Any]:
        return {"width": self.width, "height": self.height, "fps": self.fps,
                "aspect_ratio": self.aspect_ratio}


# Common resolutions for EDID generation
COMMON_RESOLUTIONS: dict[str, Resolution] = {
    # 16:9
    "1080p":    Resolution(1920, 1080, 60),
    "1440p":    Resolution(2560, 1440, 60),
    "4k":       Resolution(3840, 2160, 60),
    "4k30":     Resolution(3840, 2160, 30),
    "720p":     Resolution(1280, 720, 60),
    # 16:10
    "1920x1200": Resolution(1920, 1200, 60),
    "2560x1600": Resolution(2560, 1600, 60),
    # 21:9 ultrawide
    "2560x1080": Resolution(2560, 1080, 60),
    "3440x1440": Resolution(3440, 1440, 60),
    "3840x1600": Resolution(3840, 1600, 60),
    # 32:9 super ultrawide
    "3840x1080": Resolution(3840, 1080, 60),
    "5120x1440": Resolution(5120, 1440, 60),
    # 4:3
    "1024x768":  Resolution(1024, 768, 60),
    "1600x1200": Resolution(1600, 1200, 60),
    # 5:4
    "1280x1024": Resolution(1280, 1024, 60),
}


@dataclass
class CaptureCard:
    """A detected V4L2 capture card."""

    path: str                     # /dev/video0
    name: str                     # Card name from V4L2 info
    formats: list[str] = field(default_factory=list)  # MJPG, YUYV, NV12, etc.
    resolutions: list[Resolution] = field(default_factory=list)  # All supported
    max_width: int = 1920
    max_height: int = 1080
    max_fps: int = 60
    is_hdmi: bool = False         # Likely an HDMI capture (vs webcam)
    current_resolution: Resolution | None = None  # Active input signal
    edid_path: str = ""           # sysfs path for EDID override (if available)

    @property
    def aspect_ratio(self) -> str:
        r = self.current_resolution or Resolution(self.max_width, self.max_height)
        return r.aspect_ratio

    def to_dict(self) -> dict[str, Any]:
        return {
            "path": self.path,
            "name": self.name,
            "formats": self.formats,
            "max_width": self.max_width,
            "max_height": self.max_height,
            "max_fps": self.max_fps,
            "is_hdmi": self.is_hdmi,
            "aspect_ratio": self.aspect_ratio,
            "current_resolution": self.current_resolution.to_dict() if self.current_resolution else None,
            "resolutions": [r.to_dict() for r in self.resolutions],
            "edid_available": bool(self.edid_path),
        }


@dataclass
class DisplaySource:
    """A running capture source."""

    id: str                       # e.g., "hdmi-0"
    card: CaptureCard
    stream_path: str = ""         # URL path to HLS manifest
    active: bool = False
    proc: asyncio.subprocess.Process | None = field(default=None, repr=False)
    _mjpeg_event: asyncio.Event = field(default_factory=asyncio.Event, repr=False)
    _mjpeg_frame: bytes = field(default=b"", repr=False)

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id,
            "card": self.card.to_dict(),
            "stream_path": self.stream_path,
            "active": self.active,
            "aspect_ratio": self.card.aspect_ratio,
        }


# ── Hardware detection ───────────────────────────────────────────────────────

# Keywords in card names that indicate HDMI capture (vs webcam)
_HDMI_KEYWORDS = [
    "hdmi", "capture", "cam link", "magewell", "decklink", "blackmagic",
    "avermedia", "elgato", "geniatech", "usb3", "video grabber",
    "game capture", "razer ripsaw", "hauppauge", "yuan",
]

# Keywords that indicate NOT a capture card
_IGNORE_KEYWORDS = ["loopback", "bcm2835", "sunxi", "mtk-", "virtual"]


def detect_capture_cards() -> list[CaptureCard]:
    """Detect V4L2 capture cards on the controller."""
    if not shutil.which("v4l2-ctl"):
        return []

    cards: list[CaptureCard] = []
    for path in sorted(Path("/dev").glob("video*")):
        card = _probe_device(str(path))
        if card:
            cards.append(card)

    # Sort: HDMI cards first, then by resolution
    cards.sort(key=lambda c: (not c.is_hdmi, -(c.max_width * c.max_height)))
    return cards


def _probe_device(path: str) -> CaptureCard | None:
    """Probe a V4L2 device for capture capabilities."""
    try:
        result = subprocess.run(
            ["v4l2-ctl", "-d", path, "--info", "--list-formats-ext"],
            capture_output=True, text=True, timeout=5,
        )
        output = result.stdout
    except (FileNotFoundError, subprocess.TimeoutExpired):
        return None

    # Must have capture capability
    if "Video Capture" not in output:
        return None

    # Skip M2M encoder/decoder devices
    if "mem2mem" in output.lower():
        return None

    # Get card name
    name = "Unknown"
    for line in output.splitlines():
        if "Card type" in line:
            name = line.split(":", 1)[1].strip()
            break

    name_lower = name.lower()

    # Skip known non-capture devices — but always keep our own virtual devices
    is_ozma_virtual = name_lower.startswith("ozma")
    if not is_ozma_virtual and any(k in name_lower for k in _IGNORE_KEYWORDS):
        return None

    # Detect formats
    formats = []
    for line in output.splitlines():
        m = re.search(r"\[(\d+)\]:\s+'(\w+)'", line)
        if m:
            fmt = m.group(2)
            if fmt not in formats:
                formats.append(fmt)

    if not formats:
        return None

    # Detect all supported resolutions
    resolutions: list[Resolution] = []
    max_w, max_h, max_fps_val = 0, 0, 30
    current_w, current_h, current_fps = 0, 0, 0

    lines = output.splitlines()
    for i, line in enumerate(lines):
        m = re.search(r"Size:\s+Discrete\s+(\d+)x(\d+)", line)
        if m:
            w, h = int(m.group(1)), int(m.group(2))
            # Look ahead for FPS on next lines
            fps = 30
            for j in range(i + 1, min(i + 5, len(lines))):
                fm = re.search(r"Interval.*\((\d+\.?\d*) fps\)", lines[j])
                if fm:
                    fps = max(fps, int(float(fm.group(1))))
                elif "Size:" in lines[j]:
                    break
            r = Resolution(w, h, min(fps, 120))
            if not any(rr.width == w and rr.height == h for rr in resolutions):
                resolutions.append(r)
            if w * h > max_w * max_h:
                max_w, max_h = w, h
            if fps > max_fps_val:
                max_fps_val = fps

    if max_w == 0:
        max_w, max_h = 1920, 1080
        resolutions = [Resolution(1920, 1080, 60)]

    # Detect current input signal resolution (if card is receiving a signal)
    current_resolution = None
    try:
        dv_result = subprocess.run(
            ["v4l2-ctl", "-d", path, "--query-dv-timings"],
            capture_output=True, text=True, timeout=3,
        )
        for line in dv_result.stdout.splitlines():
            am = re.search(r"Active width:\s*(\d+)", line)
            if am:
                current_w = int(am.group(1))
            am = re.search(r"Active height:\s*(\d+)", line)
            if am:
                current_h = int(am.group(1))
        if current_w > 0 and current_h > 0:
            current_resolution = Resolution(current_w, current_h, max_fps_val)
    except (FileNotFoundError, subprocess.TimeoutExpired):
        pass

    # Detect EDID sysfs path for this device
    edid_path = _find_edid_path(path)

    is_hdmi = any(k in name_lower for k in _HDMI_KEYWORDS)

    return CaptureCard(
        path=path,
        name=name,
        formats=formats,
        resolutions=resolutions,
        max_width=max_w,
        max_height=max_h,
        max_fps=min(max_fps_val, 120),
        is_hdmi=is_hdmi,
        current_resolution=current_resolution,
        edid_path=edid_path,
    )


# ── Encoder selection ────────────────────────────────────────────────────────

@dataclass
class EncoderConfig:
    name: str
    codec: str           # ffmpeg -c:v value
    flags: list[str]     # extra ffmpeg flags
    hw_device: str = ""  # e.g., /dev/dri/renderD128 for VAAPI


def _find_edid_path(v4l2_path: str) -> str:
    """Find the sysfs EDID file for a V4L2 device, if available."""
    # USB capture cards: /sys/class/video4linux/video0/device/edid
    dev_name = Path(v4l2_path).name  # e.g., "video0"
    candidates = [
        Path(f"/sys/class/video4linux/{dev_name}/device/edid"),
        Path(f"/sys/class/video4linux/{dev_name}/edid"),
    ]
    # Also check for v4l2-ctl EDID support
    for p in candidates:
        if p.exists():
            return str(p)
    return ""


def detect_encoder(prefer_h265: bool = False) -> EncoderConfig:
    """Detect the best available hardware encoder."""
    # Priority: NVENC > VAAPI > QSV > software
    candidates = []
    if prefer_h265:
        candidates = [
            EncoderConfig("NVENC H.265", "hevc_nvenc", ["-preset", "p4", "-tune", "ll", "-rc", "cbr", "-b:v", "8M"]),
            EncoderConfig("VAAPI H.265", "hevc_vaapi", ["-qp", "24"], hw_device="/dev/dri/renderD128"),
            EncoderConfig("QSV H.265", "hevc_qsv", ["-preset", "veryfast", "-b:v", "8M"]),
        ]
    candidates += [
        EncoderConfig("NVENC H.264", "h264_nvenc", ["-preset", "p4", "-tune", "ll", "-rc", "cbr", "-b:v", "8M"]),
        EncoderConfig("VAAPI H.264", "h264_vaapi", ["-qp", "24"], hw_device="/dev/dri/renderD128"),
        EncoderConfig("QSV H.264", "h264_qsv", ["-preset", "veryfast", "-b:v", "8M"]),
        EncoderConfig("Software H.264", "libx264", ["-preset", "ultrafast", "-tune", "zerolatency", "-crf", "23"]),
    ]

    for enc in candidates:
        if _test_encoder(enc):
            return enc

    return EncoderConfig("Software H.264", "libx264", ["-preset", "ultrafast", "-tune", "zerolatency", "-crf", "23"])


def _test_encoder(enc: EncoderConfig) -> bool:
    """Test if an encoder works with a null encode."""
    cmd = ["ffmpeg", "-hide_banner", "-loglevel", "error",
           "-f", "lavfi", "-i", "color=black:size=64x64:rate=1",
           "-frames:v", "1", "-c:v", enc.codec]
    if enc.hw_device:
        cmd = ["ffmpeg", "-hide_banner", "-loglevel", "error",
               "-init_hw_device", f"vaapi=hw:{enc.hw_device}",
               "-filter_hw_device", "hw",
               "-f", "lavfi", "-i", "color=black:size=64x64:rate=1",
               "-vf", "format=nv12,hwupload",
               "-frames:v", "1", "-c:v", enc.codec]
    cmd.extend(enc.flags[:2])  # only first 2 flags for test
    cmd.extend(["-f", "null", "-"])
    try:
        result = subprocess.run(cmd, capture_output=True, timeout=10)
        return result.returncode == 0
    except (FileNotFoundError, subprocess.TimeoutExpired):
        return False


# ── Capture pipeline ─────────────────────────────────────────────────────────

class DisplayCaptureManager:
    """
    Manages HDMI capture cards on the controller.

    Detects cards, starts/stops ffmpeg capture pipelines, and serves
    HLS + MJPEG streams.  Each capture card becomes a DisplaySource
    that scenarios can bind to.
    """

    def __init__(self, codec_manager: Any = None) -> None:
        self._sources: dict[str, DisplaySource] = {}
        self._encoder: EncoderConfig | None = None
        self._codec_manager = codec_manager
        self._scan_task: asyncio.Task | None = None

    async def start(self) -> None:
        """Detect cards, select encoder, start captures."""
        CAPTURE_DIR.mkdir(parents=True, exist_ok=True)

        cards = detect_capture_cards()
        if not cards:
            log.info("No capture cards detected — display capture disabled")
            return

        self._encoder = detect_encoder(prefer_h265=True)
        log.info("Display capture encoder: %s", self._encoder.name)

        for i, card in enumerate(cards):
            source_id = f"hdmi-{i}"
            source = DisplaySource(
                id=source_id,
                card=card,
                stream_path=f"/captures/{source_id}/stream.m3u8",
            )
            self._sources[source_id] = source
            log.info("Capture card %s: %s (%s, %dx%d@%dfps, formats: %s)",
                     source_id, card.name, card.path,
                     card.max_width, card.max_height, card.max_fps,
                     ",".join(card.formats))

        # Start all captures
        for source in self._sources.values():
            await self._start_capture(source)

        # Periodic health check
        self._scan_task = asyncio.create_task(self._health_loop(), name="capture-health")

    async def stop(self) -> None:
        if self._scan_task:
            self._scan_task.cancel()
        for source in self._sources.values():
            await self._stop_capture(source)

    def list_sources(self) -> list[dict[str, Any]]:
        return [s.to_dict() for s in self._sources.values()]

    def get_source(self, source_id: str) -> DisplaySource | None:
        return self._sources.get(source_id)

    async def register_virtual_capture(self, source_id: str, device_path: str,
                                        name: str = "", width: int = 1024,
                                        height: int = 768, fps: int = 20) -> bool:
        """Register a virtual capture device (v4l2loopback from a soft node).

        Called when a node announces a capture_device in mDNS. Creates a
        DisplaySource that the capture pipeline treats identically to a
        real hardware capture card.
        """
        if source_id in self._sources:
            return True  # Already registered

        if not self._encoder:
            self._encoder = detect_encoder(prefer_h265=False)

        card = CaptureCard(
            path=device_path,
            name=name or f"Virtual: {source_id}",
            formats=["YUV420"],
            max_width=width,
            max_height=height,
            max_fps=fps,
            is_hdmi=False,
        )
        source = DisplaySource(
            id=source_id,
            card=card,
            stream_path=f"/captures/{source_id}/stream.m3u8",
        )
        self._sources[source_id] = source
        log.info("Virtual capture registered: %s → %s (%dx%d@%d)",
                 source_id, device_path, width, height, fps)

        # Start capture pipeline — the v4l2loopback device is already being
        # fed by the soft node's VirtualCapture ffmpeg
        await self._start_capture(source)
        return True

    def get_source_for_card(self, card_path: str) -> DisplaySource | None:
        """Find source by V4L2 device path."""
        for s in self._sources.values():
            if s.card.path == card_path:
                return s
        return None

    async def mjpeg_frames(self, source_id: str) -> AsyncIterator[bytes] | None:
        """Async generator of MJPEG frames for low-latency display."""
        source = self._sources.get(source_id)
        if not source:
            return None

        async def _gen():
            while True:
                await source._mjpeg_event.wait()
                source._mjpeg_event.clear()
                if source._mjpeg_frame:
                    yield source._mjpeg_frame

        return _gen()

    # ── Internal pipeline ────────────────────────────────────────────────────

    async def _start_capture(self, source: DisplaySource) -> None:
        """Launch ffmpeg capture pipeline for one card."""
        if not self._encoder:
            return

        card = source.card
        enc = self._encoder

        # Output directory for HLS segments
        out_dir = CAPTURE_DIR / source.id
        out_dir.mkdir(parents=True, exist_ok=True)
        manifest = out_dir / "stream.m3u8"
        seg_pattern = str(out_dir / "seg_%05d.ts")

        # Build ffmpeg command
        cmd = ["ffmpeg", "-hide_banner", "-loglevel", "warning", "-y"]

        # Use current signal resolution if detected, otherwise max supported
        cap_res = card.current_resolution
        cap_w = cap_res.width if cap_res else card.max_width
        cap_h = cap_res.height if cap_res else card.max_height
        cap_fps = min(cap_res.fps if cap_res else card.max_fps, DEFAULT_FPS)

        # Input: V4L2
        input_fmt = "mjpeg" if "MJPG" in card.formats else "rawvideo"
        cmd.extend([
            "-f", "v4l2",
            "-input_format", input_fmt if input_fmt != "rawvideo" else "yuyv422",
            "-video_size", f"{cap_w}x{cap_h}",
            "-framerate", str(cap_fps),
            "-i", card.path,
        ])

        # Video filter: scale if wider than limit, preserving aspect ratio
        # -2 ensures height is divisible by 2 (required for most encoders)
        filters = []
        if enc.hw_device:
            # VAAPI path
            cmd.extend(["-init_hw_device", f"vaapi=hw:{enc.hw_device}", "-filter_hw_device", "hw"])
            if cap_w > DEFAULT_MAX_WIDTH:
                filters.append(f"scale={DEFAULT_MAX_WIDTH}:-2")
            filters.append("format=nv12")
            filters.append("hwupload")
        else:
            if cap_w > DEFAULT_MAX_WIDTH:
                filters.append(f"scale={DEFAULT_MAX_WIDTH}:-2")
            filters.append("format=yuv420p")

        if filters:
            cmd.extend(["-vf", ",".join(filters)])

        # Encoder
        cmd.extend(["-c:v", enc.codec])
        cmd.extend(enc.flags)

        # HLS output
        cmd.extend([
            "-f", "hls",
            "-hls_time", str(HLS_TIME),
            "-hls_list_size", str(HLS_LIST_SIZE),
            "-hls_flags", "delete_segments+independent_segments+append_list",
            "-hls_segment_filename", seg_pattern,
            str(manifest),
        ])

        try:
            proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.PIPE,
            )
            source.proc = proc
            source.active = True
            log.info("Capture started: %s (%s → %s, pid %d)",
                     source.id, card.path, enc.name, proc.pid)

            # Monitor stderr in background
            asyncio.create_task(
                self._monitor_stderr(source), name=f"capture-log-{source.id}"
            )
        except Exception as e:
            log.error("Failed to start capture %s: %s", source.id, e)

    async def _stop_capture(self, source: DisplaySource) -> None:
        proc = source.proc
        if proc and proc.returncode is None:
            proc.terminate()
            try:
                await asyncio.wait_for(proc.wait(), timeout=5.0)
            except asyncio.TimeoutError:
                proc.kill()
        source.active = False
        source.proc = None

    async def _monitor_stderr(self, source: DisplaySource) -> None:
        """Log ffmpeg stderr and detect failures."""
        if not source.proc or not source.proc.stderr:
            return
        try:
            while True:
                line = await source.proc.stderr.readline()
                if not line:
                    break
                msg = line.decode(errors="replace").strip()
                if msg and "error" in msg.lower():
                    log.warning("Capture %s: %s", source.id, msg)
        except Exception:
            pass

        if source.active:
            rc = source.proc.returncode if source.proc else -1
            log.warning("Capture %s exited (rc=%s), will restart", source.id, rc)
            source.active = False

    async def _health_loop(self) -> None:
        """Restart failed captures."""
        while True:
            try:
                await asyncio.sleep(5.0)
                for source in self._sources.values():
                    if not source.active and source.proc and source.proc.returncode is not None:
                        log.info("Restarting capture: %s", source.id)
                        await self._start_capture(source)
            except asyncio.CancelledError:
                return
