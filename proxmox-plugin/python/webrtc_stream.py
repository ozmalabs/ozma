#!/usr/bin/python3
# SPDX-License-Identifier: GPL-2.0-or-later
"""
WebRTC video track that feeds frames from a DBusDisplayClient or KVMFR source.

Used by display-service.py when `aiortc` is available and a D-Bus display
source is connected.  Falls back gracefully if aiortc is not installed.

The FramebufferVideoTrack wraps the framebuffer source as an aiortc
MediaStreamTrack, encoding frames as H.264 via aiortc's built-in codec.
"""

from __future__ import annotations

import asyncio
import fractions
import io
import logging
import time
from typing import Any

log = logging.getLogger("ozma.proxmox.webrtc_stream")

try:
    from aiortc import MediaStreamTrack  # type: ignore[import]
    from aiortc.contrib.media import MediaPlayer  # type: ignore[import]
    import av  # type: ignore[import]
    _AIORTC_AVAILABLE = True
except ImportError:
    _AIORTC_AVAILABLE = False
    # Provide a stub base class so the import doesn't hard-fail
    class MediaStreamTrack:  # type: ignore[no-redef]
        kind = "video"


class FramebufferVideoTrack(MediaStreamTrack):
    """aiortc VideoStreamTrack that pulls JPEG frames from a display source.

    Accepts any object with a `latest_frame` attribute (bytes | None) that
    contains a JPEG-encoded frame.  Compatible with DBusDisplayClient and
    the KVMFR capture buffer.
    """

    kind = "video"

    def __init__(self, source: Any, fps: int = 30) -> None:
        if _AIORTC_AVAILABLE:
            super().__init__()
        self._source = source
        self._fps = fps
        self._frame_interval = 1.0 / fps
        self._pts = 0
        self._time_base = fractions.Fraction(1, 90000)  # RTP clock 90kHz
        self._started = time.monotonic()

    async def recv(self) -> Any:
        """Return the next video frame as an av.VideoFrame."""
        if not _AIORTC_AVAILABLE:
            raise RuntimeError("aiortc not available")

        await asyncio.sleep(self._frame_interval)

        jpeg = getattr(self._source, "latest_frame", None)
        if jpeg:
            try:
                frame = self._jpeg_to_av_frame(jpeg)
            except Exception as exc:
                log.debug("Frame decode error: %s", exc)
                frame = self._blank_frame()
        else:
            frame = self._blank_frame()

        # Assign presentation timestamp
        elapsed = time.monotonic() - self._started
        self._pts = int(elapsed / float(self._time_base))
        frame.pts = self._pts
        frame.time_base = self._time_base
        return frame

    def _jpeg_to_av_frame(self, jpeg: bytes) -> Any:
        import av
        from PIL import Image
        img = Image.open(io.BytesIO(jpeg)).convert("YUV")
        return av.VideoFrame.from_ndarray(
            __import__("numpy").array(img), format="yuv420p"
        )

    def _blank_frame(self) -> Any:
        import av
        import numpy as np
        # 1920x1080 black frame (YUV420p)
        y = np.zeros((1080, 1920), dtype=np.uint8)
        uv = np.full((540, 960), 128, dtype=np.uint8)
        frame = av.VideoFrame(width=1920, height=1080, format="yuv420p")
        frame.planes[0].update(y.tobytes())
        frame.planes[1].update(uv.tobytes())
        frame.planes[2].update(uv.tobytes())
        return frame
