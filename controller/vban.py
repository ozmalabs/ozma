# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
VBAN audio-over-UDP protocol — emitter and receiver.

VBAN is a simple, widely-supported protocol for transporting PCM audio
over UDP. 28-byte fixed header + raw interleaved PCM payload.

Header layout (28 bytes):
  0–3   Magic "VBAN"
  4     Sample rate index (VBAN_SR_LIST lookup)
  5     Samples per frame - 1  (0 = 1 sample per packet)
  6     Channels - 1            (0 = mono, 1 = stereo)
  7     Format nibbles: low = data format, high = codec (0x01 = PCM int16)
  8–23  Stream name (16 bytes, null-padded ASCII)
  24–27 Frame counter (uint32 little-endian, wraps)
  28+   Raw PCM payload

Typical operating point:
  48 000 Hz, 256 samples/frame, stereo, PCM int16.
  Frame rate ≈ 187.5 Hz, payload = 1 024 bytes/frame.

VBANReceiver:
  Listens on a UDP port. Each received frame is written as raw PCM to
  stdout of a pw-cat (PipeWire) subprocess, which creates a named
  virtual source in the PipeWire graph.

VBANSender:
  Reads PCM from a pw-cat source subprocess and sends VBAN frames
  over UDP to a target (node) address.
"""

from __future__ import annotations

import asyncio
import logging
import struct
from typing import Callable

log = logging.getLogger("ozma.vban")

# ── Protocol constants ────────────────────────────────────────────────────────

VBAN_MAGIC = b"VBAN"
VBAN_HEADER_SIZE = 28

# Sample rate index table (VBAN spec, first 21 entries)
VBAN_SR_LIST = [
    6000, 12000, 24000, 48000, 96000, 192000, 384000,
    8000, 16000, 32000, 64000, 128000, 256000, 512000,
    11025, 22050, 44100, 88200, 176400, 352800,
    705600,
]

VBAN_FORMAT_INT16 = 0x01   # low nibble of byte 7
VBAN_CODEC_PCM   = 0x00   # high nibble of byte 7 (PCM = 0)

# Practical defaults
DEFAULT_SAMPLE_RATE = 48000
DEFAULT_CHANNELS    = 2
DEFAULT_SAMPLES_PER_FRAME = 256   # ≈ 5.3 ms @ 48 kHz
DEFAULT_PORT        = 6980


# ── Header encode / decode ────────────────────────────────────────────────────

def _sr_index(rate: int) -> int:
    try:
        return VBAN_SR_LIST.index(rate)
    except ValueError:
        raise ValueError(f"Unsupported VBAN sample rate: {rate}")


def encode_header(
    stream_name: str,
    frame_counter: int,
    sample_rate: int = DEFAULT_SAMPLE_RATE,
    channels: int = DEFAULT_CHANNELS,
    samples_per_frame: int = DEFAULT_SAMPLES_PER_FRAME,
) -> bytes:
    name_bytes = stream_name.encode("ascii", errors="replace")[:16].ljust(16, b"\x00")
    return struct.pack(
        "<4sBBBB16sI",
        VBAN_MAGIC,
        _sr_index(sample_rate),
        samples_per_frame - 1,
        channels - 1,
        VBAN_FORMAT_INT16 | (VBAN_CODEC_PCM << 4),
        name_bytes,
        frame_counter & 0xFFFF_FFFF,
    )


def decode_header(data: bytes) -> dict | None:
    """Return parsed header dict or None if invalid."""
    if len(data) < VBAN_HEADER_SIZE:
        return None
    magic, sr_idx, spf_m1, ch_m1, fmt, name_raw, counter = struct.unpack_from(
        "<4sBBBB16sI", data
    )
    if magic != VBAN_MAGIC:
        return None
    if sr_idx >= len(VBAN_SR_LIST):
        return None
    return {
        "sample_rate":      VBAN_SR_LIST[sr_idx],
        "samples_per_frame": spf_m1 + 1,
        "channels":         ch_m1 + 1,
        "format":           fmt & 0x0F,
        "codec":            (fmt >> 4) & 0x0F,
        "stream_name":      name_raw.rstrip(b"\x00").decode("ascii", errors="replace"),
        "frame_counter":    counter,
        "payload_size":     len(data) - VBAN_HEADER_SIZE,
    }


# ── VBANReceiver ──────────────────────────────────────────────────────────────

class VBANReceiver:
    """
    Receives VBAN UDP frames and pushes raw PCM to a pw-cat playback process,
    which registers a named PipeWire source (virtual audio input).

    The pw-cat process creates a source stream named `stream_name` in
    PipeWire. The AudioRouter can then link it to the output sink.
    """

    def __init__(
        self,
        bind_port: int,
        stream_name: str,
        sample_rate: int = DEFAULT_SAMPLE_RATE,
        channels: int = DEFAULT_CHANNELS,
    ) -> None:
        self._port = bind_port
        self._stream_name = stream_name
        self._rate = sample_rate
        self._channels = channels
        self._transport: asyncio.DatagramTransport | None = None
        self._proc: asyncio.subprocess.Process | None = None
        self._task: asyncio.Task | None = None
        self._stop = asyncio.Event()
        self._frames_received = 0

    @property
    def stream_name(self) -> str:
        return self._stream_name

    async def start(self) -> None:
        self._stop.clear()
        self._task = asyncio.create_task(self._run(), name=f"vban-rx-{self._stream_name}")

    async def stop(self) -> None:
        self._stop.set()
        if self._transport:
            self._transport.close()
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
        # pw-cat in playback mode: reads raw PCM from stdin, plays to PipeWire
        cmd = [
            "pw-cat",
            "--playback",
            "--name", self._stream_name,
            "--target", "0",          # connect to no target (floating source)
            "--format", "s16",
            "--rate", str(self._rate),
            "--channels", str(self._channels),
            "-",                       # read from stdin
        ]
        try:
            self._proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdin=asyncio.subprocess.PIPE,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.DEVNULL,
            )
            log.info("VBAN receiver '%s' started on UDP :%d (pw-cat pid=%d)",
                     self._stream_name, self._port, self._proc.pid)
        except FileNotFoundError:
            log.error("pw-cat not found — VBAN receiver cannot start")
            return
        except Exception as e:
            log.error("Failed to start pw-cat for VBAN receiver '%s': %s", self._stream_name, e)
            return

        loop = asyncio.get_running_loop()
        try:
            self._transport, _ = await loop.create_datagram_endpoint(
                lambda: _VBANProtocol(self._on_frame),
                local_addr=("0.0.0.0", self._port),
            )
            await self._stop.wait()
        except Exception as e:
            log.error("VBAN UDP bind failed on port %d: %s", self._port, e)
        finally:
            if self._transport:
                self._transport.close()
            if self._proc and self._proc.returncode is None:
                self._proc.terminate()

    def _on_frame(self, data: bytes, addr: tuple) -> None:
        hdr = decode_header(data)
        if hdr is None:
            return
        payload = data[VBAN_HEADER_SIZE:]
        if not payload or self._proc is None or self._proc.stdin is None:
            return
        try:
            self._proc.stdin.write(payload)
            self._frames_received += 1
        except Exception:
            pass


class _VBANProtocol(asyncio.DatagramProtocol):
    def __init__(self, callback: Callable[[bytes, tuple], None]) -> None:
        self._cb = callback

    def datagram_received(self, data: bytes, addr: tuple) -> None:
        self._cb(data, addr)

    def error_received(self, exc: Exception) -> None:
        log.debug("VBAN UDP error: %s", exc)


# ── VBANSender ────────────────────────────────────────────────────────────────

class VBANSender:
    """
    Captures audio from a named PipeWire source and sends it as VBAN frames
    to a remote address (used for mic-to-node routing).
    """

    def __init__(
        self,
        target_host: str,
        target_port: int,
        source_name: str | None = None,   # PipeWire source to read; None = default mic
        stream_name: str = "ozma-mic",
        sample_rate: int = DEFAULT_SAMPLE_RATE,
        channels: int = DEFAULT_CHANNELS,
        samples_per_frame: int = DEFAULT_SAMPLES_PER_FRAME,
    ) -> None:
        self._host = target_host
        self._port = target_port
        self._source = source_name
        self._stream_name = stream_name
        self._rate = sample_rate
        self._channels = channels
        self._spf = samples_per_frame
        self._proc: asyncio.subprocess.Process | None = None
        self._task: asyncio.Task | None = None
        self._stop = asyncio.Event()
        self._frame_counter = 0

    async def start(self) -> None:
        self._stop.clear()
        self._task = asyncio.create_task(self._run(), name=f"vban-tx-{self._stream_name}")

    async def stop(self) -> None:
        self._stop.set()
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
        cmd = [
            "pw-cat",
            "--capture",
            "--format", "s16",
            "--rate", str(self._rate),
            "--channels", str(self._channels),
            "-",
        ]
        if self._source:
            cmd = cmd[:-1] + ["--target", self._source, "-"]

        try:
            self._proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdin=asyncio.subprocess.DEVNULL,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.DEVNULL,
            )
        except FileNotFoundError:
            log.error("pw-cat not found — VBAN sender cannot start")
            return

        sock = None
        try:
            import socket
            sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            frame_bytes = self._spf * self._channels * 2  # int16 = 2 bytes

            log.info("VBAN sender '%s' → %s:%d started (pw-cat pid=%d)",
                     self._stream_name, self._host, self._port, self._proc.pid)

            while not self._stop.is_set():
                assert self._proc.stdout is not None
                chunk = await self._proc.stdout.read(frame_bytes)
                if not chunk:
                    break
                # Pad to full frame if needed (last chunk)
                if len(chunk) < frame_bytes:
                    chunk = chunk + b"\x00" * (frame_bytes - len(chunk))
                header = encode_header(
                    self._stream_name, self._frame_counter,
                    self._rate, self._channels, self._spf,
                )
                self._frame_counter = (self._frame_counter + 1) & 0xFFFF_FFFF
                try:
                    sock.sendto(header + chunk, (self._host, self._port))
                except OSError:
                    pass
        finally:
            if sock:
                sock.close()
            if self._proc and self._proc.returncode is None:
                self._proc.terminate()
