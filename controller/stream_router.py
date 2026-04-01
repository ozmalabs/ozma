# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Universal stream router — any protocol input to any protocol output.

The stream router is the universal translation layer between input
protocols and output protocols.  Any video/audio/remote-desktop source
can be delivered to any consumer in any format.

Input protocols (sources):
  hdmi_capture   — V4L2 capture card (raw frames or MJPEG)
  vnc            — VNC server (asyncvnc, raw RGBA frames)
  rdp            — RDP via xfreerdp (frame buffer or X11 capture)
  rustdesk       — RustDesk relay (TCP API)
  rtsp           — RTSP camera or NVR stream
  ndi            — NDI source (via NDI SDK / ffmpeg-ndi)
  hls            — HLS manifest (pull segments)
  rtp            — RTP/UDP stream (H.264/H.265 payload)
  webcam         — V4L2 webcam (distinct from capture card)
  screen_capture — Wayland/X11 screen grab (pipewire screen capture)
  obs            — OBS virtual camera or obs-websocket screenshot
  guacamole      — Apache Guacamole connection (via REST API)
  spice          — SPICE protocol (QEMU/KVM)
  wayland        — Wayland pipewire screen capture

Output protocols (delivery):
  hls            — HLS .m3u8 + .ts segments (web playback)
  mjpeg          — Motion JPEG stream (low-latency previews)
  rtsp_server    — Re-publish as RTSP (for other systems)
  rtp            — RTP/UDP unicast or multicast
  ndi            — NDI output (broadcast standard)
  webrtc         — WebRTC (ultra-low-latency browser streaming)
  vnc_server     — VNC server (for VNC clients)
  mp4_file       — Record to MP4/MKV file
  obs_source     — Feed into OBS as a source
  framebuffer    — Raw frames to compositor/overlay engine
  guacamole      — Proxy through Guacamole for browser access

Container formats:
  mpegts         — MPEG-TS (HLS segments, RTP payload)
  mp4            — MP4 (recording, fragmented for streaming)
  mkv            — MKV (recording, flexible codec support)
  webm           — WebM (VP9/AV1 + Opus, web-native)
  flv            — FLV (legacy RTMP streaming)
  raw            — Raw frames (compositor, preview)
  null           — Passthrough (no container, e.g., NDI)

The router can chain: VNC → H.264 encode → HLS output
                       RTSP → decode → re-encode AV1 → WebRTC
                       RDP → frame grab → MJPEG preview
                       Webcam → H.265 → RTP multicast + HLS + record

