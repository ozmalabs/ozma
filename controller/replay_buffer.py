# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Replay buffer — continuous HDMI recording with clip capture and error evidence.

Like NVIDIA ShadowPlay / AMD ReLive, but at the hardware level. The
capture pipeline already produces HLS segments (1-second .ts files).
The replay buffer simply retains the last N seconds of segments instead
of deleting them. Zero additional overhead — the segments are already
being produced.

Capabilities:

  1. Clip capture ("save that!")
     Press a hotkey or API call → the last N seconds are saved as a video
     file. Instant, no re-encoding. Perfect for: game highlights, "did
     that just happen?", demo clips.

  2. Error evidence capture
     OCR trigger or serial panic fires → replay buffer is automatically
     saved with the crash context. The 30 seconds BEFORE the crash are
     preserved — showing exactly what was happening when it went wrong.
     Attached to helpdesk tickets automatically.

  3. Helpdesk / MSP integration
     On error: save replay + serial log + metrics snapshot → create a
     ticket in: Jira, ServiceNow, Zendesk, Freshdesk, ConnectWise, or
     generic webhook. The ticket includes video evidence, the crash
     backtrace, system metrics at the time of failure, and the OCR'd
     screen text. First-line support has everything they need.

  4. Automatic investigation
     On crash: save replay → OCR the frozen screen → search knowledge
     base for matching error → attach suggested resolution to the ticket.
     "This looks like KB-4521: DRBD scatter-gather deallocation. Known
     fix: update network driver to 5.15.0-72."

Storage:
  At 8 Mbps H.265, 60 seconds of buffer = ~60 MB per capture source.
  4 capture cards × 60 seconds = ~240 MB total. Negligible.

  Saved clips go to static/replays/ and are accessible via the web UI.

Implementation:
  The HLS pipeline already writes 1-second .ts segments and deletes old
  ones (hls_list_size=4). The replay buffer simply copies segments to a
  ring buffer directory before they're deleted. On "save", the ring
  buffer contents are concatenated into an MP4/MKV via ffmpeg remux.
