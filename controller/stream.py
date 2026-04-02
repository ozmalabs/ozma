# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
VNC → H.265 HLS stream manager.

For each node that advertises vnc_host/vnc_port in its mDNS TXT record,
StreamManager:
  1. Connects via asyncvnc and reads raw RGBA frames.
  2. Pipes frames to an ffmpeg subprocess as rawvideo.
  3. ffmpeg encodes H.265 (or H.264 fallback) and outputs HLS .m3u8 + .ts
     segments to static/streams/{safe_node_id}/.

The HLS manifest is served at /streams/{safe_node_id}/stream.m3u8 by the
static file mount in api.py.

Browser side uses hls.js to play the stream and Three.js VideoTexture to
display it on a monitor mesh in the 3D scene.
"""

import asyncio
import io
import logging
from pathlib import Path
from typing import TYPE_CHECKING, Any, AsyncIterator

import asyncvnc
import numpy as np
from PIL import Image

if TYPE_CHECKING:
    from state import AppState, NodeInfo

log = logging.getLogger("ozma.stream")

# H.265 segments for production use; H.264 for broader browser compat
_CODEC_H265 = [
    "libx265",
    "-x265-params", "log-level=error:keyint=30:min-keyint=30",
]
_CODEC_H264 = [
    "libx264",
    "-profile:v", "baseline",
    "-tune", "zerolatency",
]


def safe_id(node_id: str) -> str:
    """mDNS node_id → filesystem-safe directory name."""
    return node_id.replace(".", "_").replace("/", "_")


class StreamCapture:
    """Captures one node's VNC display and encodes it to HLS."""

    def __init__(
        self,
        node_id: str,
        vnc_host: str,
        vnc_port: int,
        out_dir: Path,
        codec: str = "h265",
    ) -> None:
        self.node_id = node_id
        self.active = False
        self._vnc_host = vnc_host
        self._vnc_port = vnc_port
        self._out_dir = out_dir
        self._codec = codec
        self._task: asyncio.Task | None = None
        self._stop = asyncio.Event()
        self._vnc_writer: asyncio.StreamWriter | None = None
        self._vnc_w: int = 1280
        self._vnc_h: int = 800
        # Latest JPEG frame for MJPEG subscribers; maxsize=1 means no backlog
        self._jpeg_frame: bytes | None = None
        self._jpeg_event: asyncio.Event = asyncio.Event()

    @property
    def stream_path(self) -> str:
        """URL path for the HLS manifest (relative to static root)."""
        return f"streams/{safe_id(self.node_id)}/stream.m3u8"

    def start(self) -> None:
        if self._task and not self._task.done():
            return
        self._stop.clear()
        self._task = asyncio.create_task(
            self._run_with_backoff(), name=f"stream-{self.node_id}"
        )

    def stop(self) -> None:
        self._stop.set()
        if self._task:
            self._task.cancel()
        self.active = False

    async def mjpeg_frames(self) -> AsyncIterator[bytes]:
        """Yield JPEG bytes as each new frame is captured."""
        while True:
            await self._jpeg_event.wait()
            if self._jpeg_frame is not None:
                yield self._jpeg_frame

    async def send_pointer(self, x: int, y: int, buttons: int) -> None:
        """Send a VNC PointerEvent. x/y are display pixels; buttons is a bitmask."""
        w = self._vnc_writer
        if w is None or w.is_closing():
            return
        w.write(
            b'\x05'
            + bytes([buttons & 0xFF])
            + x.to_bytes(2, 'big')
            + y.to_bytes(2, 'big')
        )
        await w.drain()

    async def send_key(self, key: str, down: bool) -> None:
        """Inject a key press or release into the VNC session."""
        w = self._vnc_writer
        if w is None or w.is_closing():
            return
        try:
            keysym = asyncvnc.key_codes[key]
        except KeyError:
            log.debug("Unknown keysym: %r", key)
            return
        flag = b'\x01' if down else b'\x00'
        w.write(b'\x04' + flag + b'\x00\x00' + keysym.to_bytes(4, 'big'))
        await w.drain()

    # ── internal ─────────────────────────────────────────────────────────────

    async def _run_with_backoff(self) -> None:
        backoff = 1.0
        while not self._stop.is_set():
            try:
                await self._capture_loop()
            except asyncio.CancelledError:
                return
            except Exception as e:
                log.warning("Stream %s error: %s — retry in %.0fs", self.node_id, e, backoff)
            try:
                await asyncio.wait_for(
                    asyncio.shield(self._stop.wait()), timeout=backoff
                )
                return
            except asyncio.TimeoutError:
                backoff = min(backoff * 2, 15.0)

    async def _capture_loop(self) -> None:
        self._out_dir.mkdir(parents=True, exist_ok=True)

        vcodec = _CODEC_H265 if self._codec == "h265" else _CODEC_H264

        log.info(
            "Connecting to VNC %s:%d for %s",
            self._vnc_host, self._vnc_port, self.node_id,
        )

        async with asyncvnc.connect(self._vnc_host, self._vnc_port) as client:
            self._vnc_writer = client.writer
            w, h = client.video.width, client.video.height
            self._vnc_w = w
            self._vnc_h = h
            log.info("VNC connected: %s %dx%d", self.node_id, w, h)

            # Cap width to 1024 to keep bitrate reasonable
            out_w = min(w, 1024)
            out_h = (out_w * h // w) & ~1   # keep even height

            fps_target = 20
            cmd = [
                "ffmpeg", "-y",
                # Input: raw RGBA frames piped from asyncvnc
                "-f", "rawvideo",
                "-pixel_format", "rgba",
                "-video_size", f"{w}x{h}",
                "-framerate", str(fps_target),
                "-i", "pipe:0",
                # Scale + encode
                # rgba → yuv420p (required; libx265 has no alpha support)
                "-vf", f"scale={out_w}:{out_h},format=yuv420p",
                "-c:v", *vcodec,
                "-preset", "ultrafast",
                # HLS output
                "-f", "hls",
                "-hls_time", "1",
                "-hls_list_size", "4",
                "-hls_flags", "delete_segments+independent_segments",
                "-hls_segment_filename", str(self._out_dir / "seg_%03d.ts"),
                str(self._out_dir / "stream.m3u8"),
            ]

            proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdin=asyncio.subprocess.PIPE,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.DEVNULL,
            )
            self.active = True
            log.info("HLS encoding started for %s → %s", self.node_id, self._out_dir)

            try:
                # First full frame
                await client.screenshot()

                fps_interval = 1.0 / fps_target
                loop = asyncio.get_running_loop()

                while not self._stop.is_set():
                    frame = client.video.as_rgba()
                    arr = np.ascontiguousarray(frame)
                    frame_bytes = arr.tobytes()

                    try:
                        proc.stdin.write(frame_bytes)
                        await proc.stdin.drain()
                    except (BrokenPipeError, ConnectionResetError):
                        log.warning("ffmpeg pipe closed for %s", self.node_id)
                        break

                    # Publish JPEG for low-latency MJPEG subscribers
                    img = Image.fromarray(arr, 'RGBA').convert('RGB')
                    if out_w != w:
                        img = img.resize((out_w, out_h), Image.LANCZOS)
                    buf = io.BytesIO()
                    img.save(buf, format='JPEG', quality=70)
                    self._jpeg_frame = buf.getvalue()
                    self._jpeg_event.set()
                    self._jpeg_event.clear()

                    # Drain all available VNC updates within the frame interval
                    # so the next snapshot has as fresh a picture as possible.
                    client.video.refresh()
                    deadline = loop.time() + fps_interval
                    while True:
                        remaining = deadline - loop.time()
                        if remaining <= 0:
                            break
                        try:
                            await asyncio.wait_for(client.read(), timeout=remaining)
                        except asyncio.TimeoutError:
                            break

            finally:
                self._vnc_writer = None
                self.active = False
                if proc.stdin and not proc.stdin.is_closing():
                    try:
                        proc.stdin.close()
                    except Exception:
                        pass
                try:
                    await asyncio.wait_for(proc.wait(), timeout=5.0)
                except asyncio.TimeoutError:
                    proc.kill()