Codec selection uses CodecManager for encode operations.
Container selection is per-output.
"""

from __future__ import annotations

import asyncio
import json
import logging
import time
from dataclasses import dataclass, field
from enum import Enum
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.stream_router")


class InputProtocol(str, Enum):
    HDMI_CAPTURE = "hdmi_capture"
    VNC = "vnc"
    RDP = "rdp"
    RUSTDESK = "rustdesk"
    RTSP = "rtsp"
    NDI = "ndi"
    HLS = "hls"
    RTP = "rtp"
    WEBCAM = "webcam"
    SCREEN_CAPTURE = "screen_capture"
    OBS = "obs"
    GUACAMOLE = "guacamole"
    SPICE = "spice"
    WAYLAND = "wayland"


class OutputProtocol(str, Enum):
    HLS = "hls"
    MJPEG = "mjpeg"
    RTSP_SERVER = "rtsp_server"
    RTP = "rtp"
    NDI = "ndi"
    WEBRTC = "webrtc"
    VNC_SERVER = "vnc_server"
    MP4_FILE = "mp4_file"
    OBS_SOURCE = "obs_source"
    FRAMEBUFFER = "framebuffer"
    GUACAMOLE = "guacamole"


class ContainerFormat(str, Enum):
    MPEGTS = "mpegts"
    MP4 = "mp4"
    MKV = "mkv"
    WEBM = "webm"
    FLV = "flv"
    RAW = "raw"
    NULL = "null"


# ── Stream route definition ────────────────────────────────────────────────

@dataclass
class StreamInput:
    """An input source for the stream router."""
    id: str
    protocol: str               # InputProtocol value
    name: str = ""
    host: str = ""              # For network protocols
    port: int = 0
    path: str = ""              # Device path, URL, or connection string
    username: str = ""          # For authenticated protocols
    password: str = ""
    options: dict = field(default_factory=dict)  # Protocol-specific options

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id, "protocol": self.protocol, "name": self.name or self.id,
            "host": self.host, "port": self.port, "path": self.path,
        }

    def ffmpeg_input_args(self) -> list[str]:
        """Generate ffmpeg input arguments for this source."""
        match self.protocol:
            case "hdmi_capture" | "webcam":
                fmt = self.options.get("input_format", "v4l2")
                args = ["-f", fmt]
                if "video_size" in self.options:
                    args.extend(["-video_size", self.options["video_size"]])
                if "framerate" in self.options:
                    args.extend(["-framerate", str(self.options["framerate"])])
                args.extend(["-i", self.path])
                return args
            case "vnc":
                # VNC handled via asyncvnc + pipe, not ffmpeg input
                return ["-f", "rawvideo", "-pixel_format", "rgba",
                        "-video_size", self.options.get("video_size", "1920x1080"),
                        "-framerate", str(self.options.get("framerate", 20)),
                        "-i", "pipe:0"]
            case "rdp":
                # RDP → xfreerdp renders to X11 → screen capture
                return ["-f", "x11grab", "-framerate", "30",
                        "-video_size", self.options.get("video_size", "1920x1080"),
                        "-i", self.options.get("display", ":99")]
            case "rtsp":
                args = ["-rtsp_transport", self.options.get("transport", "tcp")]
                url = self.path or f"rtsp://{self.host}:{self.port or 554}{self.options.get('stream_path', '/stream')}"
                args.extend(["-i", url])
                return args
            case "ndi":
                return ["-f", "libndi_newtek",
                        "-find_sources", "1",
                        "-i", self.path or self.name]
            case "hls":
                return ["-i", self.path]
            case "rtp":
                sdp = self.options.get("sdp_file", "")
                if sdp:
                    return ["-protocol_whitelist", "file,rtp,udp", "-i", sdp]
                return ["-i", f"rtp://{self.host}:{self.port}"]
            case "spice":
                # SPICE → spicy or remote-viewer → screen capture
                return ["-f", "x11grab", "-framerate", "30",
                        "-video_size", self.options.get("video_size", "1920x1080"),
                        "-i", self.options.get("display", ":98")]
            case "screen_capture" | "wayland":
                return ["-f", "pipewire", "-i", "default"]
            case _:
                return ["-i", self.path] if self.path else []


@dataclass
class StreamOutput:
    """An output destination for the stream router."""
    id: str
    protocol: str               # OutputProtocol value
    container: str = "mpegts"   # ContainerFormat value
    codec_config_id: str = ""   # CodecManager config reference
    path: str = ""              # Output path (file, URL, device)
    options: dict = field(default_factory=dict)

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id, "protocol": self.protocol,
            "container": self.container, "path": self.path,
        }

    def ffmpeg_output_args(self, codec_args: list[str] | None = None) -> list[str]:
        """Generate ffmpeg output arguments."""
        args = list(codec_args or [])
        match self.protocol:
            case "hls":
                out_dir = Path(self.path or f"/tmp/ozma-stream/{self.id}")
                out_dir.mkdir(parents=True, exist_ok=True)
                manifest = out_dir / "stream.m3u8"
                seg = str(out_dir / "seg_%05d.ts")
                args.extend([
                    "-f", "hls",
                    "-hls_time", str(self.options.get("hls_time", 1)),
                    "-hls_list_size", str(self.options.get("hls_list_size", 6)),
                    "-hls_flags", "delete_segments+independent_segments",
                    "-hls_segment_filename", seg,
                    str(manifest),
                ])
            case "mjpeg":
                args = ["-c:v", "mjpeg", "-q:v", str(self.options.get("quality", 5)),
                        "-f", "mjpeg", self.path or "pipe:1"]
            case "rtsp_server":
                args.extend(["-f", "rtsp", self.path or f"rtsp://0.0.0.0:{self.options.get('port', 8554)}/{self.id}"])
            case "rtp":
                args.extend(["-f", "rtp", f"rtp://{self.options.get('host', '0.0.0.0')}:{self.options.get('port', 5004)}"])
            case "ndi":
                ndi_name = self.options.get("ndi_name", self.id)
                args.extend(["-f", "libndi_newtek", "-ndi_name", ndi_name])
            case "mp4_file":
                ext = "mkv" if self.container == "mkv" else "webm" if self.container == "webm" else "mp4"
                args.extend(["-f", self.container, "-y", self.path or f"/tmp/ozma-recording-{self.id}.{ext}"])
            case "framebuffer":
                args.extend(["-f", "rawvideo", "-pix_fmt", "rgb24", "pipe:1"])
            case _:
                if self.path:
                    args.extend(["-f", self.container, self.path])
        return args


@dataclass
class StreamRoute:
    """A complete route: one input → one or more outputs."""
    id: str
    name: str
    input: StreamInput
    outputs: list[StreamOutput]
    active: bool = False
    proc: Any = None            # ffmpeg subprocess
    started_at: float = 0.0

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id, "name": self.name,
            "input": self.input.to_dict(),
            "outputs": [o.to_dict() for o in self.outputs],
            "active": self.active,
            "uptime_s": round(time.time() - self.started_at, 1) if self.started_at else 0,
        }


# ── Stream router manager ──────────────────────────────────────────────────

class StreamRouter:
    """
    Universal stream routing engine.

    Routes any input protocol to any combination of output protocols,
    with codec transcoding via CodecManager and container selection.
    """

    def __init__(self, codec_manager: Any = None) -> None:
        self._routes: dict[str, StreamRoute] = {}
        self._codec_manager = codec_manager
        self._config_path = Path(__file__).parent / "stream_routes.json"
        self._load_config()

    def _load_config(self) -> None:
        if self._config_path.exists():
            try:
                data = json.loads(self._config_path.read_text())
                for r in data.get("routes", []):
                    inp = StreamInput(**{k: v for k, v in r.get("input", {}).items()
                                        if k in StreamInput.__dataclass_fields__})
                    outs = [StreamOutput(**{k: v for k, v in o.items()
                                           if k in StreamOutput.__dataclass_fields__})
                            for o in r.get("outputs", [])]
                    route = StreamRoute(
                        id=r["id"], name=r.get("name", r["id"]),
                        input=inp, outputs=outs,
                    )
                    self._routes[route.id] = route
            except Exception as e:
                log.warning("Failed to load stream routes: %s", e)

    def _save_config(self) -> None:
        data = {"routes": []}
        for r in self._routes.values():
            data["routes"].append({
                "id": r.id, "name": r.name,
                "input": r.input.to_dict(),
                "outputs": [o.to_dict() for o in r.outputs],
            })
        self._config_path.write_text(json.dumps(data, indent=2))

    async def start(self) -> None:
        """Start all previously-active routes."""
        for route in self._routes.values():
            if route.active:
                route.active = False  # Reset so start_route actually starts
                await self.start_route(route.id)
        log.info("StreamRouter started: %d routes configured", len(self._routes))

    async def stop(self) -> None:
        for route in self._routes.values():
            await self._stop_route(route)
        self._save_config()

    # ── Route CRUD ──────────────────────────────────────────────────────────

    def create_route(self, data: dict) -> StreamRoute | None:
        route_id = data.get("id", "")
        if not route_id or route_id in self._routes:
            return None

        inp_data = data.get("input", {})
        inp = StreamInput(
            id=inp_data.get("id", f"{route_id}-in"),
            protocol=inp_data.get("protocol", "rtsp"),
            name=inp_data.get("name", ""),
            host=inp_data.get("host", ""),
            port=inp_data.get("port", 0),
            path=inp_data.get("path", ""),
            username=inp_data.get("username", ""),
            password=inp_data.get("password", ""),
            options=inp_data.get("options", {}),
        )

        outs = []
        for o in data.get("outputs", []):
            outs.append(StreamOutput(
                id=o.get("id", f"{route_id}-out-{len(outs)}"),
                protocol=o.get("protocol", "hls"),
                container=o.get("container", "mpegts"),
                codec_config_id=o.get("codec_config_id", ""),
                path=o.get("path", ""),
                options=o.get("options", {}),
            ))

        route = StreamRoute(id=route_id, name=data.get("name", route_id),
                            input=inp, outputs=outs)
        self._routes[route_id] = route
        self._save_config()
        return route

    def remove_route(self, route_id: str) -> bool:
        if route_id not in self._routes:
            return False
        route = self._routes[route_id]
        if route.active:
            return False  # Must stop first
        del self._routes[route_id]
        self._save_config()
        return True

    def list_routes(self) -> list[dict]:
        return [r.to_dict() for r in self._routes.values()]

    def get_route(self, route_id: str) -> StreamRoute | None:
        return self._routes.get(route_id)

    # ── Route lifecycle ─────────────────────────────────────────────────────

    async def start_route(self, route_id: str) -> bool:
        route = self._routes.get(route_id)
        if not route or route.active:
            return False

        cmd = ["ffmpeg", "-hide_banner", "-loglevel", "warning", "-y"]

        # Input
        cmd.extend(route.input.ffmpeg_input_args())

        # For multiple outputs, use tee muxer or multiple outputs
        if len(route.outputs) == 1:
            out = route.outputs[0]
            codec_args = self._get_codec_args(out.codec_config_id)
            cmd.extend(out.ffmpeg_output_args(codec_args))
        else:
            # Multiple outputs — use ffmpeg's native multi-output
            # First output gets the codec args, copies for rest
            codec_args = self._get_codec_args(route.outputs[0].codec_config_id)
            for i, out in enumerate(route.outputs):
                if i == 0:
                    cmd.extend(out.ffmpeg_output_args(codec_args))
                else:
                    # Map same input to additional output
                    cmd.extend(out.ffmpeg_output_args(codec_args))

        try:
            proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdin=asyncio.subprocess.PIPE if route.input.protocol == "vnc" else None,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.PIPE,
            )
            route.proc = proc
            route.active = True
            route.started_at = time.time()
            log.info("Route started: %s (%s → %s)",
                     route.id, route.input.protocol,
                     "+".join(o.protocol for o in route.outputs))
            asyncio.create_task(self._monitor(route), name=f"route-{route.id}")
            return True
        except Exception as e:
            log.error("Failed to start route %s: %s", route.id, e)
            return False

    async def stop_route(self, route_id: str) -> bool:
        route = self._routes.get(route_id)
        if not route:
            return False
        await self._stop_route(route)
        return True

    async def _stop_route(self, route: StreamRoute) -> None:
        if route.proc and route.proc.returncode is None:
            route.proc.terminate()
            try:
                await asyncio.wait_for(route.proc.wait(), timeout=5.0)
            except asyncio.TimeoutError:
                route.proc.kill()
        route.proc = None
        route.active = False
        route.started_at = 0

    async def _monitor(self, route: StreamRoute) -> None:
        if not route.proc or not route.proc.stderr:
            return
        try:
            async for line in route.proc.stderr:
                text = line.decode(errors="replace").rstrip()
                if text:
                    log.debug("Route %s: %s", route.id, text)
        except Exception:
            pass
        route.active = False

    def _get_codec_args(self, config_id: str) -> list[str]:
        if not self._codec_manager or not config_id:
            return ["-c:v", "libx264", "-preset", "ultrafast",
                    "-tune", "zerolatency", "-crf", "23"]
        cfg = self._codec_manager.get_config(config_id)
        return self._codec_manager.get_ffmpeg_args(cfg)

    # ── Convenience builders ────────────────────────────────────────────────

    def quick_route(self, name: str, protocol_in: str, path_in: str,
                    protocol_out: str = "hls", **kwargs) -> StreamRoute | None:
        """Create a simple single-input, single-output route."""
        route_id = f"quick-{name}-{int(time.time())}"
        return self.create_route({
            "id": route_id, "name": name,
            "input": {"protocol": protocol_in, "path": path_in, **kwargs},
            "outputs": [{"protocol": protocol_out}],
        })
