# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Doorbell-to-X — routes doorbell events to wherever the user is.

  At desk    → OzmaConsole overlay + chime (WebSocket broadcast)
  In lounge  → any other connected browser client receives the same broadcast
  On phone   → KDE Connect ping notification (instant, no app required beyond KDE Connect)
  Not home   → all of the above fire simultaneously; user gets whichever applies

When Frigate's facial recognition identifies a visitor (sub_label), the person's
name enriches the session and appears in the overlay.  This also works as a
general event trigger: e.g. "when Matt is recognised at the front door, activate
the matt-workstation scenario."

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
import json
import logging
import os
import socket
import time
import uuid
from dataclasses import dataclass, field
from typing import Any
from urllib.parse import urlparse

log = logging.getLogger("ozma.doorbell")

RING_TIMEOUT_S = 30      # auto-expire if not answered/dismissed
SESSION_TTL_S  = 300     # clean up old sessions after 5 minutes

# Audio constants
_CAM_SAMPLE_RATE  = 48_000   # inbound (camera → headset): full quality
_CAM_CHANNELS     = 2
_MIC_SAMPLE_RATE  = 8_000    # outbound (mic → camera backchannel): telephony
_MIC_CHANNELS     = 1
_VBAN_NODE_PORT   = 6980     # node VBAN receiver port (existing default)
_SAMPLES_PER_FRAME = 256     # matches vban.py DEFAULT_SAMPLES_PER_FRAME


# ── Audio bridges ──────────────────────────────────────────────────────────────

class CameraAudioBridge:
    """
    Pulls audio from a camera RTSP stream via ffmpeg and forwards it as
    VBAN UDP frames to the active node's headset output.

    Inbound path:
      ffmpeg → raw PCM (48kHz stereo) → Python VBAN packer → UDP → node:6980
      Node's existing VBANReceiver routes it through PipeWire to the headset.
    """

    def __init__(
        self,
        rtsp_url: str,
        node_host: str,
        node_port: int = _VBAN_NODE_PORT,
        stream_name: str = "doorbell-rx",
    ) -> None:
        self._rtsp_url = rtsp_url
        self._node_host = node_host
        self._node_port = node_port
        self._stream_name = stream_name
        self._proc: asyncio.subprocess.Process | None = None
        self._task: asyncio.Task | None = None

    async def start(self) -> None:
        self._task = asyncio.create_task(self._run(), name="doorbell-cam-bridge")

    async def stop(self) -> None:
        if self._proc and self._proc.returncode is None:
            self._proc.terminate()
            try:
                await asyncio.wait_for(self._proc.wait(), timeout=3.0)
            except asyncio.TimeoutError:
                self._proc.kill()
        if self._task:
            self._task.cancel()
            try:
                await self._task
            except asyncio.CancelledError:
                pass

    async def _run(self) -> None:
        from vban import encode_header
        cmd = [
            "ffmpeg", "-loglevel", "error",
            "-rtsp_transport", "tcp",
            "-i", self._rtsp_url,
            "-vn",
            "-acodec", "pcm_s16le",
            "-ar", str(_CAM_SAMPLE_RATE),
            "-ac", str(_CAM_CHANNELS),
            "-f", "s16le", "-",
        ]
        try:
            self._proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.DEVNULL,
            )
        except FileNotFoundError:
            log.error("ffmpeg not found — doorbell inbound audio unavailable")
            return

        sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        frame_bytes = _SAMPLES_PER_FRAME * _CAM_CHANNELS * 2
        counter = 0
        log.info("Doorbell inbound bridge: %s → VBAN → %s:%d",
                 self._rtsp_url, self._node_host, self._node_port)
        try:
            while True:
                assert self._proc.stdout is not None
                chunk = await self._proc.stdout.read(frame_bytes)
                if not chunk:
                    break
                if len(chunk) < frame_bytes:
                    chunk = chunk + b"\x00" * (frame_bytes - len(chunk))
                header = encode_header(
                    self._stream_name, counter,
                    _CAM_SAMPLE_RATE, _CAM_CHANNELS, _SAMPLES_PER_FRAME,
                )
                sock.sendto(header + chunk, (self._node_host, self._node_port))
                counter = (counter + 1) & 0xFFFF_FFFF
        except Exception as exc:
            log.debug("Doorbell inbound bridge ended: %s", exc)
        finally:
            sock.close()


