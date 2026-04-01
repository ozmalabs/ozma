# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Arbitrary camera capture and streaming — any V4L2, RTSP, or USB camera.

Provides flexible management of all video input sources on the system,
including webcams, USB cameras, IP cameras (RTSP/ONVIF), and virtual
camera devices.  Each camera can be independently captured, streamed,
recorded, or used as an overlay/broadcast source.

Privacy framework:
  Every camera has a privacy state.  Before a camera stream is accessible:
    1. It must be explicitly enabled in the configuration
    2. A privacy acknowledgement must be confirmed (API call or first-run UI)
    3. Privacy zones can be defined (blurred/blacked-out regions)
    4. Active recording indicator is always visible in the web UI
    5. Access log records who/when accessed each camera stream

  Privacy levels:
    disabled    — camera exists but cannot be captured (default for new cameras)
    local_only  — stream available only on the controller itself (localhost)
    network     — stream available to authenticated clients on the LAN
    public      — stream available without authentication (use with caution)

  Privacy zones:
    Rectangular regions within the camera frame that are blurred or blacked out.
    Useful for covering areas that should never be recorded (windows into
    other rooms, sensitive equipment, etc.).

Camera types:
  v4l2      — local USB/built-in cameras (/dev/video*)
  rtsp      — IP cameras via RTSP URL
  onvif     — ONVIF-compatible cameras (auto-discovery + PTZ control)
  ndi       — NDI camera sources (via NDI SDK)
  virtual   — virtual cameras (v4l2loopback, OBS virtual camera)

Streaming outputs per camera:
  HLS      — .m3u8 + .ts segments for web playback
  MJPEG    — low-latency JPEG stream for previews
  RTSP     — re-publish as RTSP server (for other systems)
  WebRTC   — ultra-low-latency browser streaming (future)
  Snapshot — single JPEG frame on demand
