#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
GStreamer WebRTC server with NVENC H.264 hardware encoding.

Runs as a subprocess using system Python (which has GStreamer bindings).
Communicates with the soft node via a Unix socket for signaling and
shared memory for framebuffer data.

This module provides a wrapper that spawns the GStreamer process and
proxies WebRTC offer/answer through it.
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
import subprocess
import sys
from pathlib import Path
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from dbus_display import DBusDisplayClient

log = logging.getLogger("ozma.softnode.gst_webrtc")

_GST_WORKER = Path(__file__).parent / "gst_webrtc_worker.py"


class GstWebRTCStream:
    """
    GStreamer NVENC WebRTC — runs as subprocess with system Python.

    The worker process reads framebuffer from a shared memory path,
    encodes with NVENC, and handles WebRTC via webrtcbin.
    Signaling goes through a Unix socket.
    """

    def __init__(self, dbus_client: DBusDisplayClient, fps: int = 30):
        self._client = dbus_client
        self._fps = fps
        self._proc: subprocess.Popen | None = None
        self._sock_path = f"/run/ozma/gst-{os.getpid()}.sock"
        self._reader: asyncio.StreamReader | None = None
        self._writer: asyncio.StreamWriter | None = None
        self._started = False

    async def start(self):
        """Start the GStreamer worker subprocess."""
        w = self._client.width or 640
        h = self._client.height or 480

        # Write framebuffer to shared memory for the worker to read
        self._shm_path = f"/dev/shm/ozma-fb-{os.getpid()}"

        self._proc = await asyncio.create_subprocess_exec(
            "/usr/bin/python3", str(_GST_WORKER),
            "--sock", self._sock_path,
            "--shm", self._shm_path,
            "--width", str(w),
            "--height", str(h),
            "--fps", str(self._fps),
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )

        # Wait for worker to create the signaling socket
        for _ in range(50):
            if os.path.exists(self._sock_path):
                break
            await asyncio.sleep(0.1)

        if not os.path.exists(self._sock_path):
            log.warning("GStreamer worker failed to start")
            return False

        self._reader, self._writer = await asyncio.open_unix_connection(self._sock_path)

        # Start frame pusher
        asyncio.create_task(self._push_frames(), name="gst-frame-push")

        self._started = True
        log.info("GStreamer NVENC WebRTC worker started (pid=%d)", self._proc.pid)
        return True

    async def _push_frames(self):
        """Write framebuffer to shared memory for the worker."""
        while self._proc and self._proc.returncode is None:
            fb = self._client._framebuffer
            if fb and self._client.width and self._client.height:
                w, h = self._client.width, self._client.height
                try:
                    with open(self._shm_path, "wb") as f:
                        f.write(bytes(fb[:w * h * 4]))
                except Exception:
                    pass
            await asyncio.sleep(1.0 / self._fps)

    async def create_offer_answer(self, offer_sdp: str) -> str | None:
        """Send offer to worker, get answer back."""
        if not self._started:
            if not await self.start():
                return None

        try:
            msg = json.dumps({"type": "offer", "sdp": offer_sdp}) + "\n"
            self._writer.write(msg.encode())
            await self._writer.drain()

            resp = await asyncio.wait_for(self._reader.readline(), timeout=10)
            data = json.loads(resp)
            return data.get("sdp")
        except Exception as e:
            log.warning("GStreamer signaling error: %s", e)
            return None

    async def stop(self):
        if self._proc:
            self._proc.terminate()
            self._proc = None
        try:
            os.unlink(self._sock_path)
        except FileNotFoundError:
            pass
        try:
            os.unlink(self._shm_path)
        except FileNotFoundError:
            pass