class StreamManager:
    """
    Manages StreamCapture instances for all nodes with VNC display info.
    Auto-starts captures when a node becomes available and polls for new nodes.
    """

    def __init__(
        self,
        state: "AppState",
        static_dir: Path | None = None,
        codec: str = "h264",   # h264=broad browser compat; h265=production via OZMA_STREAM_CODEC=h265
        codec_manager: Any = None,
    ) -> None:
        self._state = state
        self._static_dir = static_dir or (Path(__file__).parent / "static" / "streams")
        self._codec = codec
        self._codec_manager = codec_manager
        self._captures: dict[str, StreamCapture] = {}
        self._poll_task: asyncio.Task | None = None

    async def start(self) -> None:
        self._static_dir.mkdir(parents=True, exist_ok=True)
        # Start any already-known nodes
        for node in self._state.nodes.values():
            self._maybe_start(node)
        self._poll_task = asyncio.create_task(self._poll_loop(), name="stream-poll")
        log.info("StreamManager started (codec=%s)", self._codec)

    async def stop(self) -> None:
        if self._poll_task:
            self._poll_task.cancel()
        for cap in list(self._captures.values()):
            cap.stop()

    def stream_url(self, node_id: str) -> str | None:
        entry = self._captures.get(node_id)
        if entry is None:
            return None
        if isinstance(entry, _RemoteStream):
            return entry.remote_url
        return f"/{entry.stream_path}"

    def stream_type(self, node_id: str) -> str:
        """Return 'mjpeg', 'hls-local', or 'hls-remote'."""
        entry = self._captures.get(node_id)
        if isinstance(entry, _RemoteStream):
            return "hls-remote"
        if entry is not None:
            return "mjpeg"
        return "none"

    def list_streams(self) -> list[dict]:
        out = []
        for entry in self._captures.values():
            if isinstance(entry, _RemoteStream):
                out.append({"node_id": entry.node_id, "url": entry.remote_url,
                             "active": entry.active, "type": "hls-remote"})
            else:
                out.append({"node_id": entry.node_id, "url": f"/{entry.stream_path}",
                             "active": entry.active, "type": "mjpeg"})
        return out

    def vnc_dimensions(self, node_id: str) -> tuple[int, int] | None:
        """Return (width, height) of the VNC display for a node, or None."""
        entry = self._captures.get(node_id)
        if isinstance(entry, StreamCapture):
            return (entry._vnc_w, entry._vnc_h)
        return None

    def register_node(self, node: "NodeInfo") -> None:
        """Start capturing for a node if not already started. Public alias for _maybe_start."""
        self._maybe_start(node)

    def mjpeg_frames(self, node_id: str) -> AsyncIterator[bytes] | None:
        entry = self._captures.get(node_id)
        if isinstance(entry, StreamCapture):
            return entry.mjpeg_frames()
        if isinstance(entry, _SoftNodeStream):
            return entry.mjpeg_frames()
        return None

    async def get_snapshot(self, node_id: str) -> bytes | None:
        """Return a single JPEG frame from the node's stream, or None."""
        frames = self.mjpeg_frames(node_id)
        if frames is None:
            return None
        try:
            async for frame in frames:
                return frame  # return first frame
        except Exception:
            pass
        return None

    async def send_pointer(self, node_id: str, x: int, y: int, buttons: int) -> None:
        entry = self._captures.get(node_id)
        if isinstance(entry, _SoftNodeStream):
            await entry.send_pointer(x, y, buttons)
        elif isinstance(entry, StreamCapture):
            await entry.send_pointer(x, y, buttons)

    async def send_key(self, node_id: str, key: str, down: bool) -> None:
        entry = self._captures.get(node_id)
        if isinstance(entry, _SoftNodeStream):
            await entry.send_key(key, down)
        elif isinstance(entry, StreamCapture):
            await entry.send_key(key, down)

    # ── internal ─────────────────────────────────────────────────────────────

    def _maybe_start(self, node: "NodeInfo") -> None:
        # If capture exists but VNC host changed, recreate it
        existing = self._captures.get(node.id)
        if isinstance(existing, StreamCapture):
            if (existing._vnc_host != node.vnc_host or existing._vnc_port != node.vnc_port):
                log.info("VNC address changed for %s: %s:%d → %s:%d",
                         node.id, existing._vnc_host, existing._vnc_port,
                         node.vnc_host, node.vnc_port)
                existing.stop()
                del self._captures[node.id]
            else:
                return
        elif node.id in self._captures:
            return
        # Hardware node serving its own HLS stream
        if node.stream_url:
            self._captures[node.id] = _RemoteStream(node.id, node.stream_url)
            log.info("Remote stream registered for %s → %s", node.id, node.stream_url)
            return
        # Soft node with D-Bus display — pull MJPEG from soft node API
        api_port = node.api_port or 0
        if api_port and node.hw == "soft":
            cap = _SoftNodeStream(node.id, node.host, api_port)
            self._captures[node.id] = cap
            cap.start()
            log.info("Soft node display stream for %s (D-Bus via %s:%d)",
                     node.id, node.host, api_port)
            return
        # VNC node — local capture + MJPEG (fallback)
        if node.vnc_host and node.vnc_port:
            out_dir = self._static_dir / safe_id(node.id)
            cap = StreamCapture(node.id, node.vnc_host, node.vnc_port, out_dir, self._codec)
            self._captures[node.id] = cap
            cap.start()
            log.info("Stream capture started for %s (vnc=%s:%d)",
                     node.id, node.vnc_host, node.vnc_port)

    async def _poll_loop(self) -> None:
        while True:
            await asyncio.sleep(3.0)
            for node in self._state.nodes.values():
                self._maybe_start(node)
            # Remove captures for nodes that have gone offline
            gone = [nid for nid in self._captures if nid not in self._state.nodes]
            for nid in gone:
                entry = self._captures.pop(nid)
                if isinstance(entry, StreamCapture):
                    entry.stop()
                log.info("Stream capture stopped for offline node %s", nid)


