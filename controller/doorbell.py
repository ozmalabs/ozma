# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Doorbell-on-desk — routes doorbell events to the active machine.

When a Frigate-connected doorbell camera detects a button press or person,
the controller:
  1. Fires a doorbell.ringing WebSocket event to all connected clients
  2. Connected web clients (OzmaConsole, dashboard) play a chime via Web
     Audio API and show a notification overlay with a live camera snapshot
  3. The user clicks Answer or Dismiss — no phone required

Two-way audio (answer path):
  Phase 1 (current): stub — logs intent, answer state tracked
  Phase 2: ffmpeg pulls RTSP audio from camera → VBAN → active node → headset
            headset mic → VBAN → controller → camera RTSP backchannel (Reolink)
  See _start_audio() below for the planned implementation.

Event flow:
  Frigate MQTT (frigate/<cam>/doorbell)
    → ozma_bridge POSTs to POST /api/v1/frigate/webhook
      → DoorbellManager.receive_event()
        → state.events.put(doorbell.ringing)
          → _broadcast() → WebSocket clients
            → OzmaConsole.html / dashboard show overlay + play chime
"""

from __future__ import annotations

import asyncio
import logging
import time
import uuid
from dataclasses import dataclass, field
from typing import Any

log = logging.getLogger("ozma.doorbell")

RING_TIMEOUT_S = 30      # auto-expire if not answered/dismissed
SESSION_TTL_S  = 300     # clean up old sessions after 5 minutes


@dataclass
class DoorbellSession:
    id: str
    camera: str
    frigate_url: str
    started_at: float
    state: str = "ringing"           # ringing | answered | dismissed | expired
    active_node_id: str | None = None
    snapshot_url: str = ""           # Frigate latest snapshot URL
    _audio_proc: Any = field(default=None, repr=False)

    def to_dict(self) -> dict:
        return {
            "id": self.id,
            "camera": self.camera,
            "started_at": self.started_at,
            "state": self.state,
            "active_node_id": self.active_node_id,
            "snapshot_url": f"/api/v1/doorbell/{self.id}/snapshot",
            "age_s": round(time.time() - self.started_at, 1),
        }


class DoorbellManager:
    """Manages doorbell sessions and routes events to the active machine."""

    def __init__(self, state: Any, frigate_url: str = "http://localhost:5000") -> None:
        self._state = state
        self._frigate_url = frigate_url
        self._sessions: dict[str, DoorbellSession] = {}
        self._expire_task: asyncio.Task | None = None

    async def start(self) -> None:
        self._expire_task = asyncio.create_task(
            self._expire_loop(), name="doorbell-expire"
        )
        log.info("DoorbellManager started (frigate=%s)", self._frigate_url)

    async def stop(self) -> None:
        if self._expire_task:
            self._expire_task.cancel()

    # ── Public API ──────────────────────────────────────────────────────────

    async def receive_event(self, camera: str, event_type: str, payload: dict) -> DoorbellSession | None:
        """Called when Frigate sends a doorbell event for a camera.

        event_type is 'doorbell' (button press) or 'person' (detection).
        Returns the new session, or None if the event was ignored.
        """
        # Debounce: ignore if a ringing session for this camera is < 5s old
        for s in self._sessions.values():
            if s.camera == camera and s.state == "ringing":
                age = time.time() - s.started_at
                if age < 5:
                    log.debug("Doorbell debounce: ignoring %s event for %s (age=%.1fs)", event_type, camera, age)
                    return None

        session_id = uuid.uuid4().hex[:8]
        session = DoorbellSession(
            id=session_id,
            camera=camera,
            frigate_url=self._frigate_url,
            started_at=time.time(),
            active_node_id=getattr(self._state, "active_node_id", None),
            snapshot_url=f"{self._frigate_url}/api/{camera}/latest.jpg",
        )
        self._sessions[session_id] = session

        log.info("Doorbell ringing: camera=%s session=%s active_node=%s",
                 camera, session_id, session.active_node_id)

        await self._push_event("doorbell.ringing", session)
        return session

    async def answer(self, session_id: str) -> bool:
        """Answer a ringing doorbell. Returns True if state was changed."""
        session = self._sessions.get(session_id)
        if not session:
            return False
        if session.state != "ringing":
            log.debug("Answer ignored: session %s is %s", session_id, session.state)
            return False

        session.state = "answered"
        log.info("Doorbell answered: session=%s camera=%s", session_id, session.camera)
        await self._push_event("doorbell.answered", session)
        await self._start_audio(session)
        return True

    async def dismiss(self, session_id: str) -> bool:
        """Dismiss a ringing doorbell. Returns True if state was changed."""
        session = self._sessions.get(session_id)
        if not session:
            return False
        if session.state != "ringing":
            return False

        session.state = "dismissed"
        log.info("Doorbell dismissed: session=%s camera=%s", session_id, session.camera)
        await self._push_event("doorbell.dismissed", session)
        await self._stop_audio(session)
        return True

    def get_session(self, session_id: str) -> DoorbellSession | None:
        return self._sessions.get(session_id)

    def get_sessions(self) -> list[dict]:
        return [s.to_dict() for s in self._sessions.values()]

    def get_snapshot_url(self, session_id: str) -> str | None:
        s = self._sessions.get(session_id)
        return s.snapshot_url if s else None

    # ── Internal ────────────────────────────────────────────────────────────

    async def _push_event(self, event_type: str, session: DoorbellSession) -> None:
        await self._state.events.put({
            "type": event_type,
            **session.to_dict(),
        })

    async def _start_audio(self, session: DoorbellSession) -> None:
        """Start two-way audio between the camera and the active node's headset.

        Phase 2 implementation plan:
          Inbound (camera mic → headset):
            ffmpeg -i <camera_rtsp> -vn -acodec pcm_s16le -ar 48000 -ac 2 \
                   -f vban udp://<active_node_ip>:6980
            The active node's VBAN input receives the stream and routes it
            to PipeWire → headset output via the existing audio routing.

          Outbound (headset mic → camera speaker):
            Reolink supports RTSP backchannel (ANNOUNCE/RECORD) or a UDP
            push endpoint. The active node captures the headset mic via VBAN
            and streams it to the controller. The controller forwards it to
            the camera via backchannel.

            ffmpeg -f vban -i udp://0.0.0.0:6981 \
                   -acodec pcm_s16le -ar 8000 -ac 1 \
                   -f rtsp rtsp://<camera_ip>/backchannel

          This requires:
            - VBAN bidirectional session between controller and active node
            - ffmpeg pipeline per doorbell session (managed as subprocess)
            - Camera-specific backchannel URL (Reolink, Dahua differ)
            - Cleanup on dismiss/expire

        For now: log the intent so the answered state is tracked and audio
        can be wired in Phase 2 without changing the session lifecycle.
        """
        log.info(
            "Doorbell audio: session=%s camera=%s active_node=%s — "
            "two-way audio not yet implemented (Phase 2)",
            session.id, session.camera, session.active_node_id,
        )

    async def _stop_audio(self, session: DoorbellSession) -> None:
        if session._audio_proc:
            try:
                session._audio_proc.terminate()
            except ProcessLookupError:
                pass
            session._audio_proc = None

    async def _expire_loop(self) -> None:
        while True:
            await asyncio.sleep(5)
            now = time.time()
            expired = []
            for session in list(self._sessions.values()):
                if session.state == "ringing" and now - session.started_at > RING_TIMEOUT_S:
                    session.state = "expired"
                    log.debug("Doorbell expired: session=%s camera=%s", session.id, session.camera)
                    await self._push_event("doorbell.expired", session)
                    await self._stop_audio(session)
                if session.state in ("dismissed", "expired", "answered"):
                    if now - session.started_at > SESSION_TTL_S:
                        expired.append(session.id)
            for sid in expired:
                self._sessions.pop(sid, None)
