#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
WebRTC audio track from PipeWire/PulseAudio monitor.

Captures audio from a VM's PulseAudio sink input and serves
as a WebRTC audio track via aiortc.
"""

from __future__ import annotations

import asyncio
import fractions
import logging
import os
import queue
import subprocess
import time
import threading
from typing import Any

import numpy as np
from aiortc import MediaStreamTrack
from av import AudioFrame

log = logging.getLogger("ozma.softnode.webrtc_audio")


class PulseAudioTrack(MediaStreamTrack):
    """
    WebRTC audio track capturing from PulseAudio monitor.

    Uses ffmpeg to capture from the default sink monitor and
    delivers PCM frames to aiortc.
    """

    kind = "audio"

    def __init__(self, sink_monitor: str = "default.monitor",
                 sample_rate: int = 48000, channels: int = 2):
        super().__init__()
        self._sink = sink_monitor
        self._rate = sample_rate
        self._channels = channels
        self._frame_size = 960  # 20ms at 48kHz
        self._queue: queue.Queue = queue.Queue(maxsize=50)  # thread-safe
        self._proc: subprocess.Popen | None = None
        self._thread: threading.Thread | None = None
        self._started = False
        self._pts = 0

    def _start_capture(self):
        """Start ffmpeg capture from PulseAudio monitor."""
        if self._started:
            return
        self._started = True

        cmd = [
            "ffmpeg", "-hide_banner", "-loglevel", "error",
            "-f", "pulse",
            "-i", self._sink,
            "-f", "s16le",
            "-ar", str(self._rate),
            "-ac", str(self._channels),
            "-",
        ]

        env = os.environ.copy()
        env["PULSE_SERVER"] = "unix:/run/user/1000/pulse/native"
        self._proc = subprocess.Popen(
            cmd,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            env=env,
        )

        self._thread = threading.Thread(target=self._read_loop, daemon=True)
        self._thread.start()
        log.info("Audio capture started: %s @ %dHz %dch", self._sink, self._rate, self._channels)

    def _read_loop(self):
        """Read PCM data from ffmpeg stdout."""
        bytes_per_frame = self._frame_size * self._channels * 2  # s16le
        while self._proc and self._proc.poll() is None:
            data = self._proc.stdout.read(bytes_per_frame)
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

    async def recv(self) -> AudioFrame:
        if not self._started:
            self._start_capture()

        # Poll the thread-safe queue
        data = None
        for _ in range(100):  # wait up to 1 second
            try:
                data = self._queue.get_nowait()
                break
            except queue.Empty:
                await asyncio.sleep(0.01)
        if data is None:
            data = b"\x00" * (self._frame_size * self._channels * 2)

        arr = np.frombuffer(data, dtype=np.int16).reshape(1, -1)
        frame = AudioFrame.from_ndarray(arr, format="s16", layout="stereo" if self._channels == 2 else "mono")
        frame.sample_rate = self._rate
        frame.pts = self._pts
        frame.time_base = fractions.Fraction(1, self._rate)
        self._pts += self._frame_size
        return frame

    def stop(self):
        super().stop()
        if self._proc:
            self._proc.terminate()
            self._proc = None
