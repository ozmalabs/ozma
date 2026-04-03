#!/usr/bin/python3
# SPDX-License-Identifier: GPL-2.0-or-later
"""
Looking Glass / KVMFR shared-memory frame capture.

Reads frames from a KVMFR shared memory segment (/dev/shm/ozma-vmN) and
converts them to JPEG for the display service.  This is the medium-quality
display path — lower latency than QMP screendump but higher than D-Bus p2p.

Requires:
  - VM configured with ivshmem device pointing at /dev/shm/ozma-vmN
  - Guest running Looking Glass client or compatible framebuffer writer
  - Python package: pylookingglass (optional — falls back gracefully)
"""

from __future__ import annotations

import asyncio
import io
import logging
import struct
from pathlib import Path

log = logging.getLogger("ozma.proxmox.looking_glass")

# KVMFR frame header magic
_KVMFR_MAGIC = b"KVMFR000"


class LookingGlassCapture:
    """Read frames from a KVMFR shared memory segment.

    Falls back gracefully on hosts without a compatible shared memory segment
    or without the pylookingglass package installed.
    """

    def __init__(self, name: str, shm_path: str = "") -> None:
        self._name = name
        self._shm_path = shm_path or f"/dev/shm/ozma-{name}"
        self._mmap = None
        self._width = 0
        self._height = 0
        self._running = False

    async def start(self) -> bool:
        """Open the shared memory segment. Returns True if successful."""
        if not Path(self._shm_path).exists():
            return False
        try:
            return await self._open_shm()
        except Exception as exc:
            log.debug("KVMFR open failed: %s", exc)
            return False

    async def _open_shm(self) -> bool:
        try:
            import mmap
            fd = open(self._shm_path, "rb")
            header = fd.read(8)
            if header[:8] != _KVMFR_MAGIC:
                log.debug("KVMFR: bad magic in %s", self._shm_path)
                fd.close()
                return False
            self._mmap = mmap.mmap(fd.fileno(), 0, access=mmap.ACCESS_READ)
            fd.close()
            self._running = True
            log.info("KVMFR capture started from %s", self._shm_path)
            return True
        except Exception as exc:
            log.debug("KVMFR mmap failed: %s", exc)
            return False

    async def get_frame_jpeg(self) -> bytes | None:
        """Read the current frame and return as JPEG bytes, or None if unavailable."""
        if not self._running or self._mmap is None:
            return None
        try:
            return await asyncio.get_event_loop().run_in_executor(None, self._read_frame_jpeg)
        except Exception as exc:
            log.debug("KVMFR read error: %s", exc)
            return None

    def _read_frame_jpeg(self) -> bytes | None:
        """Synchronous frame read — called from executor."""
        try:
            from PIL import Image  # type: ignore[import]
            # KVMFR header: magic(8) + version(4) + width(4) + height(4) + stride(4) + type(4)
            self._mmap.seek(0)
            header = self._mmap.read(28)
            if len(header) < 28:
                return None
            _, _, w, h, stride, fmt = struct.unpack("<8sIIIII", header)
            if w == 0 or h == 0:
                return None
            if self._width != w or self._height != h:
                self._width, self._height = w, h
                log.info("KVMFR resolution: %dx%d", w, h)
            # Read raw pixel data (BGRA32)
            pixel_bytes = stride * h
            raw = self._mmap.read(pixel_bytes)
            img = Image.frombytes("RGBA", (w, h), raw, "raw", "BGRA")
            buf = io.BytesIO()
            img.convert("RGB").save(buf, format="JPEG", quality=80)
            return buf.getvalue()
        except Exception:
            return None

    async def stop(self) -> None:
        self._running = False
        if self._mmap:
            try:
                self._mmap.close()
            except Exception:
                pass
            self._mmap = None
