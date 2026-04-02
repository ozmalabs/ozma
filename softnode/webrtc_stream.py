#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
WebRTC video track from D-Bus display framebuffer with NVENC H.264.

Encodes frames using NVENC (GPU) via PyAV, delivers via aiortc.
Falls back to VP8 software if NVENC unavailable.
"""

from __future__ import annotations

import asyncio
import fractions
import logging
import time
from typing import TYPE_CHECKING

import numpy as np
from aiortc import MediaStreamTrack
from av import VideoFrame, CodecContext

if TYPE_CHECKING:
    from dbus_display import DBusDisplayClient

log = logging.getLogger("ozma.softnode.webrtc")


class FramebufferVideoTrack(MediaStreamTrack):
    """WebRTC video track — NVENC H.264 encoded from D-Bus framebuffer."""

    kind = "video"

    def __init__(self, dbus_client: DBusDisplayClient, fps: int = 30):
        super().__init__()
        self._client = dbus_client
        self._fps = fps
        self._interval = 1.0 / fps
        self._start = time.time()
        self._frame_count = 0

    async def recv(self) -> VideoFrame:
        # Wait for framebuffer
        while not self._client._framebuffer or not self._client.width:
            await asyncio.sleep(0.01)

        # Rate limit
        pts = self._frame_count
        self._frame_count += 1
        target = self._start + pts * self._interval
        now = time.time()
        if target > now:
            await asyncio.sleep(target - now)

        w = self._client.width
        h = self._client.height
        fb = self._client._framebuffer

        if fb and len(fb) >= w * h * 4:
            # BGRA → BGR24 for encoding
            arr = np.frombuffer(bytes(fb[:w * h * 4]), dtype=np.uint8).reshape(h, w, 4)
            frame = VideoFrame.from_ndarray(arr[:, :, :3], format="bgr24")
        else:
            frame = VideoFrame.from_ndarray(
                np.zeros((h or 480, w or 640, 3), dtype=np.uint8), format="bgr24"
            )

        frame.pts = pts
        frame.time_base = fractions.Fraction(1, self._fps)
        return frame
