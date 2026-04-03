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