class _SoftNodeStream:
    """
    Stream from a soft node's D-Bus display via its HTTP API.

    The soft node captures QEMU's framebuffer via D-Bus and serves
    MJPEG at /display/mjpeg. This class proxies that stream and
    forwards keyboard/mouse input to the soft node's D-Bus display.
    """

    def __init__(self, node_id: str, host: str, api_port: int) -> None:
        self.node_id = node_id
        self._host = host
        self._api_port = api_port
        self.active = False
        self._latest_jpeg: bytes | None = None
        self._task: asyncio.Task | None = None
        self._vnc_w = 1024
        self._vnc_h = 768

    def start(self) -> None:
        self._task = asyncio.create_task(self._pull_loop(), name=f"softnode-stream-{self.node_id}")
        self.active = True

    def stop(self) -> None:
        self.active = False
        if self._task:
            self._task.cancel()

    @property
    def stream_path(self) -> str:
        return f"api/v1/streams/{self.node_id}/mjpeg"

    async def _pull_loop(self) -> None:
        """Pull JPEG frames from the soft node's snapshot endpoint."""
        import aiohttp
        url = f"http://{self._host}:{self._api_port}/display/snapshot"
        while self.active:
            try:
                async with aiohttp.ClientSession() as session:
                    async with session.get(url, timeout=aiohttp.ClientTimeout(total=3)) as resp:
                        if resp.status == 200:
                            self._latest_jpeg = await resp.read()
                            # Parse dimensions
                            try:
                                from PIL import Image
                                import io
                                img = Image.open(io.BytesIO(self._latest_jpeg))
                                self._vnc_w = img.width
                                self._vnc_h = img.height
                            except Exception:
                                pass
            except Exception:
                pass
            await asyncio.sleep(1.0 / 15)

    async def mjpeg_frames(self):
        """Yield JPEG frames for MJPEG streaming."""
        while self.active:
            if self._latest_jpeg:
                yield self._latest_jpeg
            await asyncio.sleep(1.0 / 15)

    async def send_pointer(self, x: int, y: int, buttons: int) -> None:
        """Forward mouse to soft node D-Bus display."""
        import aiohttp
        action = "click" if buttons else "move"
        try:
            async with aiohttp.ClientSession() as session:
                await session.post(
                    f"http://{self._host}:{self._api_port}/input/mouse",
                    json={"x": x, "y": y, "button": 0, "action": action},
                    timeout=aiohttp.ClientTimeout(total=1),
                )
        except Exception:
            pass

    async def send_key(self, key: str, down: bool) -> None:
        """Forward keyboard to soft node D-Bus display."""
        # Map VNC keysym name to evdev keycode
        VNC_TO_EVDEV = {
            'Return': 28, 'BackSpace': 14, 'Tab': 15, 'Escape': 1,
            'Delete': 111, 'Insert': 110, 'Home': 102, 'End': 107,
            'Prior': 104, 'Next': 109,
            'Up': 103, 'Down': 108, 'Left': 105, 'Right': 106,
            'F1': 59, 'F2': 60, 'F3': 61, 'F4': 62, 'F5': 63, 'F6': 64,
            'F7': 65, 'F8': 66, 'F9': 67, 'F10': 68, 'F11': 87, 'F12': 88,
            'Control_L': 29, 'Shift_L': 42, 'Alt_L': 56, 'Super_L': 125,
            'Control_R': 97, 'Shift_R': 54, 'Alt_R': 100, 'Super_R': 126,
            'space': 57, 'Caps_Lock': 58, 'Num_Lock': 69, 'Scroll_Lock': 70,
        }
        # Single character → evdev
        CHAR_EVDEV = {c: 30 + i for i, c in enumerate('asdfghjkl')}
        CHAR_EVDEV.update({c: 16 + i for i, c in enumerate('qwertyuiop')})
        CHAR_EVDEV.update({c: 44 + i for i, c in enumerate('zxcvbnm')})
        CHAR_EVDEV.update({str(i): 2 + i if i > 0 else 11 for i in range(10)})
        CHAR_EVDEV.update({'.': 52, '-': 12, '=': 13, ',': 51, '/': 53, ';': 39,
                           "'": 40, '\\': 43, '[': 26, ']': 27, '`': 41})

        keycode = VNC_TO_EVDEV.get(key)
        if not keycode and len(key) == 1:
            keycode = CHAR_EVDEV.get(key.lower())
        if not keycode:
            return

        import aiohttp
        try:
            async with aiohttp.ClientSession() as session:
                await session.post(
                    f"http://{self._host}:{self._api_port}/input/key",
                    json={"keycode": keycode, "down": down},
                    timeout=aiohttp.ClientTimeout(total=1),
                )
        except Exception:
            pass


class _RemoteStream:
    """Placeholder for a hardware node that serves its own HLS stream."""

    def __init__(self, node_id: str, remote_url: str) -> None:
        self.node_id = node_id
        self.remote_url = remote_url
        self.active = True

    def stop(self) -> None:
        self.active = False