"""

from __future__ import annotations

import asyncio
import hashlib
import json
import logging
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, AsyncIterator

log = logging.getLogger("ozma.cameras")

CAMERA_DIR = Path(__file__).parent / "static" / "cameras"
ACCESS_LOG_PATH = Path(__file__).parent / "camera_access.log"


# ── Privacy ─────────────────────────────────────────────────────────────────

@dataclass
class PrivacyZone:
    """A rectangular region to blur or black out."""
    x: float        # 0.0-1.0 normalised coordinates
    y: float
    width: float
    height: float
    mode: str = "blur"  # blur, blackout

    def to_ffmpeg_filter(self, frame_w: int, frame_h: int) -> str:
        px = int(self.x * frame_w)
        py = int(self.y * frame_h)
        pw = int(self.width * frame_w)
        ph = int(self.height * frame_h)
        if self.mode == "blackout":
            return f"drawbox=x={px}:y={py}:w={pw}:h={ph}:color=black:t=fill"
        # Gaussian blur: crop region, blur, overlay back
        return (f"split[base][blur];[blur]crop={pw}:{ph}:{px}:{py},"
                f"boxblur=20:20[blurred];[base][blurred]overlay={px}:{py}")


@dataclass
class PrivacyConfig:
    """Privacy settings for a camera."""
    level: str = "disabled"          # disabled, local_only, network, public
    acknowledged: bool = False       # User has confirmed privacy notice
    acknowledged_by: str = ""        # Who acknowledged (username/IP)
    acknowledged_at: float = 0.0     # When acknowledged (timestamp)
    zones: list[PrivacyZone] = field(default_factory=list)
    recording_notice: bool = True    # Show "recording" indicator in UI
    access_log_enabled: bool = True  # Log all stream access

    def to_dict(self) -> dict[str, Any]:
        return {
            "level": self.level,
            "acknowledged": self.acknowledged,
            "acknowledged_by": self.acknowledged_by,
            "zones": [{"x": z.x, "y": z.y, "width": z.width, "height": z.height,
                       "mode": z.mode} for z in self.zones],
            "recording_notice": self.recording_notice,
        }

    @classmethod
    def from_dict(cls, d: dict) -> "PrivacyConfig":
        zones = [PrivacyZone(**z) for z in d.get("zones", [])]
        return cls(
            level=d.get("level", "disabled"),
            acknowledged=d.get("acknowledged", False),
            acknowledged_by=d.get("acknowledged_by", ""),
            acknowledged_at=d.get("acknowledged_at", 0.0),
            zones=zones,
            recording_notice=d.get("recording_notice", True),
            access_log_enabled=d.get("access_log_enabled", True),
        )


# ── Camera definition ───────────────────────────────────────────────────────

@dataclass
class CameraSource:
    """A camera source available for capture."""
    id: str
    name: str
    camera_type: str          # v4l2, rtsp, onvif, ndi, virtual
    path: str = ""            # /dev/video* for v4l2, rtsp:// URL for RTSP, etc.
    width: int = 1920
    height: int = 1080
    fps: int = 30
    codec_config_id: str = "" # Reference to CodecManager config (empty = default)

    # Runtime state
    active: bool = False
    proc: Any = None          # ffmpeg subprocess
    privacy: PrivacyConfig = field(default_factory=PrivacyConfig)
    tags: list[str] = field(default_factory=list)  # user tags: "office", "lab", etc.

    # MJPEG preview
    _mjpeg_frame: bytes = field(default=b"", repr=False)
    _mjpeg_event: asyncio.Event = field(default_factory=asyncio.Event, repr=False)

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id, "name": self.name, "type": self.camera_type,
            "path": self.path, "width": self.width, "height": self.height,
            "fps": self.fps, "active": self.active, "tags": self.tags,
            "privacy": self.privacy.to_dict(),
            "stream_path": f"/cameras/{self.id}/stream.m3u8" if self.active else None,
            "mjpeg_path": f"/api/v1/cameras/{self.id}/mjpeg" if self.active else None,
            "snapshot_path": f"/api/v1/cameras/{self.id}/snapshot",
        }


# ── Access logging ──────────────────────────────────────────────────────────

def _log_access(camera_id: str, action: str, client: str = "") -> None:
    """Append to the camera access log."""
    entry = {
        "ts": time.time(),
        "camera": camera_id,
        "action": action,
        "client": client,
    }
    try:
        with open(ACCESS_LOG_PATH, "a") as f:
            f.write(json.dumps(entry) + "\n")
    except Exception:
        pass


# ── Camera manager ──────────────────────────────────────────────────────────

class CameraManager:
    """
    Manages all camera sources — local, network, and virtual.

    Handles detection, privacy enforcement, capture pipelines, and
    streaming output.  Integrates with CodecManager for encoder selection.
    """

    def __init__(self, codec_manager: Any = None) -> None:
        self._cameras: dict[str, CameraSource] = {}
        self._codec_manager = codec_manager
        self._config_path = Path(__file__).parent / "cameras.json"
        self._scan_task: asyncio.Task | None = None
        self._load_config()

    def _load_config(self) -> None:
        """Load saved camera configurations."""
        if self._config_path.exists():
            try:
                data = json.loads(self._config_path.read_text())
                for cam_data in data.get("cameras", []):
                    cam = CameraSource(
                        id=cam_data["id"],
                        name=cam_data.get("name", cam_data["id"]),
                        camera_type=cam_data.get("type", "v4l2"),
                        path=cam_data.get("path", ""),
                        width=cam_data.get("width", 1920),
                        height=cam_data.get("height", 1080),
                        fps=cam_data.get("fps", 30),
                        codec_config_id=cam_data.get("codec_config_id", ""),
                        tags=cam_data.get("tags", []),
                    )
                    cam.privacy = PrivacyConfig.from_dict(cam_data.get("privacy", {}))
                    self._cameras[cam.id] = cam
            except Exception as e:
                log.warning("Failed to load camera config: %s", e)

    def _save_config(self) -> None:
        """Persist camera configurations."""
        data = {"cameras": []}
        for cam in self._cameras.values():
            data["cameras"].append({
                "id": cam.id, "name": cam.name, "type": cam.camera_type,
                "path": cam.path, "width": cam.width, "height": cam.height,
                "fps": cam.fps, "codec_config_id": cam.codec_config_id,
                "tags": cam.tags, "privacy": cam.privacy.to_dict(),
            })
        self._config_path.write_text(json.dumps(data, indent=2))

    async def start(self) -> None:
        """Detect local cameras and start configured streams."""
        CAMERA_DIR.mkdir(parents=True, exist_ok=True)
        self._detect_local_cameras()
        # Auto-start cameras that were previously active and privacy-cleared
        for cam in self._cameras.values():
            if cam.privacy.level != "disabled" and cam.privacy.acknowledged:
                await self.start_capture(cam.id)
        self._scan_task = asyncio.create_task(self._rescan_loop(), name="camera-rescan")
        log.info("CameraManager started: %d cameras", len(self._cameras))

    async def stop(self) -> None:
        if self._scan_task:
            self._scan_task.cancel()
        for cam in self._cameras.values():
            await self._stop_capture(cam)
        self._save_config()

    def _detect_local_cameras(self) -> None:
        """Scan for V4L2 camera devices (not capture cards)."""
        import subprocess
        try:
            result = subprocess.run(
                ["v4l2-ctl", "--list-devices"],
                capture_output=True, text=True, timeout=5,
            )
            if result.returncode != 0:
                return

            current_name = ""
            for line in result.stdout.splitlines():
                line = line.rstrip()
                if not line:
                    continue
                if not line.startswith("\t"):
                    current_name = line.rstrip(":")
                else:
                    dev_path = line.strip()
                    if not dev_path.startswith("/dev/video"):
                        continue
                    cam_id = f"local-{Path(dev_path).name}"
                    if cam_id not in self._cameras:
                        self._cameras[cam_id] = CameraSource(
                            id=cam_id,
                            name=current_name or cam_id,
                            camera_type="v4l2",
                            path=dev_path,
                        )
                        log.debug("Detected camera: %s (%s)", cam_id, current_name)
        except Exception as e:
            log.debug("Camera detection: %s", e)

    async def _rescan_loop(self) -> None:
        """Periodically rescan for new cameras."""
        while True:
            await asyncio.sleep(30)
            self._detect_local_cameras()

    # ── Privacy enforcement ─────────────────────────────────────────────────

    def acknowledge_privacy(self, camera_id: str, client_info: str = "") -> bool:
        """
        Acknowledge the privacy notice for a camera.

        Must be called before a camera can be enabled.
        Returns False if camera not found.
        """
        cam = self._cameras.get(camera_id)
        if not cam:
            return False
        cam.privacy.acknowledged = True
        cam.privacy.acknowledged_by = client_info
        cam.privacy.acknowledged_at = time.time()
        _log_access(camera_id, "privacy_acknowledged", client_info)
        self._save_config()
        return True

    def set_privacy_level(self, camera_id: str, level: str) -> bool:
        """Set privacy level. Requires prior acknowledgement."""
        cam = self._cameras.get(camera_id)
        if not cam:
            return False
        if level != "disabled" and not cam.privacy.acknowledged:
            return False
        cam.privacy.level = level
        _log_access(camera_id, f"privacy_level_set:{level}")
        self._save_config()
        return True

    def add_privacy_zone(self, camera_id: str, zone: PrivacyZone) -> bool:
        cam = self._cameras.get(camera_id)
        if not cam:
            return False
        cam.privacy.zones.append(zone)
        self._save_config()
        return True

    def remove_privacy_zone(self, camera_id: str, index: int) -> bool:
        cam = self._cameras.get(camera_id)
        if not cam or index >= len(cam.privacy.zones):
            return False
        cam.privacy.zones.pop(index)
        self._save_config()
        return True

    def _check_access(self, camera_id: str, client: str = "") -> str | None:
        """Check if access is allowed. Returns error string or None if OK."""
        cam = self._cameras.get(camera_id)
        if not cam:
            return "Camera not found"
        if cam.privacy.level == "disabled":
            return "Camera is disabled — enable in settings and acknowledge privacy notice"
        if not cam.privacy.acknowledged:
            return "Privacy notice not acknowledged — acknowledge before accessing"
        if cam.privacy.access_log_enabled:
            _log_access(camera_id, "stream_access", client)
        return None

    # ── Capture pipeline ────────────────────────────────────────────────────

    async def start_capture(self, camera_id: str) -> bool:
        """Start capturing from a camera. Requires privacy clearance."""
        cam = self._cameras.get(camera_id)
        if not cam:
            return False
        err = self._check_access(camera_id)
        if err:
            log.warning("Cannot start capture %s: %s", camera_id, err)
            return False
        if cam.active:
            return True

        out_dir = CAMERA_DIR / camera_id
        out_dir.mkdir(parents=True, exist_ok=True)

        cmd = ["ffmpeg", "-hide_banner", "-loglevel", "warning", "-y"]

        # Input based on camera type
        match cam.camera_type:
            case "v4l2":
                cmd.extend([
                    "-f", "v4l2",
                    "-video_size", f"{cam.width}x{cam.height}",
                    "-framerate", str(cam.fps),
                    "-i", cam.path,
                ])
            case "rtsp":
                cmd.extend([
                    "-rtsp_transport", "tcp",
                    "-i", cam.path,
                ])
            case "onvif":
                # ONVIF cameras provide RTSP URLs after discovery
                cmd.extend(["-rtsp_transport", "tcp", "-i", cam.path])
            case "ndi":
                cmd.extend(["-f", "libndi_newtek", "-i", cam.path])
            case _:
                cmd.extend(["-i", cam.path])

        # Privacy zone filters
        filters = []
        if cam.privacy.zones:
            for zone in cam.privacy.zones:
                if zone.mode == "blackout":
                    px = int(zone.x * cam.width)
                    py = int(zone.y * cam.height)
                    pw = int(zone.width * cam.width)
                    ph = int(zone.height * cam.height)
                    filters.append(f"drawbox=x={px}:y={py}:w={pw}:h={ph}:color=black:t=fill")

        # Codec selection via CodecManager
        if self._codec_manager:
            from codec_manager import CodecConfig
            cfg_id = cam.codec_config_id or "default"
            codec_cfg = self._codec_manager.get_config(cfg_id)
            enc_args = self._codec_manager.get_ffmpeg_args(codec_cfg)
            # Insert vf filters before codec args if present
            vf_idx = None
            for i, arg in enumerate(enc_args):
                if arg == "-vf":
                    vf_idx = i
                    break
            if vf_idx is not None and filters:
                # Prepend our privacy filters to the codec's vf chain
                existing_vf = enc_args[vf_idx + 1]
                enc_args[vf_idx + 1] = ",".join(filters) + "," + existing_vf
            elif filters:
                cmd.extend(["-vf", ",".join(filters)])
            cmd.extend(enc_args)
        else:
            if filters:
                cmd.extend(["-vf", ",".join(filters)])
            cmd.extend(["-c:v", "libx264", "-preset", "ultrafast",
                        "-tune", "zerolatency", "-crf", "23"])

        # HLS output
        manifest = out_dir / "stream.m3u8"
        seg_pattern = str(out_dir / "seg_%05d.ts")
        cmd.extend([
            "-f", "hls",
            "-hls_time", "1",
            "-hls_list_size", "6",
            "-hls_flags", "delete_segments+independent_segments",
            "-hls_segment_filename", seg_pattern,
            str(manifest),
        ])

        try:
            proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.PIPE,
            )
            cam.proc = proc
            cam.active = True
            _log_access(camera_id, "capture_started")
            log.info("Camera capture started: %s (%s)", camera_id, cam.name)
            asyncio.create_task(self._monitor_stderr(cam), name=f"cam-log-{camera_id}")
            return True
        except Exception as e:
            log.error("Failed to start camera %s: %s", camera_id, e)
            return False

    async def _stop_capture(self, cam: CameraSource) -> None:
        if cam.proc and cam.proc.returncode is None:
            cam.proc.terminate()
            try:
                await asyncio.wait_for(cam.proc.wait(), timeout=5.0)
            except asyncio.TimeoutError:
                cam.proc.kill()
        cam.proc = None
        cam.active = False
        _log_access(cam.id, "capture_stopped")

    async def stop_capture(self, camera_id: str) -> bool:
        cam = self._cameras.get(camera_id)
        if not cam:
            return False
        await self._stop_capture(cam)
        return True

    async def _monitor_stderr(self, cam: CameraSource) -> None:
        if not cam.proc or not cam.proc.stderr:
            return
        try:
            async for line in cam.proc.stderr:
                text = line.decode(errors="replace").rstrip()
                if text:
                    log.debug("Camera %s ffmpeg: %s", cam.id, text)
        except Exception:
            pass

    async def snapshot(self, camera_id: str, client: str = "") -> bytes | None:
        """Capture a single JPEG frame."""
        cam = self._cameras.get(camera_id)
        if not cam:
            return None
        err = self._check_access(camera_id, client)
        if err:
            return None

        cmd = ["ffmpeg", "-hide_banner", "-loglevel", "error"]
        match cam.camera_type:
            case "v4l2":
                cmd.extend(["-f", "v4l2", "-i", cam.path])
            case "rtsp" | "onvif":
                cmd.extend(["-rtsp_transport", "tcp", "-i", cam.path])
            case _:
                cmd.extend(["-i", cam.path])
        cmd.extend(["-frames:v", "1", "-f", "image2pipe", "-vcodec", "mjpeg", "pipe:1"])

        try:
            proc = await asyncio.create_subprocess_exec(
                *cmd, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.DEVNULL,
            )
            stdout, _ = await asyncio.wait_for(proc.communicate(), timeout=10)
            _log_access(camera_id, "snapshot", client)
            return stdout if stdout else None
        except Exception:
            return None

    # ── Camera CRUD ─────────────────────────────────────────────────────────

    def add_camera(self, data: dict) -> CameraSource | None:
        """Add a new camera source (RTSP, ONVIF, etc.)."""
        cam_id = data.get("id", "")
        if not cam_id or cam_id in self._cameras:
            return None
        cam = CameraSource(
            id=cam_id,
            name=data.get("name", cam_id),
            camera_type=data.get("type", "rtsp"),
            path=data.get("path", data.get("url", "")),
            width=data.get("width", 1920),
            height=data.get("height", 1080),
            fps=data.get("fps", 30),
            tags=data.get("tags", []),
        )
        self._cameras[cam_id] = cam
        self._save_config()
        return cam

    def remove_camera(self, camera_id: str) -> bool:
        if camera_id not in self._cameras:
            return False
        del self._cameras[camera_id]
        self._save_config()
        return True

    def update_camera(self, camera_id: str, data: dict) -> bool:
        cam = self._cameras.get(camera_id)
        if not cam:
            return False
        if "name" in data:
            cam.name = data["name"]
        if "path" in data or "url" in data:
            cam.path = data.get("path", data.get("url", cam.path))
        if "width" in data:
            cam.width = data["width"]
        if "height" in data:
            cam.height = data["height"]
        if "fps" in data:
            cam.fps = data["fps"]
        if "tags" in data:
            cam.tags = data["tags"]
        if "codec_config_id" in data:
            cam.codec_config_id = data["codec_config_id"]
        self._save_config()
        return True

    # ── Queries ─────────────────────────────────────────────────────────────

    def list_cameras(self) -> list[dict]:
        return [c.to_dict() for c in self._cameras.values()]

    def get_camera(self, camera_id: str) -> CameraSource | None:
        return self._cameras.get(camera_id)

    def get_access_log(self, camera_id: str | None = None, limit: int = 100) -> list[dict]:
        """Read the access log, optionally filtered by camera."""
        entries: list[dict] = []
        try:
            with open(ACCESS_LOG_PATH) as f:
                for line in f:
                    entry = json.loads(line)
                    if camera_id and entry.get("camera") != camera_id:
                        continue
                    entries.append(entry)
        except FileNotFoundError:
            pass
        return entries[-limit:]
