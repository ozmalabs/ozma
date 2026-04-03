# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Mobile phone camera ingestion via WHIP (WebRTC HTTP Ingest Protocol).

Phones stream H.264/Opus via WebRTC to the controller.  aiortc handles the
signalling (SDP offer/answer, ICE) and receives the RTP media.  ffmpeg writes
HLS segments to disk so the stream appears alongside other cameras in the
dashboard.  An optional RTMP relay pushes the same stream to an external
destination (YouTube Live, Twitch, OBS, etc.) without requiring RTMP support
on the mobile device.

Architecture:
  Phone → WHIP POST (SDP) → controller → aiortc PC → VideoStreamTrack
                                                    ↓
                                             ffmpeg (stdin pipe)
                                                    ↓
                               HLS segments  +  optional RTMP relay
"""

from __future__ import annotations

import asyncio
import logging
import time
import uuid
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.mobile_camera")

# ── Session dataclass ────────────────────────────────────────────────────────

@dataclass
class MobileSession:
    session_id: str
    camera_id: str          # CameraSource id registered in camera_manager
    name: str
    created_at: float
    peer_connection: Any    # aiortc RTCPeerConnection
    relay_rtmp_url: str = ""
    ffmpeg_proc: Any = None # asyncio subprocess
    bytes_received: int = 0
    last_keyframe_at: float = 0.0
    _video_track: Any = None
    _audio_track: Any = None
    _pipe_writer: asyncio.StreamWriter | None = None

    def to_dict(self) -> dict:
        return {
            "session_id": self.session_id,
            "camera_id": self.camera_id,
            "name": self.name,
            "created_at": self.created_at,
            "relay_rtmp_url": self.relay_rtmp_url,
            "bytes_received": self.bytes_received,
            "status": "active" if self.ffmpeg_proc and self.ffmpeg_proc.returncode is None else "connecting",
        }


# ── Manager ──────────────────────────────────────────────────────────────────

class MobileCameraManager:
    """
    Receive WHIP ingest from mobile phones, convert to HLS, register as
    CameraSource objects in the shared CameraManager.

    Requires:
      pip install aiortc aioice pyee av
    """

    def __init__(self, camera_mgr: Any = None, hls_dir: Path | None = None) -> None:
        self._camera_mgr = camera_mgr
        self._hls_dir = hls_dir or (Path(__file__).parent / "static" / "cameras")
        self._sessions: dict[str, MobileSession] = {}

    # ── WHIP protocol handlers ────────────────────────────────────────────────

    async def start_session(
        self,
        offer_sdp: str,
        name: str = "Mobile Camera",
        relay_rtmp: str = "",
    ) -> tuple[str, str]:
        """
        Process a WHIP POST (SDP offer).

        Returns (answer_sdp, session_id).
        Registers a CameraSource of type "mobile" with camera_manager.
        Spawns ffmpeg writing HLS segments.
        """
        try:
            from aiortc import RTCPeerConnection, RTCSessionDescription
        except ImportError:
            raise RuntimeError(
                "aiortc is required for mobile camera ingest. "
                "Install with: pip install aiortc"
            )

        session_id = str(uuid.uuid4()).replace("-", "")[:16]
        camera_id = f"mobile-{session_id[:8]}"

        pc = RTCPeerConnection()

        session = MobileSession(
            session_id=session_id,
            camera_id=camera_id,
            name=name,
            created_at=time.time(),
            peer_connection=pc,
            relay_rtmp_url=relay_rtmp,
        )
        self._sessions[session_id] = session

        # Register camera source now (before ICE completes) so it appears
        # in the dashboard immediately; active=False until ffmpeg starts.
        if self._camera_mgr:
            self._camera_mgr.add_camera({
                "id": camera_id,
                "name": name,
                "type": "mobile",
                "path": session_id,      # path stores the session_id
                "width": 1280,
                "height": 720,
                "fps": 30,
                "tags": ["mobile"],
            })
            # Auto-acknowledge privacy for phone cameras — the phone operator
            # is actively choosing to stream; no separate acknowledgement needed.
            cam = self._camera_mgr.get_camera(camera_id)
            if cam:
                cam.privacy.acknowledged = True
                cam.privacy.level = "network"

        # ICE candidate handler — called for host candidates found locally
        @pc.on("icecandidate")
        def _on_ice(candidate: Any) -> None:
            # With aiortc we gather all candidates during answer creation;
            # trickle candidates from the remote side arrive via PATCH.
            pass

        @pc.on("track")
        def _on_track(track: Any) -> None:
            log.debug("Mobile session %s: received %s track", session_id, track.kind)
            if track.kind == "video":
                session._video_track = track
            elif track.kind == "audio":
                session._audio_track = track
            # Schedule ffmpeg startup once we have at least the video track
            asyncio.create_task(
                self._ensure_ffmpeg(session),
                name=f"mobile-ffmpeg-{session_id[:8]}",
            )

        # Set remote description (the phone's offer)
        await pc.setRemoteDescription(RTCSessionDescription(sdp=offer_sdp, type="offer"))

        # Create answer
        answer = await pc.createAnswer()
        await pc.setLocalDescription(answer)

        # Wait for ICE gathering to complete (aiortc is synchronous on this)
        if hasattr(pc, "iceGatheringState"):
            deadline = time.time() + 5.0
            while pc.iceGatheringState != "complete" and time.time() < deadline:
                await asyncio.sleep(0.05)

        answer_sdp: str = pc.localDescription.sdp
        log.info("Mobile WHIP session started: %s (name=%s)", session_id, name)
        return answer_sdp, session_id

    async def trickle(self, session_id: str, candidate_sdpfrag: str) -> None:
        """Add a trickle ICE candidate (from mobile PATCH request)."""
        session = self._sessions.get(session_id)
        if not session:
            raise KeyError(f"Session not found: {session_id}")
        try:
            from aiortc.sdp import candidate_from_sdp
            # candidate_sdpfrag is an SDP fragment like "a=candidate:..."
            for line in candidate_sdpfrag.splitlines():
                line = line.strip()
                if line.startswith("a=candidate:") or line.startswith("candidate:"):
                    sdp_line = line.removeprefix("a=")
                    candidate = candidate_from_sdp(sdp_line.removeprefix("candidate:"))
                    if hasattr(session.peer_connection, "addIceCandidate"):
                        await session.peer_connection.addIceCandidate(candidate)
        except Exception as exc:
            log.warning("Trickle ICE failed for session %s: %s", session_id, exc)

    async def end_session(self, session_id: str) -> None:
        """Teardown: close PC, kill ffmpeg, deregister CameraSource."""
        session = self._sessions.pop(session_id, None)
        if not session:
            return
        # Kill ffmpeg
        if session.ffmpeg_proc and session.ffmpeg_proc.returncode is None:
            try:
                session.ffmpeg_proc.terminate()
                await asyncio.wait_for(session.ffmpeg_proc.wait(), timeout=5.0)
            except Exception:
                try:
                    session.ffmpeg_proc.kill()
                except Exception:
                    pass
        # Close pipe writer
        if session._pipe_writer:
            try:
                session._pipe_writer.close()
            except Exception:
                pass
        # Close WebRTC peer connection
        try:
            await session.peer_connection.close()
        except Exception:
            pass
        # Deregister camera source
        if self._camera_mgr:
            self._camera_mgr.remove_camera(session.camera_id)
        log.info("Mobile WHIP session ended: %s", session_id)

    async def get_session(self, session_id: str) -> MobileSession | None:
        return self._sessions.get(session_id)

    async def list_sessions(self) -> list[dict]:
        return [s.to_dict() for s in self._sessions.values()]

    # ── ffmpeg pipeline ───────────────────────────────────────────────────────

    async def _ensure_ffmpeg(self, session: MobileSession) -> None:
        """
        Start ffmpeg as soon as a video track is available.

        aiortc provides a VideoStreamTrack whose recv() method yields
        av.VideoFrame objects.  We use aiortc's MediaRecorder for simplicity
        when producing HLS, falling back to a raw pipe approach if needed.
        """
        # Wait up to 10 seconds for the video track to arrive
        deadline = time.time() + 10.0
        while session._video_track is None and time.time() < deadline:
            await asyncio.sleep(0.1)
        if session._video_track is None:
            log.warning("Mobile session %s: no video track after 10s — aborting ffmpeg",
                        session.session_id)
            return

        out_dir = self._hls_dir / session.camera_id
        out_dir.mkdir(parents=True, exist_ok=True)
        manifest = str(out_dir / "stream.m3u8")
        seg_pattern = str(out_dir / "seg_%05d.ts")

        # Use aiortc MediaRecorder to pipe directly to ffmpeg HLS output.
        # We construct an ffmpeg command that reads from a pipe (-f rawvideo)
        # and writes HLS + optional RTMP.
        try:
            from aiortc.contrib.media import MediaRecorder
        except ImportError:
            log.warning("aiortc.contrib.media not available — using pipe approach")
            await self._ffmpeg_pipe_approach(session, out_dir, manifest, seg_pattern)
            return

        # Build ffmpeg args for HLS output
        # MediaRecorder wraps ffmpeg; we supply output options.
        options = {
            "vcodec": "copy",        # Phone sends H.264 — copy without re-encoding
            "acodec": "aac",         # Re-encode Opus → AAC for HLS compatibility
            "f": "hls",
            "hls_time": "2",
            "hls_list_size": "5",
            "hls_flags": "delete_segments+independent_segments",
            "hls_segment_filename": seg_pattern,
        }

        # If RTMP relay is requested, we can't do both outputs from MediaRecorder
        # directly.  Use the pipe approach so ffmpeg handles both.
        if session.relay_rtmp_url:
            await self._ffmpeg_pipe_approach(session, out_dir, manifest, seg_pattern)
            return

        try:
            recorder = MediaRecorder(manifest, options=options)
            if session._video_track:
                recorder.addTrack(session._video_track)
            if session._audio_track:
                recorder.addTrack(session._audio_track)
            await recorder.start()
            # Mark the camera active
            if self._camera_mgr:
                cam = self._camera_mgr.get_camera(session.camera_id)
                if cam:
                    cam.active = True
            log.info("Mobile session %s: MediaRecorder started → %s",
                     session.session_id, manifest)
        except Exception as exc:
            log.warning("Mobile session %s: MediaRecorder failed (%s) — trying pipe",
                        session.session_id, exc)
            await self._ffmpeg_pipe_approach(session, out_dir, manifest, seg_pattern)

    async def _ffmpeg_pipe_approach(
        self,
        session: MobileSession,
        out_dir: Path,
        manifest: str,
        seg_pattern: str,
    ) -> None:
        """
        Fallback: spawn ffmpeg reading a FIFO/pipe and outputting HLS.

        For RTMP relay: add a second -f flv output to the same ffmpeg process.
        """
        # Use a named pipe so both HLS and optional RTMP can be teed from
        # a single ffmpeg input without re-reading the track twice.
        import os
        fifo_path = str(out_dir / "input.ts")
        try:
            os.mkfifo(fifo_path)
        except FileExistsError:
            pass
        except OSError:
            fifo_path = None  # fall through to /dev/stdin approach

        cmd = [
            "ffmpeg", "-hide_banner", "-loglevel", "warning", "-y",
            "-re",
            "-i", fifo_path or "pipe:0",
            # HLS output
            "-c:v", "copy",
            "-c:a", "aac",
            "-f", "hls",
            "-hls_time", "2",
            "-hls_list_size", "5",
            "-hls_flags", "delete_segments+independent_segments",
            "-hls_segment_filename", seg_pattern,
            manifest,
        ]

        # Add RTMP relay as a second output
        if session.relay_rtmp_url:
            cmd.extend([
                "-c:v", "copy",
                "-c:a", "aac",
                "-f", "flv",
                session.relay_rtmp_url,
            ])

        try:
            use_stdin = fifo_path is None
            proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdin=asyncio.subprocess.PIPE if use_stdin else None,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.PIPE,
            )
            session.ffmpeg_proc = proc
            asyncio.create_task(
                _drain_stderr(proc, f"mobile-{session.session_id[:8]}"),
                name=f"mobile-stderr-{session.session_id[:8]}",
            )

            if use_stdin and session._video_track:
                # Feed frames to ffmpeg's stdin using av
                asyncio.create_task(
                    self._feed_frames(session, proc),
                    name=f"mobile-feed-{session.session_id[:8]}",
                )

            if self._camera_mgr:
                cam = self._camera_mgr.get_camera(session.camera_id)
                if cam:
                    cam.active = True
                    cam.proc = proc

            log.info("Mobile session %s: ffmpeg started (pipe=%s, rtmp=%s)",
                     session.session_id, not use_stdin, bool(session.relay_rtmp_url))

        except Exception as exc:
            log.error("Mobile session %s: failed to start ffmpeg: %s",
                      session.session_id, exc)

    async def _feed_frames(self, session: MobileSession, proc: Any) -> None:
        """Read av.VideoFrame objects from the aiortc track, encode to MPEG-TS and write to ffmpeg stdin."""
        import av
        try:
            while True:
                try:
                    frame = await asyncio.wait_for(
                        session._video_track.recv(), timeout=10.0
                    )
                except asyncio.TimeoutError:
                    log.debug("Mobile session %s: frame timeout", session.session_id)
                    break
                except Exception as exc:
                    log.debug("Mobile session %s: track ended: %s", session.session_id, exc)
                    break

                # Write raw frame data to ffmpeg stdin
                if proc.stdin and not proc.stdin.is_closing():
                    try:
                        # Frame is an av.VideoFrame; get bytes from the plane
                        data = bytes(frame.planes[0])
                        session.bytes_received += len(data)
                        proc.stdin.write(data)
                        await proc.stdin.drain()
                    except (BrokenPipeError, ConnectionResetError):
                        break
        finally:
            if proc.stdin and not proc.stdin.is_closing():
                proc.stdin.close()


async def _drain_stderr(proc: Any, label: str) -> None:
    """Log ffmpeg stderr output at debug level."""
    if not proc.stderr:
        return
    try:
        async for line in proc.stderr:
            text = line.decode(errors="replace").rstrip()
            if text:
                log.debug("ffmpeg[%s]: %s", label, text)
    except Exception:
        pass


# ── Clip browser ─────────────────────────────────────────────────────────────

import json as _json
import os as _os
import stat as _stat


@dataclass
class ClipInfo:
    """Metadata for a recorded camera clip."""
    clip_id: str            # Unique identifier (basename without ext)
    camera_id: str
    timestamp: float        # Unix epoch of clip start
    duration_sec: float     # Duration in seconds (0 if unknown)
    size_bytes: int         # File size
    path: str               # Absolute path on disk
    thumbnail_url: str = "" # URL to JPEG thumbnail (if available)
    event_type: str = ""    # "motion" | "object" | "continuous" | ""

    def to_dict(self) -> dict:
        return {
            "clip_id":       self.clip_id,
            "camera_id":     self.camera_id,
            "timestamp":     self.timestamp,
            "duration_sec":  self.duration_sec,
            "size_bytes":    self.size_bytes,
            "thumbnail_url": self.thumbnail_url,
            "event_type":    self.event_type,
        }


class ClipBrowser:
    """
    Browse recorded clips for one or more cameras.

    Clips are assumed to be .mp4 (or .mkv) files under:
      {recordings_dir}/{camera_id}/{YYYY}/{MM}/{DD}/

    Falls back to a flat listing if subdirectory layout is absent.
    """

    def __init__(self, recordings_dir: Path) -> None:
        self._dir = recordings_dir

    def list_clips(
        self,
        camera_id: str,
        limit: int = 100,
        before: float | None = None,
        event_type: str | None = None,
    ) -> list[ClipInfo]:
        """
        Return recorded clips for camera_id, newest first.

        Args:
          camera_id:   Camera node ID
          limit:       Max clips to return
          before:      Only clips with timestamp < before (pagination)
          event_type:  Filter by event type tag
        """
        cam_dir = self._dir / camera_id
        if not cam_dir.exists():
            return []

        clips: list[ClipInfo] = []
        for p in sorted(cam_dir.rglob("*.mp4"), key=lambda f: f.stat().st_mtime, reverse=True):
            st = p.stat()
            ts = st.st_mtime
            if before and ts >= before:
                continue
            clip_id = p.stem
            ev_type = self._parse_event_type(p)
            if event_type and ev_type != event_type:
                continue
            clips.append(ClipInfo(
                clip_id=clip_id,
                camera_id=camera_id,
                timestamp=ts,
                duration_sec=self._probe_duration(p),
                size_bytes=st.st_size,
                path=str(p),
                thumbnail_url=self._thumbnail_url(p, camera_id),
                event_type=ev_type,
            ))
            if len(clips) >= limit:
                break
        return clips

    def get_clip(self, camera_id: str, clip_id: str) -> ClipInfo | None:
        """Find a specific clip by ID."""
        cam_dir = self._dir / camera_id
        if not cam_dir.exists():
            return None
        for p in cam_dir.rglob(f"{clip_id}.mp4"):
            st = p.stat()
            return ClipInfo(
                clip_id=clip_id,
                camera_id=camera_id,
                timestamp=st.st_mtime,
                duration_sec=self._probe_duration(p),
                size_bytes=st.st_size,
                path=str(p),
                thumbnail_url=self._thumbnail_url(p, camera_id),
                event_type=self._parse_event_type(p),
            )
        return None

    @staticmethod
    def _probe_duration(p: Path) -> float:
        """Try to read duration from a sidecar .json or return 0."""
        sidecar = p.with_suffix(".json")
        if sidecar.exists():
            try:
                d = _json.loads(sidecar.read_text())
                return float(d.get("duration_sec", 0))
            except Exception:
                pass
        return 0.0

    @staticmethod
    def _parse_event_type(p: Path) -> str:
        """Infer event type from filename suffix convention e.g. foo_motion.mp4."""
        stem = p.stem.lower()
        for ev in ("motion", "object", "continuous", "doorbell"):
            if stem.endswith(f"_{ev}"):
                return ev
        return ""

    @staticmethod
    def _thumbnail_url(p: Path, camera_id: str) -> str:
        """Return relative URL for thumbnail if a .jpg sidecar exists."""
        thumb = p.with_suffix(".jpg")
        if thumb.exists():
            return f"/api/v1/cameras/{camera_id}/clips/{p.stem}/thumbnail"
        return ""


# ── Guest token helper ────────────────────────────────────────────────────────

@dataclass
class GuestCameraToken:
    """A scoped token that grants camera-view-only access."""
    token: str
    camera_ids: list[str]   # Empty = all cameras
    expires_at: float       # Unix epoch
    label: str = ""         # Human-readable label (e.g. "Grandma's phone")

    def to_dict(self) -> dict:
        return {
            "token":      self.token,
            "camera_ids": self.camera_ids,
            "expires_at": self.expires_at,
            "label":      self.label,
        }

    def is_expired(self) -> bool:
        return time.time() > self.expires_at

    def allows_camera(self, camera_id: str) -> bool:
        if not self.camera_ids:
            return True  # All cameras
        return camera_id in self.camera_ids


class GuestTokenManager:
    """
    Manages camera-only guest tokens for the consumer gifting use case.

    A technical friend sets up the camera system and calls
    create_guest_token() to get a token for a non-technical family member.
    The token only permits:
      - Listing cameras
      - Viewing live streams
      - Browsing recorded clips

    It cannot access HID, audio routing, nodes, or any sensitive config.
    """

    def __init__(self, data_dir: Path, mesh_ca: Any = None) -> None:
        self._data_dir = data_dir
        self._mesh_ca = mesh_ca  # MeshCA for signing (optional; plain tokens if None)
        self._tokens: dict[str, GuestCameraToken] = {}  # token → GuestCameraToken
        self._load()

    def create_token(
        self,
        label: str = "",
        camera_ids: list[str] | None = None,
        ttl_days: int = 365,
    ) -> GuestCameraToken:
        """
        Create a new guest token with camera_view scope.

        Args:
          label:      Human label (e.g. "Mum's iPhone")
          camera_ids: Camera node IDs this token may access (empty = all)
          ttl_days:   Token lifetime in days (default 1 year)
        """
        import secrets as _sec
        raw = _sec.token_hex(32)
        expires_at = time.time() + ttl_days * 86400
        gt = GuestCameraToken(
            token=raw,
            camera_ids=list(camera_ids or []),
            expires_at=expires_at,
            label=label,
        )
        self._tokens[raw] = gt
        self._save()
        log.info("Guest token created: label=%r cameras=%s ttl_days=%d",
                 label, camera_ids or "all", ttl_days)
        return gt

    def validate(self, token: str) -> GuestCameraToken | None:
        """Return the GuestCameraToken if valid and unexpired, else None."""
        gt = self._tokens.get(token)
        if gt is None or gt.is_expired():
            return None
        return gt

    def revoke(self, token: str) -> bool:
        if token in self._tokens:
            del self._tokens[token]
            self._save()
            return True
        return False

    def list_tokens(self) -> list[dict]:
        now = time.time()
        return [
            {**gt.to_dict(), "expired": gt.is_expired()}
            for gt in self._tokens.values()
            if now - gt.expires_at < 86400 * 30  # prune tokens expired >30d ago
        ]

    def _save(self) -> None:
        self._data_dir.mkdir(parents=True, exist_ok=True)
        p = self._data_dir / "guest_tokens.json"
        tmp = p.with_suffix(".tmp")
        tmp.write_text(_json.dumps(
            {tok: gt.to_dict() for tok, gt in self._tokens.items()}, indent=2
        ))
        tmp.chmod(0o600)
        tmp.rename(p)

    def _load(self) -> None:
        p = self._data_dir / "guest_tokens.json"
        if not p.exists():
            return
        try:
            data = _json.loads(p.read_text())
            for tok, d in data.items():
                self._tokens[tok] = GuestCameraToken(
                    token=d["token"],
                    camera_ids=d.get("camera_ids", []),
                    expires_at=d.get("expires_at", 0.0),
                    label=d.get("label", ""),
                )
        except Exception:
            log.exception("Failed to load guest tokens")


# ── Motion push notifications ─────────────────────────────────────────────────

@dataclass
class PushWebhook:
    """A webhook URL registered to receive motion/object alerts."""
    webhook_id: str
    url: str
    camera_ids: list[str]   # Empty = all cameras
    events: list[str]       # ["motion", "object", "doorbell"] or empty = all
    label: str = ""
    created_at: float = field(default_factory=time.time)
    last_fired_at: float = 0.0
    failures: int = 0

    def to_dict(self) -> dict:
        return {
            "webhook_id":   self.webhook_id,
            "url":          self.url,
            "camera_ids":   self.camera_ids,
            "events":       self.events,
            "label":        self.label,
            "created_at":   self.created_at,
            "last_fired_at": self.last_fired_at,
            "failures":     self.failures,
        }


class MotionPushManager:
    """
    Sends POST webhook notifications when cameras detect motion or objects.

    Integrates with the controller event queue — Frigate and auto_configure
    push events like frigate.motion_started or device_discovered. This
    manager watches for those events and forwards them to registered webhooks.

    Webhook payload (JSON POST body):
      {
        "event":       "motion" | "object" | "doorbell",
        "camera_id":   "cam01._ozma._udp.local.",
        "camera_name": "Front Door",
        "label":       "person",          // object label if applicable
        "confidence":  0.87,              // detection confidence
        "snapshot_url": "https://...",    // snapshot URL if available
        "ts":          1700000000.0,
      }
    """

    _MAX_FAILURES = 5       # Disable webhook after this many consecutive failures

    def __init__(self, data_dir: Path) -> None:
        self._data_dir = data_dir
        self._webhooks: dict[str, PushWebhook] = {}  # webhook_id → webhook
        self._load()

    def register(
        self,
        url: str,
        camera_ids: list[str] | None = None,
        events: list[str] | None = None,
        label: str = "",
    ) -> PushWebhook:
        import secrets as _sec
        wh = PushWebhook(
            webhook_id=_sec.token_hex(8),
            url=url,
            camera_ids=list(camera_ids or []),
            events=list(events or []),
            label=label,
        )
        self._webhooks[wh.webhook_id] = wh
        self._save()
        log.info("Push webhook registered: %s → %s", wh.webhook_id, url[:60])
        return wh

    def unregister(self, webhook_id: str) -> bool:
        if webhook_id in self._webhooks:
            del self._webhooks[webhook_id]
            self._save()
            return True
        return False

    def list_webhooks(self) -> list[dict]:
        return [wh.to_dict() for wh in self._webhooks.values()]

    def get_webhook(self, webhook_id: str) -> PushWebhook | None:
        return self._webhooks.get(webhook_id)

    async def notify(
        self,
        camera_id: str,
        event_type: str,
        camera_name: str = "",
        label: str = "",
        confidence: float = 0.0,
        snapshot_url: str = "",
    ) -> int:
        """
        Fire notification to all matching webhooks.

        Returns count of successful deliveries.
        """
        payload = _json.dumps({
            "event":        event_type,
            "camera_id":    camera_id,
            "camera_name":  camera_name,
            "label":        label,
            "confidence":   confidence,
            "snapshot_url": snapshot_url,
            "ts":           time.time(),
        }).encode()

        ok_count = 0
        for wh in list(self._webhooks.values()):
            if wh.failures >= self._MAX_FAILURES:
                continue
            if wh.camera_ids and camera_id not in wh.camera_ids:
                continue
            if wh.events and event_type not in wh.events:
                continue
            delivered = await self._fire(wh, payload)
            if delivered:
                ok_count += 1
                wh.failures = 0
            else:
                wh.failures += 1
            wh.last_fired_at = time.time()
        if self._webhooks:
            self._save()
        return ok_count

    async def _fire(self, wh: PushWebhook, payload: bytes) -> bool:
        """POST the payload to the webhook URL. Returns True on success."""
        import urllib.request as _req
        try:
            loop = asyncio.get_running_loop()
            request = _req.Request(
                wh.url,
                data=payload,
                headers={"Content-Type": "application/json"},
                method="POST",
            )
            await asyncio.wait_for(
                loop.run_in_executor(None, lambda: _req.urlopen(request, timeout=5)),
                timeout=6.0,
            )
            log.debug("Push webhook fired: %s → %s", wh.webhook_id, wh.url[:60])
            return True
        except Exception as e:
            log.debug("Push webhook failed: %s — %s", wh.webhook_id, e)
            return False

    def _save(self) -> None:
        self._data_dir.mkdir(parents=True, exist_ok=True)
        p = self._data_dir / "push_webhooks.json"
        tmp = p.with_suffix(".tmp")
        tmp.write_text(_json.dumps(
            {wid: wh.to_dict() for wid, wh in self._webhooks.items()}, indent=2
        ))
        tmp.chmod(0o600)
        tmp.rename(p)

    def _load(self) -> None:
        p = self._data_dir / "push_webhooks.json"
        if not p.exists():
            return
        try:
            data = _json.loads(p.read_text())
            for wid, d in data.items():
                self._webhooks[wid] = PushWebhook(
                    webhook_id=d["webhook_id"],
                    url=d["url"],
                    camera_ids=d.get("camera_ids", []),
                    events=d.get("events", []),
                    label=d.get("label", ""),
                    created_at=d.get("created_at", 0.0),
                    last_fired_at=d.get("last_fired_at", 0.0),
                    failures=d.get("failures", 0),
                )
        except Exception:
            log.exception("Failed to load push webhooks")
