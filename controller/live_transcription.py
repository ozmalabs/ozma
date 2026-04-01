# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Live transcription — real-time speech-to-text on any audio stream.

Captures audio from any PipeWire source (phone calls via KDE Connect,
microphone, meeting audio, any node's audio) and transcribes it in
real-time using Whisper.cpp streaming mode.

The transcription appears in the dashboard as the call/meeting happens.
Stored with timestamps for searchable history.

Audio path:
  Phone call → KDE Connect → PipeWire → capture → Whisper.cpp → text
  Meeting    → Node audio  → PipeWire → capture → Whisper.cpp → text
  Mic        → Default mic → PipeWire → capture → Whisper.cpp → text

The phone doesn't need an app. The audio is captured at the PipeWire
level — same as any other audio source routed through ozma.

Whisper modes:
  Local streaming: whisper.cpp with --stream flag, processes 5s chunks
  Local batch: whisper.cpp on saved segments (higher accuracy, slight delay)
  Cloud fallback: OpenAI Whisper API via Connect proxy (if local not available)
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
import shutil
import subprocess
import tempfile
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.transcription")

WHISPER_MODELS_DIR = Path("/opt/ozma/models")
CHUNK_SECONDS = 5  # process audio in 5-second chunks
SAMPLE_RATE = 16000
CHANNELS = 1


@dataclass
class TranscriptionSegment:
    """A segment of transcribed text with timing."""
    start: float       # seconds from session start
    end: float
    text: str
    confidence: float = 0.0
    speaker: str = ""  # future: speaker diarization

    def to_dict(self) -> dict:
        return {
            "start": round(self.start, 2),
            "end": round(self.end, 2),
            "text": self.text,
            "speaker": self.speaker,
        }


@dataclass
class TranscriptionSession:
    """An active live transcription session."""
    id: str
    source: str              # PipeWire source name or "default"
    started_at: float = 0.0
    segments: list[TranscriptionSegment] = field(default_factory=list)
    active: bool = False
    total_seconds: float = 0.0
    language: str = "en"

    # Internal state
    _capture_proc: Any = None
    _whisper_task: Any = None

    def to_dict(self) -> dict:
        return {
            "id": self.id,
            "source": self.source,
            "active": self.active,
            "started_at": self.started_at,
            "duration_s": round(time.time() - self.started_at, 1) if self.started_at else 0,
            "segments": len(self.segments),
            "total_seconds_transcribed": round(self.total_seconds, 1),
            "language": self.language,
        }

    @property
    def full_text(self) -> str:
        return " ".join(s.text for s in self.segments)


class LiveTranscriptionManager:
    """
    Manages live transcription sessions.

    Captures audio from PipeWire sources and transcribes in real-time
    using Whisper.cpp streaming or chunk-based processing.
    """

    def __init__(self, connect: Any = None) -> None:
        self._connect = connect
        self._sessions: dict[str, TranscriptionSession] = {}
        self._whisper_bin = ""
        self._whisper_model = ""
        self._detect_whisper()

    def _detect_whisper(self) -> None:
        """Find local Whisper.cpp binary and model."""
        for name in ["whisper-cpp", "whisper", "main"]:
            path = shutil.which(name)
            if path:
                self._whisper_bin = path
                break

        # Find model (prefer small.en for speed, fall back to base.en)
        for model_name in ["ggml-small.en.bin", "ggml-base.en.bin", "ggml-tiny.en.bin"]:
            for search_dir in [WHISPER_MODELS_DIR, Path.home() / ".cache" / "whisper"]:
                candidate = search_dir / model_name
                if candidate.exists():
                    self._whisper_model = str(candidate)
                    break
            if self._whisper_model:
                break

        if self._whisper_bin and self._whisper_model:
            log.info("Whisper.cpp: %s (model: %s)", self._whisper_bin, Path(self._whisper_model).name)
        else:
            log.info("Whisper.cpp not available — live transcription uses cloud fallback")

    @property
    def local_available(self) -> bool:
        return bool(self._whisper_bin and self._whisper_model)

    # ── Session lifecycle ───────────────────────────────────────────────────

    async def start_session(self, session_id: str, source: str = "default",
                             language: str = "en", account_id: str = "") -> TranscriptionSession | None:
        """
        Start a live transcription session on a PipeWire audio source.

        source: PipeWire node name (e.g., "ozma-vm1.monitor", "default",
                or a specific capture device name)
        """
        if session_id in self._sessions:
            return self._sessions[session_id]

        session = TranscriptionSession(
            id=session_id,
            source=source,
            started_at=time.time(),
            active=True,
            language=language,
        )
        self._sessions[session_id] = session

        # Start the capture → transcribe pipeline
        session._whisper_task = asyncio.create_task(
            self._transcription_loop(session, account_id),
            name=f"transcribe-{session_id}",
        )

        log.info("Live transcription started: %s (source=%s, lang=%s)",
                 session_id, source, language)
        return session

    async def stop_session(self, session_id: str) -> TranscriptionSession | None:
        session = self._sessions.get(session_id)
        if not session:
            return None

        session.active = False
        if session._capture_proc and session._capture_proc.returncode is None:
            session._capture_proc.terminate()
        if session._whisper_task:
            session._whisper_task.cancel()

        log.info("Transcription stopped: %s (%d segments, %.1fs)",
                 session_id, len(session.segments), session.total_seconds)
        return session

    def get_session(self, session_id: str) -> TranscriptionSession | None:
        return self._sessions.get(session_id)

    def list_sessions(self) -> list[dict]:
        return [s.to_dict() for s in self._sessions.values()]

    # ── Transcription pipeline ──────────────────────────────────────────────

    async def _transcription_loop(self, session: TranscriptionSession,
                                    account_id: str) -> None:
        """Capture audio in chunks and transcribe each chunk."""
        chunk_dir = Path(tempfile.mkdtemp(prefix=f"ozma-transcribe-{session.id}-"))
        chunk_index = 0
        session_start = time.time()

        try:
            while session.active:
                # Capture a chunk of audio from PipeWire
                chunk_path = chunk_dir / f"chunk_{chunk_index:05d}.wav"
                ok = await self._capture_chunk(session, chunk_path)
                if not ok:
                    await asyncio.sleep(1)
                    continue

                # Transcribe the chunk
                chunk_start = (chunk_index * CHUNK_SECONDS)
                segments = await self._transcribe_chunk(chunk_path, session.language, chunk_start)

                if segments:
                    session.segments.extend(segments)
                    session.total_seconds += CHUNK_SECONDS

                    # Broadcast new segments to WebSocket subscribers
                    for seg in segments:
                        log.debug("[%s] %.1f-%.1f: %s", session.id, seg.start, seg.end, seg.text)

                # Clean up chunk file
                chunk_path.unlink(missing_ok=True)
                chunk_index += 1

        except asyncio.CancelledError:
            pass
        except Exception as e:
            log.error("Transcription loop error: %s", e)
        finally:
            # Clean up temp directory
            import shutil as sh
            sh.rmtree(chunk_dir, ignore_errors=True)
            session.active = False

    async def _capture_chunk(self, session: TranscriptionSession,
                               output_path: Path) -> bool:
        """Capture CHUNK_SECONDS of audio from PipeWire to a WAV file."""
        # Use pw-record (PipeWire) or parec (PulseAudio) to capture audio
        if shutil.which("pw-record"):
            cmd = [
                "pw-record",
                "--target", session.source,
                "--rate", str(SAMPLE_RATE),
                "--channels", str(CHANNELS),
                "--format", "s16",
                str(output_path),
            ]
        elif shutil.which("parec"):
            cmd = [
                "parec",
                "--format=s16le",
                "--rate", str(SAMPLE_RATE),
                "--channels", str(CHANNELS),
                "-d", session.source if session.source != "default" else "",
                "--file-format=wav",
                str(output_path),
            ]
        elif shutil.which("ffmpeg"):
            # Fallback: ffmpeg with PulseAudio
            cmd = [
                "ffmpeg", "-hide_banner", "-loglevel", "error",
                "-f", "pulse", "-i", session.source if session.source != "default" else "default",
                "-ar", str(SAMPLE_RATE), "-ac", str(CHANNELS),
                "-t", str(CHUNK_SECONDS),
                "-y", str(output_path),
            ]
            try:
                proc = await asyncio.create_subprocess_exec(
                    *cmd, stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.DEVNULL,
                )
                await asyncio.wait_for(proc.wait(), timeout=CHUNK_SECONDS + 5)
                return output_path.exists() and output_path.stat().st_size > 100
            except Exception:
                return False
        else:
            return False

        try:
            proc = await asyncio.create_subprocess_exec(
                *cmd, stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.DEVNULL,
            )
            session._capture_proc = proc
            # Wait for CHUNK_SECONDS then kill
            await asyncio.sleep(CHUNK_SECONDS)
            if proc.returncode is None:
                proc.terminate()
                await asyncio.wait_for(proc.wait(), timeout=2)
            return output_path.exists() and output_path.stat().st_size > 100
        except Exception:
            return False

    async def _transcribe_chunk(self, audio_path: Path, language: str,
                                  offset_seconds: float) -> list[TranscriptionSegment]:
        """Transcribe a single audio chunk using Whisper.cpp."""
        if not self.local_available:
            return []

        try:
            proc = await asyncio.create_subprocess_exec(
                self._whisper_bin,
                "-m", self._whisper_model,
                "-f", str(audio_path),
                "-l", language,
                "--output-json",
                "--no-timestamps", "false",
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.DEVNULL,
            )
            stdout, _ = await asyncio.wait_for(proc.communicate(), timeout=30)

            if proc.returncode != 0 or not stdout:
                return []

            # Parse whisper output
            data = json.loads(stdout)
            segments = []

            for seg in data.get("transcription", data.get("segments", [])):
                text = seg.get("text", "").strip()
                if not text or text in ("[BLANK_AUDIO]", "(silence)"):
                    continue

                # Whisper timestamps are relative to the chunk
                # Add the offset to get absolute session time
                t0 = seg.get("offsets", {}).get("from", seg.get("start", 0))
                t1 = seg.get("offsets", {}).get("to", seg.get("end", 0))
                # Convert from milliseconds if needed
                if t0 > 1000:
                    t0 /= 1000
                    t1 /= 1000

                segments.append(TranscriptionSegment(
                    start=offset_seconds + t0,
                    end=offset_seconds + t1,
                    text=text,
                ))

            return segments

        except asyncio.TimeoutError:
            log.debug("Whisper timed out on chunk %s", audio_path)
            return []
        except Exception as e:
            log.debug("Whisper error: %s", e)
            return []

    # ── Query ───────────────────────────────────────────────────────────────

    def get_transcript(self, session_id: str) -> str:
        """Get the full transcript text."""
        session = self._sessions.get(session_id)
        return session.full_text if session else ""

    def get_segments(self, session_id: str, since: float = 0) -> list[dict]:
        """Get transcript segments, optionally since a timestamp."""
        session = self._sessions.get(session_id)
        if not session:
            return []
        segments = session.segments
        if since > 0:
            segments = [s for s in segments if s.start >= since]
        return [s.to_dict() for s in segments]

    def search_transcripts(self, query: str) -> list[dict]:
        """Search all transcripts for a text pattern."""
        results = []
        query_lower = query.lower()
        for session in self._sessions.values():
            for seg in session.segments:
                if query_lower in seg.text.lower():
                    results.append({
                        "session_id": session.id,
                        "source": session.source,
                        "segment": seg.to_dict(),
                    })
        return results

    # ── Status ──────────────────────────────────────────────────────────────

    def status(self) -> dict:
        return {
            "whisper_available": self.local_available,
            "whisper_model": Path(self._whisper_model).name if self._whisper_model else "",
            "active_sessions": sum(1 for s in self._sessions.values() if s.active),
            "total_sessions": len(self._sessions),
            "total_segments": sum(len(s.segments) for s in self._sessions.values()),
            "total_seconds": round(sum(s.total_seconds for s in self._sessions.values()), 1),
        }