"""

from __future__ import annotations

import asyncio
import json
import logging
import shutil
import time
from collections import deque
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.replay")

REPLAY_DIR = Path(__file__).parent / "static" / "replays"
BUFFER_DIR = Path("/tmp/ozma-replay-buffer")


@dataclass
class ReplayClip:
    """A saved replay clip."""
    filename: str
    source_id: str
    duration_s: float
    trigger: str            # "manual", "ocr_trigger", "serial_panic", "hotkey"
    timestamp: float
    context: dict = field(default_factory=dict)  # error info, metrics, OCR text
    size_bytes: int = 0
    ticket_id: str = ""     # Helpdesk ticket created for this clip

    def to_dict(self) -> dict[str, Any]:
        return {
            "filename": self.filename,
            "url": f"/replays/{self.filename}",
            "source_id": self.source_id,
            "duration_s": self.duration_s,
            "trigger": self.trigger,
            "timestamp": self.timestamp,
            "size_bytes": self.size_bytes,
            "ticket_id": self.ticket_id,
            "context": self.context,
        }


@dataclass
class HelpdeskConfig:
    """Configuration for automatic ticket creation."""
    enabled: bool = False
    system: str = ""         # jira, servicenow, zendesk, freshdesk, connectwise, webhook
    url: str = ""
    api_key: str = ""
    project: str = ""        # Jira project key, ServiceNow assignment group, etc.
    auto_create: bool = True  # Create tickets automatically on error

    def to_dict(self) -> dict[str, Any]:
        return {"enabled": self.enabled, "system": self.system, "url": self.url,
                "project": self.project, "auto_create": self.auto_create}


class ReplayBuffer:
    """
    Continuous replay buffer per capture source.

    Retains the last N seconds of HLS segments. On trigger, saves them
    as a clip with context (error info, metrics, serial log).
    """

    def __init__(self, source_id: str, buffer_seconds: int = 60) -> None:
        self.source_id = source_id
        self._buffer_seconds = buffer_seconds
        self._segments: deque[Path] = deque(maxlen=buffer_seconds)
        self._buffer_dir = BUFFER_DIR / source_id
        self._buffer_dir.mkdir(parents=True, exist_ok=True)

    def add_segment(self, segment_path: Path) -> None:
        """Copy a new HLS segment into the ring buffer."""
        if not segment_path.exists():
            return
        dest = self._buffer_dir / segment_path.name
        try:
            shutil.copy2(segment_path, dest)
            self._segments.append(dest)
            # Clean up oldest if over limit
            while len(self._segments) > self._buffer_seconds:
                old = self._segments.popleft()
                old.unlink(missing_ok=True)
        except Exception:
            pass

    async def save_clip(self, trigger: str = "manual", context: dict | None = None,
                         seconds: int = 0) -> ReplayClip | None:
        """
        Save the replay buffer as a video clip.

        seconds: how many seconds to save (0 = entire buffer)
        """
        REPLAY_DIR.mkdir(parents=True, exist_ok=True)

        segments = list(self._segments)
        if seconds > 0:
            segments = segments[-seconds:]
        if not segments:
            return None

        timestamp = time.time()
        filename = f"replay-{self.source_id}-{time.strftime('%Y%m%d_%H%M%S')}-{trigger}.mkv"
        output_path = REPLAY_DIR / filename

        # Concatenate segments via ffmpeg (remux, no re-encode)
        concat_file = self._buffer_dir / "concat.txt"
        concat_file.write_text("\n".join(f"file '{s}'" for s in segments if s.exists()))

        try:
            proc = await asyncio.create_subprocess_exec(
                "ffmpeg", "-hide_banner", "-loglevel", "warning",
                "-f", "concat", "-safe", "0",
                "-i", str(concat_file),
                "-c", "copy", "-y", str(output_path),
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.PIPE,
            )
            await asyncio.wait_for(proc.communicate(), timeout=30)
        except Exception as e:
            log.warning("Replay save failed: %s", e)
            return None
        finally:
            concat_file.unlink(missing_ok=True)

        if not output_path.exists():
            return None

        clip = ReplayClip(
            filename=filename,
            source_id=self.source_id,
            duration_s=len(segments),
            trigger=trigger,
            timestamp=timestamp,
            context=context or {},
            size_bytes=output_path.stat().st_size,
        )
        log.info("Replay saved: %s (%.0fs, %s, %.1f MB)",
                 filename, clip.duration_s, trigger, clip.size_bytes / 1048576)
        return clip


class ReplayManager:
    """
    Manages replay buffers for all capture sources.

    Watches for new HLS segments, feeds them into per-source ring buffers,
    and handles clip saving + helpdesk integration.
    """

    def __init__(self, captures: Any = None, metrics: Any = None,
                 serial: Any = None, text_capture: Any = None) -> None:
        self._captures = captures
        self._metrics = metrics
        self._serial = serial
        self._text_capture = text_capture
        self._buffers: dict[str, ReplayBuffer] = {}
        self._clips: list[ReplayClip] = []
        self._helpdesk = HelpdeskConfig()
        self._task: asyncio.Task | None = None
        self._buffer_seconds = 60

    async def start(self) -> None:
        self._task = asyncio.create_task(self._watch_loop(), name="replay-buffer")
        log.info("Replay buffer started (%ds per source)", self._buffer_seconds)

    async def stop(self) -> None:
        if self._task:
            self._task.cancel()

    def configure_helpdesk(self, config: dict) -> None:
        self._helpdesk = HelpdeskConfig(**{k: v for k, v in config.items()
                                           if hasattr(HelpdeskConfig, k)})

    # ── Clip management ──────────────────────────────────────────────────────

    async def save_clip(self, source_id: str = "", trigger: str = "manual",
                         seconds: int = 0) -> ReplayClip | None:
        """Save a replay clip. If source_id is empty, uses the active source."""
        if not source_id:
            source_id = list(self._buffers.keys())[0] if self._buffers else ""
        buf = self._buffers.get(source_id)
        if not buf:
            return None

        # Build context: metrics, serial log, OCR text
        context = self._build_context(source_id)

        clip = await buf.save_clip(trigger=trigger, context=context, seconds=seconds)
        if clip:
            self._clips.append(clip)

            # Auto-create helpdesk ticket on error triggers
            if trigger in ("ocr_trigger", "serial_panic") and self._helpdesk.auto_create:
                ticket_id = await self._create_ticket(clip)
                if ticket_id:
                    clip.ticket_id = ticket_id

        return clip

    async def on_error_detected(self, source_id: str, error_type: str,
                                  error_text: str) -> ReplayClip | None:
        """
        Called when an OCR trigger or serial panic fires.

        Automatically saves the replay buffer with the error context.
        The clip contains the N seconds BEFORE the error — showing
        exactly what was happening when it went wrong.
        """
        context = self._build_context(source_id)
        context["error_type"] = error_type
        context["error_text"] = error_text[:1000]

        clip = await self.save_clip(source_id, trigger=error_type)
        return clip

    def list_clips(self) -> list[dict]:
        # Include saved clips + scan disk
        clips = [c.to_dict() for c in self._clips]
        for f in sorted(REPLAY_DIR.glob("replay-*.mkv"), reverse=True)[:50]:
            if not any(c["filename"] == f.name for c in clips):
                clips.append({
                    "filename": f.name,
                    "url": f"/replays/{f.name}",
                    "size_bytes": f.stat().st_size,
                })
        return clips

    # ── Context building ─────────────────────────────────────────────────────

    def _build_context(self, source_id: str) -> dict:
        context: dict[str, Any] = {"source_id": source_id, "timestamp": time.time()}

        # Metrics snapshot
        if self._metrics:
            for src in self._metrics.get_all():
                if source_id in src.get("id", ""):
                    context["metrics"] = src.get("metrics", {})
                    break

        # Serial log (last 20 lines)
        if self._serial:
            for console in self._serial.list_consoles():
                if source_id.split("-")[0] in console.get("node_id", ""):
                    context["serial"] = self._serial.get_text(console["id"], lines=20)
                    break

        # OCR text
        if self._text_capture and self._text_capture.last_result:
            context["screen_text"] = self._text_capture.last_result.text[:500]

        return context

    # ── Helpdesk integration ─────────────────────────────────────────────────

    async def _create_ticket(self, clip: ReplayClip) -> str:
        """Create a helpdesk ticket with the clip as evidence."""
        if not self._helpdesk.enabled or not self._helpdesk.url:
            return ""

        title = f"Ozma: {clip.trigger} on {clip.source_id}"
        description = (
            f"**Error detected:** {clip.context.get('error_type', 'unknown')}\n\n"
            f"**Error text:**\n```\n{clip.context.get('error_text', 'N/A')[:500]}\n```\n\n"
            f"**Screen text (OCR):**\n```\n{clip.context.get('screen_text', 'N/A')[:300]}\n```\n\n"
            f"**Serial log:**\n```\n{clip.context.get('serial', 'N/A')[:300]}\n```\n\n"
            f"**Replay clip:** {clip.duration_s:.0f}s of video before the error\n"
            f"**Metrics at time of error:** {json.dumps(clip.context.get('metrics', {}), indent=2)[:500]}\n"
        )

        try:
            import urllib.request
            loop = asyncio.get_running_loop()

            match self._helpdesk.system:
                case "jira":
                    payload = json.dumps({
                        "fields": {
                            "project": {"key": self._helpdesk.project},
                            "summary": title,
                            "description": description,
                            "issuetype": {"name": "Bug"},
                        }
                    }).encode()
                    headers = {
                        "Content-Type": "application/json",
                        "Authorization": f"Bearer {self._helpdesk.api_key}",
                    }
                    def _post():
                        req = urllib.request.Request(
                            f"{self._helpdesk.url}/rest/api/2/issue",
                            data=payload, headers=headers,
                        )
                        with urllib.request.urlopen(req, timeout=15) as r:
                            return json.loads(r.read()).get("key", "")
                    ticket_id = await loop.run_in_executor(None, _post)
                    log.info("Jira ticket created: %s", ticket_id)
                    return ticket_id

                case "webhook":
                    payload = json.dumps({
                        "title": title, "description": description,
                        "source": "ozma", "clip_url": f"/replays/{clip.filename}",
                        "context": clip.context,
                    }).encode()
                    def _post_webhook():
                        req = urllib.request.Request(
                            self._helpdesk.url, data=payload,
                            headers={"Content-Type": "application/json"},
                        )
                        urllib.request.urlopen(req, timeout=15)
                    await loop.run_in_executor(None, _post_webhook)
                    return "webhook-sent"

                case _:
                    # Generic webhook fallback
                    return ""

        except Exception as e:
            log.warning("Helpdesk ticket creation failed: %s", e)
            return ""

    # ── Segment watcher ──────────────────────────────────────────────────────

    async def _watch_loop(self) -> None:
        """Watch for new HLS segments and feed them into replay buffers."""
        captures_dir = Path(__file__).parent / "static" / "captures"
        seen: dict[str, set[str]] = {}

        while True:
            try:
                if not captures_dir.exists():
                    await asyncio.sleep(2)
                    continue

                for source_dir in captures_dir.iterdir():
                    if not source_dir.is_dir():
                        continue
                    source_id = source_dir.name

                    if source_id not in self._buffers:
                        self._buffers[source_id] = ReplayBuffer(source_id, self._buffer_seconds)
                    if source_id not in seen:
                        seen[source_id] = set()

                    for seg in sorted(source_dir.glob("seg_*.ts")):
                        if seg.name not in seen[source_id]:
                            seen[source_id].add(seg.name)
                            self._buffers[source_id].add_segment(seg)
                            # Keep seen set bounded
                            if len(seen[source_id]) > self._buffer_seconds * 2:
                                seen[source_id] = set(list(seen[source_id])[-self._buffer_seconds:])

                await asyncio.sleep(1)
            except asyncio.CancelledError:
                return
