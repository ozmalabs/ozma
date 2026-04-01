# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Session recording — save captured display video to file on demand.

Records the active capture source's HLS stream to an MP4/MKV file.
Uses ffmpeg to remux HLS segments into a continuous recording.

API:
  POST /api/v1/recording/start  {"source_id": "hdmi-0"}
  POST /api/v1/recording/stop
  GET  /api/v1/recording/status
  GET  /api/v1/recording/list   — list saved recordings

Recordings are saved to controller/static/recordings/ with timestamp
and scenario name.
"""

from __future__ import annotations

import asyncio
import logging
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.recording")

RECORDINGS_DIR = Path(__file__).parent / "static" / "recordings"


@dataclass
class Recording:
    """An active or completed recording."""
    filename: str
    source_id: str
    scenario_id: str
    started_at: float
    stopped_at: float = 0.0
    size_bytes: int = 0

    @property
    def duration_s(self) -> float:
        end = self.stopped_at or time.time()
        return end - self.started_at

    def to_dict(self) -> dict[str, Any]:
        return {
            "filename": self.filename,
            "source_id": self.source_id,
            "scenario_id": self.scenario_id,
            "started_at": self.started_at,
            "duration_s": round(self.duration_s, 1),
            "size_bytes": self.size_bytes,
            "url": f"/recordings/{self.filename}" if self.stopped_at else None,
        }


class SessionRecorder:
    """Records capture streams to files."""

    def __init__(self) -> None:
        self._proc: asyncio.subprocess.Process | None = None
        self._active: Recording | None = None
        self._history: list[Recording] = []
        RECORDINGS_DIR.mkdir(parents=True, exist_ok=True)

    @property
    def is_recording(self) -> bool:
        return self._proc is not None and self._proc.returncode is None

    async def start_recording(self, source_id: str, hls_path: str, scenario_id: str = "") -> bool:
        if self.is_recording:
            return False

        timestamp = time.strftime("%Y%m%d_%H%M%S")
        scenario_slug = scenario_id.replace(" ", "_")[:20] if scenario_id else "unknown"
        filename = f"{timestamp}_{scenario_slug}_{source_id}.mkv"
        output_path = RECORDINGS_DIR / filename

        # Use ffmpeg to record from HLS manifest
        manifest = Path(__file__).parent / "static" / hls_path.lstrip("/")
        if not manifest.exists():
            log.warning("HLS manifest not found: %s", manifest)
            return False

        cmd = [
            "ffmpeg", "-hide_banner", "-loglevel", "warning",
            "-live_start_index", "-1",
            "-i", str(manifest),
            "-c", "copy",  # Remux, no re-encode
            "-y", str(output_path),
        ]

        try:
            self._proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.PIPE,
            )
            self._active = Recording(
                filename=filename,
                source_id=source_id,
                scenario_id=scenario_id,
                started_at=time.time(),
            )
            log.info("Recording started: %s (pid %d)", filename, self._proc.pid)
            return True
        except Exception as e:
            log.warning("Failed to start recording: %s", e)
            return False

    async def stop_recording(self) -> Recording | None:
        if not self._proc or not self._active:
            return None

        self._proc.terminate()
        try:
            await asyncio.wait_for(self._proc.wait(), timeout=10.0)
        except asyncio.TimeoutError:
            self._proc.kill()

        self._active.stopped_at = time.time()
        output_path = RECORDINGS_DIR / self._active.filename
        if output_path.exists():
            self._active.size_bytes = output_path.stat().st_size

        recording = self._active
        self._history.append(recording)
        self._active = None
        self._proc = None

        log.info("Recording stopped: %s (%.1fs, %d bytes)",
                 recording.filename, recording.duration_s, recording.size_bytes)
        return recording

    def status(self) -> dict[str, Any]:
        return {
            "recording": self.is_recording,
            "active": self._active.to_dict() if self._active else None,
        }

    def list_recordings(self) -> list[dict[str, Any]]:
        # Include history + scan disk for older recordings
        files = sorted(RECORDINGS_DIR.glob("*.mkv"), reverse=True)
        result = []
        known = {r.filename for r in self._history}
        for f in files:
            if f.name in known:
                rec = next(r for r in self._history if r.filename == f.name)
                result.append(rec.to_dict())
            else:
                result.append({
                    "filename": f.name,
                    "size_bytes": f.stat().st_size,
                    "url": f"/recordings/{f.name}",
                })
        return result