class BackchannelSink:
    """
    Accepts raw PCM chunks (s16le, 8 kHz mono) from the dashboard mic
    WebSocket and forwards them to the camera RTSP backchannel via ffmpeg.

    Outbound path:
      Dashboard mic → Web Audio API → PCM/WebSocket → controller
      → BackchannelSink.write() → queue → ffmpeg stdin
      → G.711 µ-law RTSP ANNOUNCE/RECORD → camera speaker

    write() is non-blocking — overflow drops frames rather than blocking.
    """

    def __init__(self, backchannel_url: str) -> None:
        self._url = backchannel_url
        self._proc: asyncio.subprocess.Process | None = None
        self._task: asyncio.Task | None = None
        self._queue: asyncio.Queue[bytes] = asyncio.Queue(maxsize=64)

    async def start(self) -> None:
        cmd = [
            "ffmpeg", "-loglevel", "error",
            "-f", "s16le",
            "-ar", str(_MIC_SAMPLE_RATE),
            "-ac", str(_MIC_CHANNELS),
            "-i", "pipe:0",
            "-acodec", "pcm_mulaw",    # G.711 µ-law — widely accepted by cameras
            "-ar", "8000",
            "-ac", "1",
            "-f", "rtsp",
            "-rtsp_transport", "tcp",
            self._url,
        ]
        try:
            self._proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdin=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.DEVNULL,
            )
        except FileNotFoundError:
            log.error("ffmpeg not found — doorbell backchannel unavailable")
            return
        self._task = asyncio.create_task(self._feed(), name="doorbell-backchannel")
        log.info("Doorbell backchannel sink started → %s", self._url)

    async def _feed(self) -> None:
        try:
            while self._proc and self._proc.returncode is None:
                chunk = await self._queue.get()
                if not chunk:
                    break
                if self._proc.stdin:
                    self._proc.stdin.write(chunk)
                    await self._proc.stdin.drain()
        except Exception as exc:
            log.debug("Doorbell backchannel feed ended: %s", exc)

    def write(self, pcm_chunk: bytes) -> None:
        """Called by the WebSocket handler — non-blocking."""
        try:
            self._queue.put_nowait(pcm_chunk)
        except asyncio.QueueFull:
            pass  # drop if downstream can't keep up

    async def stop(self) -> None:
        try:
            self._queue.put_nowait(b"")  # sentinel
        except asyncio.QueueFull:
            pass
        if self._proc and self._proc.returncode is None:
            if self._proc.stdin:
                try:
                    self._proc.stdin.close()
                except Exception:
                    pass
            try:
                await asyncio.wait_for(self._proc.wait(), timeout=3.0)
            except asyncio.TimeoutError:
                self._proc.kill()
        if self._task:
            self._task.cancel()
            try:
                await self._task
            except asyncio.CancelledError:
                pass


@dataclass
class DoorbellSession:
    id: str
    camera: str
    frigate_url: str
    started_at: float
    state: str = "ringing"           # ringing | answered | dismissed | expired
    active_node_id: str | None = None
    snapshot_url: str = ""           # Frigate latest snapshot URL
    person: str = ""                 # Recognised person name (Frigate sub_label), if any
    _audio_proc: Any = field(default=None, repr=False)
    _cam_bridge: Any = field(default=None, repr=False)   # CameraAudioBridge | None
    _backchannel: Any = field(default=None, repr=False)  # BackchannelSink | None

    def to_dict(self) -> dict:
        d = {
            "id": self.id,
            "camera": self.camera,
            "started_at": self.started_at,
            "state": self.state,
            "active_node_id": self.active_node_id,
            "snapshot_url": f"/api/v1/doorbell/{self.id}/snapshot",
            "age_s": round(time.time() - self.started_at, 1),
        }
        if self.person:
            d["person"] = self.person
        return d


class DoorbellManager:
    """Manages doorbell sessions and routes events to wherever the user is.

    Doorbell-to-X delivery:
      - All connected WebSocket clients receive doorbell.ringing (covers desk + lounge)
      - KDE Connect ping fires to all paired phones (covers not-at-desk / not-home)
      - NotificationManager.on_event fires webhooks / Slack / Discord if configured
      Presence awareness is opportunistic: all channels fire simultaneously, so the
      user is reached on whichever applies.  Deduplication is the client's job.
    """

    def __init__(
        self,
        state: Any,
        frigate_url: str = "http://localhost:5000",
        kdeconnect: Any = None,
        notifier: Any = None,
    ) -> None:
        self._state = state
        self._frigate_url = frigate_url
        self._kdeconnect = kdeconnect
        self._notifier = notifier
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
        await self._notify_user(session)
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

    def enrich_person(self, camera: str, person: str) -> None:
        """Update ringing sessions on this camera with the recognised person's name.

        Called when Frigate fires a person_recognized event near-simultaneously with
        a doorbell button press.  The name enriches the session and is pushed to
        WebSocket clients via a doorbell.person_identified event.
        """
        for session in self._sessions.values():
            if session.camera == camera and session.state == "ringing" and not session.person:
                session.person = person
                log.info("Doorbell person identified: session=%s person=%s", session.id, person)
                asyncio.create_task(
                    self._push_event("doorbell.person_identified", session),
                    name=f"doorbell-person-{session.id}",
                )

    # ── Internal ────────────────────────────────────────────────────────────

    async def _push_event(self, event_type: str, session: DoorbellSession) -> None:
        await self._state.events.put({
            "type": event_type,
            **session.to_dict(),
        })

    async def _notify_user(self, session: DoorbellSession) -> None:
        """Push doorbell notification to the user's phone (doorbell-to-X).

        All channels fire simultaneously — the user is reached on whichever applies:
          - WebSocket broadcast (OzmaConsole overlay) reaches desk and lounge clients
          - KDE Connect ping reaches the phone when away from desk
        Both fire unconditionally; clients / devices handle deduplication/dismissal.
        """
        if session.person:
            text = f"{session.person} at your door ({session.camera})"
        else:
            text = f"Someone at your door ({session.camera})"

        # KDE Connect: ping all paired phones
        if self._kdeconnect:
            for device in self._kdeconnect._devices.values():
                if device.connected:
                    try:
                        await self._kdeconnect.ping(device.id, message=text)
                    except Exception as exc:
                        log.debug("KDE Connect ping failed for %s: %s", device.id, exc)

        # Webhook / Slack / Discord notifications
        if self._notifier:
            try:
                await self._notifier.on_event("doorbell.ringing", {
                    "title": "Doorbell",
                    "message": text,
                    "camera": session.camera,
                    "person": session.person,
                    "session_id": session.id,
                    "snapshot_url": session.to_dict()["snapshot_url"],
                })
            except Exception as exc:
                log.debug("Notifier send failed: %s", exc)

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
            await self._expire_loop_once()

    async def _expire_loop_once(self) -> None:
        """Single expiry sweep — separated for testability."""
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
