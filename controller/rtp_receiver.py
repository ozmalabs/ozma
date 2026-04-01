# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
RTP video receiver — decodes distributed H.265 streams from video nodes.

For each node advertising video_rtp_port in mDNS, the controller runs
an ffmpeg process that:
  1. Receives the RTP stream
  2. Decodes H.265 via hardware (VAAPI/QuickSync) or software
  3. Re-encodes to HLS segments in static/captures/{node_id}/

The HLS output is identical in format to local capture cards, so the
scenario engine and web UI treat them identically.

Latency: ~25ms glass-to-glass (8ms capture + 4ms encode + <1ms network +
4ms decode + 8ms display).
"""

from __future__ import annotations

import asyncio
import logging
import shutil
from dataclasses import dataclass
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.rtp_receiver")

CAPTURE_DIR = Path(__file__).parent / "static" / "captures"


@dataclass
class RTPSource:
    """A remote video node streaming via RTP."""

    node_id: str
    host: str
    rtp_port: int
    width: int = 1920
    height: int = 1080
    fps: int = 60
    active: bool = False
    proc: asyncio.subprocess.Process | None = None

    @property
    def source_id(self) -> str:
        return f"rtp-{self.node_id.split('.')[0]}"

    @property
    def stream_path(self) -> str:
        return f"/captures/{self.source_id}/stream.m3u8"

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.source_id,
            "node_id": self.node_id,
            "host": self.host,
            "rtp_port": self.rtp_port,
            "stream_path": self.stream_path,
            "active": self.active,
            "card": {
                "name": f"RTP from {self.node_id.split('.')[0]}",
                "max_width": self.width,
                "max_height": self.height,
                "max_fps": self.fps,
                "is_hdmi": True,
                "formats": ["H.265/RTP"],
            },
        }


class RTPReceiverManager:
    """
    Manages RTP video receivers for distributed video nodes.

    Scans AppState for nodes advertising video_rtp_port, starts ffmpeg
    receivers for each, and produces HLS output alongside local captures.
    """

    def __init__(self, state: Any) -> None:
        self._state = state
        self._sources: dict[str, RTPSource] = {}
        self._task: asyncio.Task | None = None

    async def start(self) -> None:
        CAPTURE_DIR.mkdir(parents=True, exist_ok=True)
        self._task = asyncio.create_task(self._scan_loop(), name="rtp-receiver-scan")
        log.info("RTP receiver manager started")

    async def stop(self) -> None:
        if self._task:
            self._task.cancel()
        for source in self._sources.values():
            await self._stop_receiver(source)

    def list_sources(self) -> list[dict[str, Any]]:
        return [s.to_dict() for s in self._sources.values()]

    def get_source(self, source_id: str) -> RTPSource | None:
        for s in self._sources.values():
            if s.source_id == source_id:
                return s
        return None

    async def _scan_loop(self) -> None:
        """Periodically check for new/removed video nodes."""
        while True:
            try:
                await self._scan()
                await asyncio.sleep(5.0)
            except asyncio.CancelledError:
                return

    async def _scan(self) -> None:
        # Find nodes with video RTP capability
        active_ids = set()
        for node in self._state.nodes.values():
            # Check for video_rtp_port in node properties
            # This would be advertised in mDNS TXT: video_rtp_port=5004
            rtp_port = getattr(node, "video_rtp_port", None)
            if not rtp_port:
                continue

            active_ids.add(node.id)
            if node.id not in self._sources:
                source = RTPSource(
                    node_id=node.id,
                    host=node.host,
                    rtp_port=rtp_port,
                )
                self._sources[node.id] = source
                await self._start_receiver(source)

        # Stop receivers for nodes that went offline
        for node_id in list(self._sources):
            if node_id not in active_ids:
                await self._stop_receiver(self._sources.pop(node_id))

    async def _start_receiver(self, source: RTPSource) -> None:
        """Start an ffmpeg RTP receiver → HLS pipeline."""
        out_dir = CAPTURE_DIR / source.source_id
        out_dir.mkdir(parents=True, exist_ok=True)
        manifest = out_dir / "stream.m3u8"
        seg_pattern = str(out_dir / "seg_%05d.ts")

        # Build SDP content for ffmpeg input
        sdp = (
            f"v=0\n"
            f"c=IN IP4 {source.host}\n"
            f"m=video {source.rtp_port} RTP/AVP 96\n"
            f"a=rtpmap:96 H265/90000\n"
        )
        sdp_file = out_dir / "stream.sdp"
        sdp_file.write_text(sdp)

        cmd = [
            "ffmpeg", "-hide_banner", "-loglevel", "warning",
            "-protocol_whitelist", "file,rtp,udp",
            "-i", str(sdp_file),
            "-c:v", "copy",  # No re-encode — pass through H.265
            "-f", "hls",
            "-hls_time", "1",
            "-hls_list_size", "4",
            "-hls_flags", "delete_segments+independent_segments+append_list",
            "-hls_segment_filename", seg_pattern,
            str(manifest),
        ]

        try:
            source.proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.PIPE,
            )
            source.active = True
            log.info("RTP receiver started: %s from %s:%d (pid %d)",
                     source.source_id, source.host, source.rtp_port, source.proc.pid)
        except Exception as e:
            log.warning("Failed to start RTP receiver for %s: %s", source.node_id, e)

    async def _stop_receiver(self, source: RTPSource) -> None:
        if source.proc and source.proc.returncode is None:
            source.proc.terminate()
            try:
                await asyncio.wait_for(source.proc.wait(), timeout=5.0)
            except asyncio.TimeoutError:
                source.proc.kill()
        source.active = False
