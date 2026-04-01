# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Looking Glass IVSHMEM reader — capture GPU passthrough VM framebuffer.

When a VM has a real GPU via VFIO passthrough, QEMU's VNC shows nothing
because the GPU renders directly. Looking Glass solves this by:

  1. Guest program captures the GPU framebuffer (DXGI on Windows, KMS on Linux)
  2. Writes frames to an IVSHMEM shared memory region (PCI device)
  3. This module reads frames from the host side of that shared memory

The result: GPU passthrough VMs feed frames into the same capture pipeline
as regular VMs. The controller doesn't know the difference.

QEMU args for IVSHMEM:
  -device ivshmem-plain,memdev=ivshmem,bus=pci.0
  -object memory-backend-file,id=ivshmem,share=on,mem-path=/dev/shm/looking-glass,size=128M

Guest side: install Looking Glass Host (https://looking-glass.io)

Output: raw frames written to a v4l2loopback device (/dev/videoN),
identical to VirtualCapture — plugs into existing HLS/MJPEG pipeline.
"""

from __future__ import annotations

import asyncio
import ctypes
import io
import logging
import mmap
import os
import struct
import time
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.softnode.looking_glass")

# KVMFR (KVM Frame Relay) shared memory header
# See: https://github.com/gnif/LookingGlass/blob/master/common/include/common/KVMFR.h

KVMFR_MAGIC = b"KVMFR---"
KVMFR_VERSION = 21  # Current protocol version

# Frame types
FRAME_TYPE_INVALID = 0
FRAME_TYPE_BGRA = 1
FRAME_TYPE_RGBA = 2
FRAME_TYPE_RGB_24 = 3
FRAME_TYPE_NV12 = 4
FRAME_TYPE_YUV420 = 5

# Frame flags
FRAME_FLAG_BLOCK_OPAQUE = 1 << 0

# Header offsets (from KVMFR.h)
KVMFR_HEADER_SIZE = 32      # magic(8) + version(4) + features(4) + padding(16)
FRAME_HEADER_OFFSET = 1024  # Frame header starts at 1KB into the SHM


class KVMFRHeader(ctypes.LittleEndianStructure):
    """KVMFR shared memory header."""
    _pack_ = 1
    _fields_ = [
        ("magic", ctypes.c_char * 8),
        ("version", ctypes.c_uint32),
        ("features", ctypes.c_uint32),
    ]


class KVMFRFrame(ctypes.LittleEndianStructure):
    """KVMFR frame descriptor."""
    _pack_ = 1
    _fields_ = [
        ("frame_serial", ctypes.c_uint32),
        ("frame_type", ctypes.c_uint32),
        ("width", ctypes.c_uint32),
        ("height", ctypes.c_uint32),
        ("stride", ctypes.c_uint32),
        ("pitch", ctypes.c_uint32),
        ("offset", ctypes.c_uint32),       # offset from start of SHM to pixel data
        ("data_size", ctypes.c_uint32),
        ("flags", ctypes.c_uint32),
        ("rotation", ctypes.c_uint32),
        ("damaged_rects_count", ctypes.c_uint32),
    ]


class LookingGlassCapture:
    """
    Reads framebuffer from Looking Glass IVSHMEM shared memory.

    Produces frames as JPEG bytes or raw pixels for piping to ffmpeg/v4l2loopback.
    Implements the same interface as VirtualCapture: start() → device_path.
    """

    def __init__(self, vm_name: str, shm_path: str = "/dev/shm/looking-glass",
                 fps: int = 30) -> None:
        self._vm_name = vm_name
        self._shm_path = shm_path
        self._fps = fps
        self._mm: mmap.mmap | None = None
        self._fd: int = -1
        self._running = False
        self._width = 0
        self._height = 0
        self._last_serial = 0
        self._latest_frame: bytes | None = None  # JPEG
        self._device_path: str | None = None
        self._ffmpeg_proc: asyncio.subprocess.Process | None = None

    @property
    def device_path(self) -> str | None:
        return self._device_path

    @property
    def width(self) -> int:
        return self._width

    @property
    def height(self) -> int:
        return self._height

    @property
    def available(self) -> bool:
        """Check if the IVSHMEM shared memory file exists and is valid."""
        if not Path(self._shm_path).exists():
            return False
        try:
            with open(self._shm_path, "rb") as f:
                header_bytes = f.read(ctypes.sizeof(KVMFRHeader))
                if len(header_bytes) < ctypes.sizeof(KVMFRHeader):
                    return False
                header = KVMFRHeader.from_buffer_copy(header_bytes)
                return header.magic == KVMFR_MAGIC
        except (OSError, ValueError):
            return False

    async def start(self) -> str | None:
        """
        Start reading frames from IVSHMEM.

        Returns a v4l2loopback device path (e.g., /dev/video10) that the
        existing capture pipeline can consume, or None if unavailable.
        """
        if not self.available:
            log.warning("Looking Glass SHM not available at %s", self._shm_path)
            return None

        try:
            self._fd = os.open(self._shm_path, os.O_RDONLY)
            file_size = os.fstat(self._fd).st_size
            self._mm = mmap.mmap(self._fd, file_size, access=mmap.ACCESS_READ)

            # Validate header
            header = KVMFRHeader.from_buffer_copy(self._mm[:ctypes.sizeof(KVMFRHeader)])
            if header.magic != KVMFR_MAGIC:
                log.error("Invalid KVMFR magic: %r", header.magic)
                await self.stop()
                return None

            log.info("Looking Glass connected: %s (KVMFR v%d, %d MB SHM)",
                     self._vm_name, header.version, file_size // (1024 * 1024))

            # Read initial frame to get dimensions
            frame = self._read_frame_header()
            if frame and frame.width > 0:
                self._width = frame.width
                self._height = frame.height
                log.info("Looking Glass initial resolution: %dx%d", self._width, self._height)

            # Create v4l2loopback device and start frame pump
            self._device_path = await self._create_loopback()
            if self._device_path:
                self._running = True
                asyncio.create_task(self._frame_pump(), name=f"lg-pump-{self._vm_name}")
                return self._device_path

        except Exception as e:
            log.error("Looking Glass start failed: %s", e)
            await self.stop()

        return None

    async def stop(self) -> None:
        """Stop the capture and clean up."""
        self._running = False
        if self._ffmpeg_proc:
            try:
                self._ffmpeg_proc.kill()
                await self._ffmpeg_proc.wait()
            except Exception:
                pass
            self._ffmpeg_proc = None
        if self._mm:
            self._mm.close()
            self._mm = None
        if self._fd >= 0:
            os.close(self._fd)
            self._fd = -1

    async def get_frame_jpeg(self) -> bytes | None:
        """Get the latest frame as JPEG. For snapshot/MJPEG endpoints."""
        return self._latest_frame

    def _read_frame_header(self) -> KVMFRFrame | None:
        """Read the current frame descriptor from shared memory."""
        if not self._mm:
            return None
        try:
            offset = FRAME_HEADER_OFFSET
            size = ctypes.sizeof(KVMFRFrame)
            data = self._mm[offset:offset + size]
            if len(data) < size:
                return None
            return KVMFRFrame.from_buffer_copy(data)
        except Exception:
            return None

    def _read_frame_pixels(self, frame: KVMFRFrame) -> bytes | None:
        """Read raw pixel data for a frame from shared memory."""
        if not self._mm or not frame or frame.data_size == 0:
            return None
        try:
            start = frame.offset
            end = start + frame.data_size
            if end > len(self._mm):
                return None
            return bytes(self._mm[start:end])
        except Exception:
            return None

    async def _frame_pump(self) -> None:
        """Background task: read frames from SHM, write to v4l2loopback via ffmpeg."""
        frame_interval = 1.0 / self._fps

        while self._running:
            try:
                frame = self._read_frame_header()
                if not frame or frame.frame_serial == self._last_serial:
                    await asyncio.sleep(frame_interval / 2)
                    continue

                self._last_serial = frame.frame_serial

                # Resolution change detection
                if frame.width != self._width or frame.height != self._height:
                    self._width = frame.width
                    self._height = frame.height
                    log.info("Looking Glass resolution changed: %dx%d", self._width, self._height)
                    # Restart ffmpeg with new dimensions
                    if self._ffmpeg_proc:
                        self._ffmpeg_proc.kill()
                        await self._ffmpeg_proc.wait()
                    self._ffmpeg_proc = await self._start_ffmpeg()

                # Read pixels and pipe to ffmpeg
                pixels = self._read_frame_pixels(frame)
                if pixels and self._ffmpeg_proc and self._ffmpeg_proc.stdin:
                    try:
                        self._ffmpeg_proc.stdin.write(pixels)
                        await self._ffmpeg_proc.stdin.drain()
                    except (BrokenPipeError, ConnectionResetError):
                        log.warning("ffmpeg pipe broken, restarting")
                        self._ffmpeg_proc = await self._start_ffmpeg()

                    # Also create a JPEG snapshot for the API
                    await self._update_jpeg_snapshot(pixels, frame)

                await asyncio.sleep(frame_interval)

            except asyncio.CancelledError:
                break
            except Exception as e:
                log.error("Looking Glass frame pump error: %s", e)
                await asyncio.sleep(1)

    async def _update_jpeg_snapshot(self, pixels: bytes, frame: KVMFRFrame) -> None:
        """Convert raw pixels to JPEG for snapshot endpoint."""
        try:
            from PIL import Image
            if frame.frame_type == FRAME_TYPE_BGRA:
                mode, raw_mode = "RGBA", "BGRA"
            elif frame.frame_type == FRAME_TYPE_RGBA:
                mode, raw_mode = "RGBA", "RGBA"
            elif frame.frame_type == FRAME_TYPE_RGB_24:
                mode, raw_mode = "RGB", "RGB"
            else:
                return  # Can't convert NV12/YUV420 without more work
            img = Image.frombytes(mode, (frame.width, frame.height), pixels, "raw", raw_mode, frame.stride)
            buf = io.BytesIO()
            img.convert("RGB").save(buf, format="JPEG", quality=75)
            self._latest_frame = buf.getvalue()
        except Exception:
            pass

    async def _create_loopback(self) -> str | None:
        """Create a v4l2loopback device for this capture source."""
        # Reuse VirtualCapture's pattern
        import shutil
        if not shutil.which("v4l2loopback-ctl"):
            log.warning("v4l2loopback-ctl not found — can't create virtual capture device")
            return None

        label = f"Ozma LG {self._vm_name}"
        try:
            proc = await asyncio.create_subprocess_exec(
                "sudo", "v4l2loopback-ctl", "add",
                f"card_label={label}",
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            stdout, _ = await proc.communicate()
            device = stdout.decode().strip()
            if device and device.startswith("/dev/video"):
                log.info("Looking Glass loopback device: %s (%s)", device, label)
                return device
        except Exception as e:
            log.warning("Failed to create loopback device: %s", e)
        return None

    async def _start_ffmpeg(self) -> asyncio.subprocess.Process | None:
        """Start ffmpeg to pipe raw pixels into v4l2loopback."""
        if not self._device_path or self._width == 0:
            return None

        # Determine pixel format from last known frame type
        pix_fmt = "bgra"  # default for BGRA frames

        cmd = [
            "ffmpeg", "-hide_banner", "-loglevel", "warning",
            "-f", "rawvideo",
            "-pix_fmt", pix_fmt,
            "-s", f"{self._width}x{self._height}",
            "-r", str(self._fps),
            "-i", "pipe:0",
            "-vf", "format=yuv420p",
            "-f", "v4l2",
            "-pix_fmt", "yuv420p",
            self._device_path,
        ]

        try:
            proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdin=asyncio.subprocess.PIPE,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.PIPE,
            )
            log.info("Looking Glass ffmpeg started: %dx%d → %s", self._width, self._height, self._device_path)
            return proc
        except Exception as e:
            log.error("Failed to start ffmpeg for Looking Glass: %s", e)
            return None
