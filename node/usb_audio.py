# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
USB Audio Class 2 (UAC2) gadget support.

When the UAC2 function is active in the composite gadget, the USB host sees
the node as an audio device:
  - Host capture (microphone input): receives audio forwarded from the HDMI
    capture card.  This is the "playback" direction from the gadget's
    perspective — the device writes audio that the host reads.
  - Host playback (speaker output): audio played by the host is readable on
    the device's ALSA capture interface.  Useful for speaker passthrough, but
    not wired up here by default.

Audio bridge
────────────
Rather than running a separate bridge process (which would conflict with the
HLS ffmpeg for the same ALSA device), the bridge is integrated into
MediaCapture.  USBAudioGadget is responsible only for:
  1. Detecting (and optionally setting up) the UAC2 ConfigFS function.
  2. Discovering the ALSA device name for the gadget's playback interface.
  3. Exposing that device name so MediaCapture can add a second ffmpeg output.

ConfigFS UAC2 terminology
─────────────────────────
  p_* (playback) — audio flowing FROM device TO host.  The device writes to
                   this ALSA playback interface; the host captures it.
  c_* (capture)  — audio flowing FROM host TO device.  The host writes to
                   this endpoint; the device reads it as ALSA capture.
"""

from __future__ import annotations

import asyncio
import logging
import re
import subprocess
from pathlib import Path

log = logging.getLogger("ozma.node.usb_audio")

_GADGET_SCRIPT = Path(__file__).parent.parent / "tinynode" / "gadget" / "setup_gadget.sh"
_UAC2_FUNC_DIR = Path("/sys/kernel/config/usb_gadget/ozma/functions/uac2.usb0")

# Patterns to recognise the UAC2 gadget ALSA card in `aplay -l` output.
_UAC2_NAME_RE = re.compile(r"UAC2|uac2|Gadget Audio|g_audio", re.IGNORECASE)


def find_uac2_playback_device() -> str | None:
    """
    Return the ALSA device string for the UAC2 gadget's playback interface
    (device → host direction), or None if not found.

    Parses `aplay -l` to find a card whose name matches UAC2/gadget patterns,
    then returns `hw:<card_index>,0`.
    """
    try:
        result = subprocess.run(
            ["aplay", "-l"],
            capture_output=True, text=True, timeout=5,
        )
        output = result.stdout
    except (FileNotFoundError, subprocess.TimeoutExpired):
        return None

    for line in output.splitlines():
        if not line.startswith("card "):
            continue
        if _UAC2_NAME_RE.search(line):
            m = re.match(r"card (\d+):", line)
            if m:
                card = m.group(1)
                log.debug("Found UAC2 gadget ALSA card: %s", line.strip())
                return f"hw:{card},0"
    return None


def find_uac2_capture_device() -> str | None:
    """
    Return the ALSA device string for the UAC2 gadget's capture interface
    (host → device direction), or None if not found.

    Same card as the playback device, device index 1 by convention.
    """
    dev = find_uac2_playback_device()
    if dev is None:
        return None
    # Same card, second device (capture from host)
    return dev.replace(",0", ",1")


def uac2_active() -> bool:
    """Return True if the UAC2 ConfigFS function exists and is linked."""
    return _UAC2_FUNC_DIR.exists()


class USBAudioGadget:
    """
    Manages the UAC2 gadget function lifecycle.

    Typical use:
        gadget = await USBAudioGadget.open()
        if gadget.playback_device:
            # pass gadget.playback_device to MediaCapture(uac2_device=...)
        ...
        await gadget.close()
    """

    def __init__(self) -> None:
        self.playback_device: str | None = None   # ALSA device: write here → host hears it
        self.capture_device: str | None = None    # ALSA device: read here ← host plays it

    @classmethod
    async def open(cls, auto_setup: bool = True) -> "USBAudioGadget":
        """
        Detect the UAC2 ALSA interface.  If it's not present and auto_setup
        is True, attempt to (re-)run the gadget setup script which adds the
        UAC2 function to the existing composite gadget.
        """
        gadget = cls()
        if not uac2_active() and auto_setup:
            await gadget._run_setup()

        # Wait briefly for ALSA to create the sound card
        gadget.playback_device = await gadget._wait_for_alsa(timeout=5.0)
        if gadget.playback_device:
            gadget.capture_device = gadget.playback_device.replace(",0", ",1")
            log.info(
                "UAC2 audio gadget ready — playback: %s  capture: %s",
                gadget.playback_device, gadget.capture_device,
            )
        else:
            log.warning(
                "UAC2 ALSA device not found — USB audio will be unavailable. "
                "Ensure the gadget script ran and the kernel has usb_f_uac2."
            )
        return gadget

    async def close(self) -> None:
        """No persistent process to stop — cleanup is handled by teardown_gadget.sh."""

    # ── internal ──────────────────────────────────────────────────────────────

    async def _run_setup(self) -> None:
        if not _GADGET_SCRIPT.exists():
            log.warning("Gadget setup script not found: %s", _GADGET_SCRIPT)
            return
        log.info("UAC2 function absent — running gadget setup script")
        try:
            result = await asyncio.get_running_loop().run_in_executor(
                None,
                lambda: subprocess.run(
                    ["sudo", str(_GADGET_SCRIPT)],
                    capture_output=True, text=True, timeout=30,
                ),
            )
            if result.returncode != 0:
                log.warning(
                    "Gadget setup exited %d:\n%s", result.returncode, result.stderr
                )
        except subprocess.TimeoutExpired:
            log.warning("Gadget setup script timed out")

    async def _wait_for_alsa(self, timeout: float) -> str | None:
        loop = asyncio.get_running_loop()
        deadline = loop.time() + timeout
        while loop.time() < deadline:
            dev = find_uac2_playback_device()
            if dev:
                return dev
            await asyncio.sleep(0.25)
        return find_uac2_playback_device()
