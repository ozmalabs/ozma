# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Per-seat WebRTC streaming for multi-seat.

Each seat exposes a WebRTC signaling endpoint on its HTTP API so that
browsers, thin clients, and phones can get sub-100ms video+audio from
any seat. The controller doesn't need to be in the loop — clients
connect directly to the seat's api_port.

Video: captures from the seat's display region (x11grab snapshot or
ffmpeg pipe) and delivers frames via an aiortc video track.

Audio: captures from the seat's PipeWire sink monitor (via ffmpeg
-f pulse) and delivers PCM frames via an aiortc audio track.

aiortc is an optional dependency. If not installed, the WebRTC endpoints
return 503 and MJPEG/HLS remains the only streaming path.

Usage:
    handler = SeatWebRTCHandler(seat)
    # Add routes to the seat's aiohttp app
    handler.add_routes(app)
    # On shutdown
    await handler.cleanup()
"""

from __future__ import annotations

import asyncio
import fractions
import logging
import os
import queue
import subprocess
import threading
import time
from typing import TYPE_CHECKING, Any

from aiohttp import web

if TYPE_CHECKING:
    from .seat import Seat

log = logging.getLogger("ozma.agent.multiseat.webrtc")

# Sentinel: set to True once we verify aiortc is importable.
_AIORTC_AVAILABLE: bool | None = None


def _check_aiortc() -> bool:
    """Check once whether aiortc is available."""
    global _AIORTC_AVAILABLE
    if _AIORTC_AVAILABLE is not None:
        return _AIORTC_AVAILABLE
    try:
        # Test av import in a subprocess to avoid DLL load crashes
        import subprocess, sys as _sys
        result = subprocess.run(
            [_sys.executable, "-c", "import av; import aiortc; import numpy"],
            capture_output=True, timeout=10,
        )
        if result.returncode != 0:
            raise ImportError(f"av/aiortc probe failed: {result.stderr.decode()[:200]}")
        import aiortc  # noqa: F401
        import av  # noqa: F401
        import numpy  # noqa: F401
        _AIORTC_AVAILABLE = True
    except Exception as e:
        _AIORTC_AVAILABLE = False
        log.info("WebRTC disabled (aiortc/av probe failed: %s) — MJPEG/HLS fallback active",
                 str(e)[:100])
    return _AIORTC_AVAILABLE


# ── Video track ──────────────────────────────────────────────────────────────

class SeatVideoTrack:
    """
    WebRTC video track that captures frames from the seat's display region.

    Uses ffmpeg x11grab to capture the seat's display region as raw frames,
    then wraps each in an av.VideoFrame for aiortc.

    Falls back to periodic JPEG snapshot decoding if raw capture fails.
    """

    kind = "video"

    def __init__(self, seat: Seat, fps: int = 30) -> None:
        self._seat = seat
        self._fps = fps
        self._interval = 1.0 / fps
        self._start = time.time()
        self._frame_count = 0
        self._capture_proc: subprocess.Popen | None = None
        self._capture_thread: threading.Thread | None = None
        self._frame_queue: queue.Queue = queue.Queue(maxsize=5)
        self._started = False
        self._stopped = False

    def _get_capture_region(self) -> tuple[int, int, int, int, str]:
        """Return (width, height, grab_x, grab_y, display_env) for this seat."""
        display_env = os.environ.get("DISPLAY", ":0")
        seat = self._seat
        if seat.display:
            return (seat.display.width, seat.display.height,
                    seat.display.x_offset, seat.display.y_offset,
                    display_env)
        return (seat.capture_width, seat.capture_height, 0, 0, display_env)

    def _start_capture(self) -> None:
        """Start ffmpeg raw frame capture in a background thread."""
        if self._started:
            return
        self._started = True

        width, height, grab_x, grab_y, display_env = self._get_capture_region()

        cmd = [
            "ffmpeg", "-hide_banner", "-loglevel", "error",
            "-f", "x11grab",
            "-framerate", str(self._fps),
            "-video_size", f"{width}x{height}",
            "-i", f"{display_env}+{grab_x},{grab_y}",
            "-f", "rawvideo",
            "-pix_fmt", "bgr24",
            "-",
        ]

        try:
            self._capture_proc = subprocess.Popen(
                cmd,
                stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL,
            )
            self._capture_thread = threading.Thread(
                target=self._read_frames,
                args=(width, height),
                daemon=True,
            )
            self._capture_thread.start()
            log.info("Seat %s: WebRTC video capture started (%dx%d@%dfps)",
                     self._seat.name, width, height, self._fps)
        except Exception as e:
            log.warning("Seat %s: WebRTC video capture failed: %s",
                        self._seat.name, e)
            self._capture_proc = None

    def _read_frames(self, width: int, height: int) -> None:
        """Read raw BGR24 frames from ffmpeg stdout."""
        frame_bytes = width * height * 3
        proc = self._capture_proc
        if not proc or not proc.stdout:
            return

        while not self._stopped and proc.poll() is None:
            data = proc.stdout.read(frame_bytes)
            if len(data) < frame_bytes:
                break
            try:
                self._frame_queue.put_nowait((data, width, height))
            except queue.Full:
                # Drop oldest frame
                try:
                    self._frame_queue.get_nowait()
                except queue.Empty:
                    pass
                self._frame_queue.put_nowait((data, width, height))

    async def recv(self) -> Any:
        """Deliver the next video frame to aiortc."""
        import numpy as np
        from av import VideoFrame

        if not self._started:
            self._start_capture()

        # Rate limiting
        pts = self._frame_count
        self._frame_count += 1
        target = self._start + pts * self._interval
        now = time.time()
        if target > now:
            await asyncio.sleep(target - now)

        # Get frame from capture thread
        frame_data = None
        for _ in range(50):  # wait up to 500ms
            try:
                frame_data = self._frame_queue.get_nowait()
                break
            except queue.Empty:
                await asyncio.sleep(0.01)

        width, height, _, _, _ = self._get_capture_region()

        if frame_data:
            raw, w, h = frame_data
            arr = np.frombuffer(raw, dtype=np.uint8).reshape(h, w, 3)
            frame = VideoFrame.from_ndarray(arr, format="bgr24")
        else:
            # Black frame fallback
            arr = np.zeros((height, width, 3), dtype=np.uint8)
            frame = VideoFrame.from_ndarray(arr, format="bgr24")

        frame.pts = pts
        frame.time_base = fractions.Fraction(1, self._fps)
        return frame

    def stop(self) -> None:
        """Stop capture."""
        self._stopped = True
        if self._capture_proc:
            try:
                self._capture_proc.terminate()
            except Exception:
                pass
            self._capture_proc = None


# ── Audio track ──────────────────────────────────────────────────────────────

class SeatAudioTrack:
    """
    WebRTC audio track from the seat's PipeWire/PulseAudio sink monitor.

    Captures audio via ffmpeg from the seat's virtual null sink monitor
    (e.g. "ozma-seat-0.monitor") and delivers PCM frames to aiortc.

    Reuses the pattern from softnode/webrtc_audio.py.
    """

    kind = "audio"

    def __init__(self, seat: Seat, sample_rate: int = 48000,
                 channels: int = 2) -> None:
        self._seat = seat
        self._rate = sample_rate
        self._channels = channels
        self._frame_size = 960  # 20ms at 48kHz
        self._queue: queue.Queue = queue.Queue(maxsize=50)
        self._proc: subprocess.Popen | None = None
        self._thread: threading.Thread | None = None
        self._started = False
        self._stopped = False
        self._pts = 0

    def _get_monitor_source(self) -> str:
        """Determine the PulseAudio monitor source for this seat."""
        if self._seat.audio_sink:
            return f"{self._seat.audio_sink}.monitor"
        return "default.monitor"

    def _start_capture(self) -> None:
        """Start ffmpeg audio capture from PulseAudio monitor."""
        if self._started:
            return
        self._started = True

        source = self._get_monitor_source()
        cmd = [
            "ffmpeg", "-hide_banner", "-loglevel", "error",
            "-f", "pulse",
            "-i", source,
            "-f", "s16le",
            "-ar", str(self._rate),
            "-ac", str(self._channels),
            "-",
        ]

        try:
            self._proc = subprocess.Popen(
                cmd,
                stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL,
            )
            self._thread = threading.Thread(
                target=self._read_loop, daemon=True,
            )
            self._thread.start()
            log.info("Seat %s: WebRTC audio capture started (%s @ %dHz %dch)",
                     self._seat.name, source, self._rate, self._channels)
        except Exception as e:
            log.warning("Seat %s: WebRTC audio capture failed: %s",
                        self._seat.name, e)
            self._proc = None

    def _read_loop(self) -> None:
        """Read PCM data from ffmpeg stdout."""
        bytes_per_frame = self._frame_size * self._channels * 2  # s16le
        proc = self._proc
        if not proc or not proc.stdout:
            return

        while not self._stopped and proc.poll() is None:
            data = proc.stdout.read(bytes_per_frame)
            if len(data) < bytes_per_frame:
                break
            try:
                self._queue.put_nowait(data)
            except queue.Full:
                try:
                    self._queue.get_nowait()
                except queue.Empty:
                    pass
                self._queue.put_nowait(data)

    async def recv(self) -> Any:
        """Deliver the next audio frame to aiortc."""
        import numpy as np
        from av import AudioFrame

        if not self._started:
            self._start_capture()

        # Poll the thread-safe queue
        data = None
        for _ in range(100):  # wait up to 1s
            try:
                data = self._queue.get_nowait()
                break
            except queue.Empty:
                await asyncio.sleep(0.01)

        if data is None:
            # Silence
            data = b"\x00" * (self._frame_size * self._channels * 2)

        arr = np.frombuffer(data, dtype=np.int16).reshape(1, -1)
        layout = "stereo" if self._channels == 2 else "mono"
        frame = AudioFrame.from_ndarray(arr, format="s16", layout=layout)
        frame.sample_rate = self._rate
        frame.pts = self._pts
        frame.time_base = fractions.Fraction(1, self._rate)
        self._pts += self._frame_size
        return frame

    def stop(self) -> None:
        """Stop capture."""
        self._stopped = True
        if self._proc:
            try:
                self._proc.terminate()
            except Exception:
                pass
            self._proc = None


# ── WebRTC handler ───────────────────────────────────────────────────────────

class SeatWebRTCHandler:
    """
    Manages WebRTC peer connections for a single seat.

    Handles SDP offer/answer exchange, creates video+audio tracks,
    and provides bitrate control.

    Usage:
        handler = SeatWebRTCHandler(seat)
        handler.add_routes(app)  # adds POST /webrtc/offer, /webrtc/bitrate
        await handler.cleanup()  # on shutdown
    """

    def __init__(self, seat: Seat) -> None:
        self._seat = seat
        self._pcs: list[Any] = []  # RTCPeerConnection instances
        self._video_tracks: list[SeatVideoTrack] = []
        self._audio_tracks: list[SeatAudioTrack] = []
        self._target_bitrate: int = 4_000_000  # 4 Mbps default

    @property
    def available(self) -> bool:
        """True if aiortc is installed and WebRTC is usable."""
        return _check_aiortc()

    @property
    def peer_count(self) -> int:
        """Number of active WebRTC peer connections."""
        return len(self._pcs)

    def add_routes(self, app: web.Application) -> None:
        """Register WebRTC endpoints on the seat's aiohttp app."""
        app.router.add_post("/webrtc/offer", self.handle_offer)
        app.router.add_post("/webrtc/bitrate", self.handle_bitrate)

    async def handle_offer(self, request: web.Request) -> web.Response:
        """
        Handle WebRTC offer from a browser/client.

        Receives an SDP offer, creates a peer connection with video+audio
        tracks from the seat's capture, and returns the SDP answer.
        """
        if not _check_aiortc():
            return web.json_response(
                {"error": "aiortc not installed — WebRTC unavailable"},
                status=503,
            )

        try:
            body = await request.json()
        except Exception:
            return web.json_response(
                {"error": "invalid JSON body"}, status=400,
            )

        sdp = body.get("sdp")
        sdp_type = body.get("type", "offer")
        if not sdp:
            return web.json_response(
                {"error": "missing 'sdp' in request body"}, status=400,
            )

        try:
            # Patch aiortc H.264 bitrate limits
            import aiortc.codecs.h264 as _h264mod
            _h264mod.DEFAULT_BITRATE = self._target_bitrate
            _h264mod.MAX_BITRATE = 50_000_000  # 50 Mbps ceiling for 4K60

            from aiortc import RTCPeerConnection, RTCSessionDescription
            from aiortc import MediaStreamTrack

            offer = RTCSessionDescription(sdp=sdp, type=sdp_type)
            pc = RTCPeerConnection()
            self._pcs.append(pc)

            @pc.on("connectionstatechange")
            async def on_state_change():
                if pc.connectionState in ("failed", "closed"):
                    await self._remove_pc(pc)

            # Video track from seat's display
            video_track = SeatVideoTrack(
                self._seat,
                fps=min(self._seat.capture_fps, 60),
            )
            # Wrap to be a proper MediaStreamTrack subclass
            wrapped_video = _WrappedVideoTrack(video_track)
            pc.addTrack(wrapped_video)
            self._video_tracks.append(video_track)

            # Audio track from seat's PipeWire sink
            audio_track = SeatAudioTrack(self._seat)
            wrapped_audio = _WrappedAudioTrack(audio_track)
            pc.addTrack(wrapped_audio)
            self._audio_tracks.append(audio_track)

            await pc.setRemoteDescription(offer)
            answer = await pc.createAnswer()
            await pc.setLocalDescription(answer)

            answer_sdp = pc.localDescription.sdp if pc.localDescription else None
            if answer_sdp:
                log.info("Seat %s: WebRTC session established "
                         "(peers=%d, bitrate=%dk)",
                         self._seat.name, len(self._pcs),
                         self._target_bitrate // 1000)
                return web.json_response({
                    "sdp": answer_sdp,
                    "type": "answer",
                })

            return web.json_response(
                {"error": "WebRTC negotiation failed"}, status=500,
            )

        except ImportError as e:
            log.warning("WebRTC import error: %s", e)
            return web.json_response(
                {"error": f"WebRTC dependency missing: {e}"}, status=503,
            )
        except Exception as e:
            log.error("Seat %s: WebRTC offer failed: %s",
                      self._seat.name, e, exc_info=True)
            return web.json_response(
                {"error": str(e)}, status=500,
            )

    async def handle_bitrate(self, request: web.Request) -> web.Response:
        """
        Adjust WebRTC video bitrate.

        Body: {"bitrate": 4000000}  (in bits per second)
        """
        try:
            body = await request.json()
        except Exception:
            return web.json_response(
                {"error": "invalid JSON body"}, status=400,
            )

        bitrate = body.get("bitrate", 4_000_000)
        bitrate = max(500_000, min(int(bitrate), 50_000_000))
        self._target_bitrate = bitrate

        # Try to update active encoder bitrate on existing connections
        updated = False
        for pc in self._pcs:
            try:
                for sender in pc.getSenders():
                    if (sender.track and sender.track.kind == "video"
                            and hasattr(sender, "_encoder") and sender._encoder):
                        sender._encoder.target_bitrate = bitrate
                        updated = True
            except Exception:
                pass

        log.info("Seat %s: WebRTC bitrate set to %dk",
                 self._seat.name, bitrate // 1000)
        return web.json_response({
            "ok": True,
            "bitrate": bitrate,
            "updated_active": updated,
        })

    async def cleanup(self) -> None:
        """Close all peer connections and stop tracks."""
        log.info("Seat %s: cleaning up %d WebRTC peer(s)",
                 self._seat.name, len(self._pcs))

        for track in self._video_tracks:
            track.stop()
        self._video_tracks.clear()

        for track in self._audio_tracks:
            track.stop()
        self._audio_tracks.clear()

        for pc in list(self._pcs):
            try:
                await pc.close()
            except Exception:
                pass
        self._pcs.clear()

    async def _remove_pc(self, pc: Any) -> None:
        """Remove a peer connection that has disconnected."""
        if pc in self._pcs:
            self._pcs.remove(pc)
            try:
                await pc.close()
            except Exception:
                pass
            log.debug("Seat %s: WebRTC peer disconnected (remaining=%d)",
                      self._seat.name, len(self._pcs))

    def to_dict(self) -> dict:
        """Serialize WebRTC state for monitoring."""
        return {
            "available": self.available,
            "peers": self.peer_count,
            "target_bitrate": self._target_bitrate,
            "video_tracks": len(self._video_tracks),
            "audio_tracks": len(self._audio_tracks),
        }


# ── aiortc MediaStreamTrack wrappers ────────────────────────────────────────

# These wrap our track classes to inherit from MediaStreamTrack properly.
# Only created when aiortc is available (inside handle_offer).

class _WrappedVideoTrack:
    """Wraps SeatVideoTrack as an aiortc-compatible MediaStreamTrack."""

    kind = "video"

    def __init__(self, inner: SeatVideoTrack) -> None:
        self._inner = inner
        # Dynamically inherit from MediaStreamTrack if available
        try:
            from aiortc import MediaStreamTrack
            self.__class__ = type(
                "_WrappedVideoTrack",
                (MediaStreamTrack,),
                {
                    "kind": "video",
                    "recv": self._recv,
                    "stop": self._stop,
                },
            )
            MediaStreamTrack.__init__(self)
        except ImportError:
            pass

    async def _recv(self) -> Any:
        return await self._inner.recv()

    async def recv(self) -> Any:
        return await self._inner.recv()

    def _stop(self) -> None:
        self._inner.stop()

    def stop(self) -> None:
        self._inner.stop()


class _WrappedAudioTrack:
    """Wraps SeatAudioTrack as an aiortc-compatible MediaStreamTrack."""

    kind = "audio"

    def __init__(self, inner: SeatAudioTrack) -> None:
        self._inner = inner
        try:
            from aiortc import MediaStreamTrack
            self.__class__ = type(
                "_WrappedAudioTrack",
                (MediaStreamTrack,),
                {
                    "kind": "audio",
                    "recv": self._recv,
                    "stop": self._stop,
                },
            )
            MediaStreamTrack.__init__(self)
        except ImportError:
            pass

    async def _recv(self) -> Any:
        return await self._inner.recv()

    async def recv(self) -> Any:
        return await self._inner.recv()

    def _stop(self) -> None:
        self._inner.stop()

    def stop(self) -> None:
        self._inner.stop()
