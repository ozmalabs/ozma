# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
EDID management for capture cards.

EDID (Extended Display Identification Data) tells a GPU what resolutions
a display supports.  HDMI capture cards have their own EDID — the source
machine reads it to decide what resolution to output.

Many cheap capture cards only advertise 1080p/60.  By overriding the EDID,
ozma can force the source to output:
  - 4K (if the GPU and card actually support it)
  - Ultrawide (21:9, 32:9)
  - Custom resolutions

EDID override methods:
  1. v4l2-ctl --set-edid (works on most USB capture cards)
  2. Direct sysfs write to /sys/.../edid (some PCIe cards)
  3. Companion agent on the target machine changes display resolution
     via OS display settings API (Windows: ChangeDisplaySettingsEx,
     Linux: xrandr, macOS: displayplacer)

This module generates EDID binaries and applies them to capture cards.
"""

from __future__ import annotations

import asyncio
import logging
import struct
import shutil
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.edid")


# ── EDID binary generation ───────────────────────────────────────────────────

def generate_edid(
    width: int = 1920,
    height: int = 1080,
    refresh: int = 60,
    name: str = "OZMA",
    manufacturer: str = "OZM",
) -> bytes:
    """
    Generate a minimal valid EDID 1.3 block (128 bytes) for a given resolution.

    This creates an EDID that advertises a single preferred timing.
    The source GPU will use this as the native/preferred resolution.
    """
    edid = bytearray(128)

    # Header (bytes 0-7)
    edid[0:8] = b"\x00\xff\xff\xff\xff\xff\xff\x00"

    # Manufacturer ID (bytes 8-9): 3-letter code packed into 2 bytes
    # Each letter: A=1, B=2, ... Z=26, packed as 5 bits each
    m = manufacturer.upper()[:3].ljust(3, "A")
    mid = ((ord(m[0]) - ord("A") + 1) << 10) | ((ord(m[1]) - ord("A") + 1) << 5) | (ord(m[2]) - ord("A") + 1)
    edid[8] = (mid >> 8) & 0xFF
    edid[9] = mid & 0xFF

    # Product code (bytes 10-11)
    edid[10:12] = b"\x01\x00"

    # Serial number (bytes 12-15)
    edid[12:16] = b"\x01\x00\x00\x00"

    # Week and year of manufacture (bytes 16-17)
    edid[16] = 1    # week
    edid[17] = 36   # year - 1990 = 2026

    # EDID version 1.3 (bytes 18-19)
    edid[18] = 1    # version
    edid[19] = 3    # revision

    # Basic display parameters (bytes 20-24)
    edid[20] = 0x80  # Digital input (DVI/HDMI)
    edid[21] = 0     # Max horizontal image size (cm) — will be computed
    edid[22] = 0     # Max vertical image size (cm)
    edid[23] = 120   # Gamma (2.2 = value/100 + 1, so 120 = 2.20)
    edid[24] = 0x0A  # Supported features: RGB colour, preferred timing

    # Chromaticity (bytes 25-34): sRGB default values
    edid[25:35] = bytes([0xEE, 0x91, 0xA3, 0x54, 0x4C, 0x99, 0x26, 0x0F, 0x50, 0x54])

    # Established timings (bytes 35-37): none beyond our preferred
    edid[35:38] = b"\x00\x00\x00"

    # Standard timings (bytes 38-53): unused, fill with 0x0101
    for i in range(38, 54, 2):
        edid[i] = 0x01
        edid[i + 1] = 0x01

    # Detailed timing descriptor #1 (bytes 54-71): our preferred resolution
    _write_detailed_timing(edid, 54, width, height, refresh)

    # Descriptor #2 (bytes 72-89): Monitor name
    edid[72:75] = b"\x00\x00\x00"
    edid[75] = 0xFC  # Tag: monitor name
    edid[76] = 0x00
    name_bytes = name[:13].encode("ascii").ljust(13, b"\x20")
    edid[77:90] = name_bytes

    # Descriptor #3 (bytes 90-107): Monitor range limits
    edid[90:93] = b"\x00\x00\x00"
    edid[93] = 0xFD  # Tag: range limits
    edid[94] = 0x00
    edid[95] = 1     # Min V freq (Hz)
    edid[96] = max(refresh, 75)  # Max V freq
    edid[97] = 1     # Min H freq (kHz)
    edid[98] = 255   # Max H freq (kHz)
    edid[99] = 255   # Max pixel clock / 10 MHz
    edid[100] = 0x00 # No extended timing info
    edid[101:108] = b"\x0A\x20\x20\x20\x20\x20\x20"

    # Descriptor #4 (bytes 108-125): unused
    edid[108:126] = b"\x00" * 18

    # Extension block count (byte 126)
    edid[126] = 0

    # Checksum (byte 127): make the entire block sum to 0 mod 256
    edid[127] = (256 - (sum(edid[:127]) % 256)) % 256

    return bytes(edid)


def _write_detailed_timing(edid: bytearray, offset: int, w: int, h: int, refresh: int) -> None:
    """Write a Detailed Timing Descriptor at the given offset."""
    # Approximate timing parameters
    h_blank = 160   # horizontal blanking pixels
    v_blank = 35    # vertical blanking lines
    h_total = w + h_blank
    v_total = h + v_blank
    pixel_clock = h_total * v_total * refresh  # Hz
    pc_10khz = pixel_clock // 10000

    h_front = 48
    h_sync = 32
    v_front = 3
    v_sync = 5

    # Pixel clock (2 bytes, little-endian, in 10kHz units)
    edid[offset] = pc_10khz & 0xFF
    edid[offset + 1] = (pc_10khz >> 8) & 0xFF

    # Horizontal active + blanking
    edid[offset + 2] = w & 0xFF
    edid[offset + 3] = h_blank & 0xFF
    edid[offset + 4] = ((w >> 8) << 4) | ((h_blank >> 8) & 0x0F)

    # Vertical active + blanking
    edid[offset + 5] = h & 0xFF
    edid[offset + 6] = v_blank & 0xFF
    edid[offset + 7] = ((h >> 8) << 4) | ((v_blank >> 8) & 0x0F)

    # Sync offsets and widths
    edid[offset + 8] = h_front & 0xFF
    edid[offset + 9] = h_sync & 0xFF
    edid[offset + 10] = ((v_front & 0x0F) << 4) | (v_sync & 0x0F)
    edid[offset + 11] = (((h_front >> 8) & 0x03) << 6) | (((h_sync >> 8) & 0x03) << 4) | \
                         (((v_front >> 4) & 0x03) << 2) | ((v_sync >> 4) & 0x03)

    # Image size (mm) — approximate from pixels assuming ~96 DPI
    mm_w = int(w * 25.4 / 96)
    mm_h = int(h * 25.4 / 96)
    edid[offset + 12] = mm_w & 0xFF
    edid[offset + 13] = mm_h & 0xFF
    edid[offset + 14] = ((mm_w >> 8) << 4) | ((mm_h >> 8) & 0x0F)

    # Borders + flags
    edid[offset + 15] = 0  # H border
    edid[offset + 16] = 0  # V border
    edid[offset + 17] = 0x18  # Non-interlaced, normal display


# ── EDID application ─────────────────────────────────────────────────────────

async def set_edid(device_path: str, edid_data: bytes) -> bool:
    """
    Apply an EDID override to a capture card.

    Tries v4l2-ctl --set-edid first (most compatible), then sysfs write.
    """
    # Method 1: v4l2-ctl --set-edid
    if shutil.which("v4l2-ctl"):
        hex_edid = edid_data.hex()
        try:
            proc = await asyncio.create_subprocess_exec(
                "v4l2-ctl", "-d", device_path,
                "--set-edid=type=hdmi", f"--set-edid=edid={hex_edid}",
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            _, err = await asyncio.wait_for(proc.communicate(), timeout=5.0)
            if proc.returncode == 0:
                log.info("EDID set via v4l2-ctl on %s", device_path)
                return True
            log.debug("v4l2-ctl EDID failed: %s", err.decode().strip())
        except (asyncio.TimeoutError, FileNotFoundError):
            pass

    # Method 2: Write EDID file via v4l2-ctl file format
    if shutil.which("v4l2-ctl"):
        import tempfile
        with tempfile.NamedTemporaryFile(suffix=".bin", delete=False) as f:
            f.write(edid_data)
            tmp_path = f.name
        try:
            proc = await asyncio.create_subprocess_exec(
                "v4l2-ctl", "-d", device_path,
                f"--set-edid=file={tmp_path},format=raw",
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            _, err = await asyncio.wait_for(proc.communicate(), timeout=5.0)
            if proc.returncode == 0:
                log.info("EDID set via v4l2-ctl file on %s", device_path)
                return True
        except (asyncio.TimeoutError, FileNotFoundError):
            pass
        finally:
            Path(tmp_path).unlink(missing_ok=True)

    log.warning("Could not set EDID on %s", device_path)
    return False


async def get_current_edid(device_path: str) -> bytes | None:
    """Read the current EDID from a capture card."""
    if not shutil.which("v4l2-ctl"):
        return None
    try:
        proc = await asyncio.create_subprocess_exec(
            "v4l2-ctl", "-d", device_path, "--get-edid=format=raw",
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.DEVNULL,
        )
        out, _ = await asyncio.wait_for(proc.communicate(), timeout=5.0)
        return out if out and len(out) >= 128 else None
    except (asyncio.TimeoutError, FileNotFoundError):
        return None


def parse_edid_resolution(edid_data: bytes) -> tuple[int, int, int] | None:
    """Extract the preferred resolution from an EDID block."""
    if len(edid_data) < 72:
        return None
    # Read first detailed timing descriptor at offset 54
    pc_10khz = edid_data[54] | (edid_data[55] << 8)
    if pc_10khz == 0:
        return None
    w = edid_data[56] | ((edid_data[58] >> 4) << 8)
    h = edid_data[59] | ((edid_data[61] >> 4) << 8)
    h_blank = edid_data[57] | ((edid_data[58] & 0x0F) << 8)
    v_blank = edid_data[60] | ((edid_data[61] & 0x0F) << 8)
    pixel_clock = pc_10khz * 10000
    h_total = w + h_blank
    v_total = h + v_blank
    if h_total > 0 and v_total > 0:
        refresh = pixel_clock // (h_total * v_total)
    else:
        refresh = 60
    return (w, h, refresh)
