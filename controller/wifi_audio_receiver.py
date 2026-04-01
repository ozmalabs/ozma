# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Wi-Fi audio receivers — AirPlay and Spotify Connect.

Makes the ozma controller appear as a wireless audio receiver on the
network.  Phones and laptops can send audio without any ozma-specific
app — they use built-in OS features.

AirPlay receiver (shairport-sync):
  - iPhone/iPad/Mac: tap "Ozma" in Control Center → AirPlay
  - Once selected, it persists across sessions
  - 44.1kHz ALAC (lossless) or AAC, ~200ms latency
  - Audio lands in PipeWire as a named source; AudioRouter can route it

Spotify Connect (librespot):
  - Any phone/tablet/desktop with Spotify: "Ozma" appears as a device
  - One tap in the Spotify app — no separate app needed
  - 44.1kHz Ogg Vorbis 320kbps, ~250ms latency
  - Works on both iOS and Android

Both run as managed subprocesses outputting to PipeWire.  The audio
appears as named PipeWire sources that can be routed through ozma's
audio output system (with delay, to multiple outputs, etc.).

Requirements:
  AirPlay: `apt install shairport-sync` or available in PATH
  Spotify: `librespot` binary in PATH (https://github.com/librespot-org/librespot)
"""

from __future__ import annotations

import asyncio
import logging
import shutil
from typing import Any

log = logging.getLogger("ozma.wifi_audio")


class AirPlayReceiver:
    """
    AirPlay (RAOP) receiver via shairport-sync.

    When running, iPhones/iPads/Macs on the network see "Ozma" (or
    configured name) as an AirPlay destination.  Audio is piped into
    PipeWire via shairport-sync's native PipeWire backend (or ALSA
    fallback → PipeWire captures it automatically).

    No app needed on the phone.  Select once in Control Center, it
    remembers the selection.
    """

    def __init__(
        self,
        name: str = "Ozma",
        port: int = 5000,
        pw_sink: str = "",    # Empty = PipeWire default
    ) -> None:
        self._name = name
        self._port = port
        self._pw_sink = pw_sink
        self._proc: asyncio.subprocess.Process | None = None
        self._available = False

    @property
    def available(self) -> bool:
        return self._available

    @property
    def running(self) -> bool:
        return self._proc is not None and self._proc.returncode is None

    async def start(self) -> bool:
        """Start shairport-sync as a managed subprocess."""
        if not shutil.which("shairport-sync"):
            log.info("shairport-sync not installed — AirPlay receiver disabled")
            return False

        # Build command — shairport-sync with PipeWire or ALSA backend
        cmd = [
            "shairport-sync",
            "--name", self._name,
            "--port", str(self._port),
            "--use-stderr",         # Log to stderr (we capture it)
        ]

        # Prefer PipeWire backend if available
        # shairport-sync 4.x supports --output=pw (PipeWire native)
        # Older versions use ALSA, which PipeWire captures automatically
        version = await self._get_version()
        if version and version >= (4, 0):
            cmd.extend(["--output", "pw"])
            if self._pw_sink:
                cmd.extend(["--pw-backend-options", f"node.name={self._pw_sink}"])

        try:
            self._proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.PIPE,
            )
            self._available = True
            log.info("AirPlay receiver started: '%s' on port %d (pid %d)",
                     self._name, self._port, self._proc.pid)

            # Monitor in background
            asyncio.create_task(self._monitor(), name="airplay-monitor")
            return True
        except Exception as e:
            log.warning("Failed to start AirPlay receiver: %s", e)
            return False

    async def stop(self) -> None:
        if self._proc and self._proc.returncode is None:
            self._proc.terminate()
            try:
                await asyncio.wait_for(self._proc.wait(), timeout=5.0)
            except asyncio.TimeoutError:
                self._proc.kill()
        self._available = False

    async def _monitor(self) -> None:
        """Watch shairport-sync stderr for connection events."""
        if not self._proc or not self._proc.stderr:
            return
        try:
            while True:
                line = await self._proc.stderr.readline()
                if not line:
                    break
                msg = line.decode(errors="replace").strip()
                if msg:
                    log.debug("AirPlay: %s", msg)
        except Exception:
            pass
        log.warning("AirPlay receiver exited (rc=%s)", self._proc.returncode)

    async def _get_version(self) -> tuple[int, ...] | None:
        try:
            proc = await asyncio.create_subprocess_exec(
                "shairport-sync", "--version",
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            out, _ = await asyncio.wait_for(proc.communicate(), timeout=3.0)
            # Parse "3.3.9" or "4.1.0" etc.
            import re
            m = re.search(r"(\d+)\.(\d+)\.?(\d*)", out.decode())
            if m:
                return tuple(int(x) for x in m.groups() if x)
        except Exception:
            pass
        return None

    def state_dict(self) -> dict[str, Any]:
        return {
            "type": "airplay",
            "name": self._name,
            "available": self._available,
            "running": self.running,
            "port": self._port,
        }


class SpotifyReceiver:
    """
    Spotify Connect receiver via librespot.

    When running, any Spotify app (phone, tablet, desktop) on the
    network sees "Ozma" as an available playback device.  One tap
    in Spotify to start playing through ozma's desk speakers.

    Audio is 44.1kHz Ogg Vorbis 320kbps, piped through PipeWire.
    No Spotify premium required for basic playback (premium for
    high quality and full library).

    Requires librespot binary: https://github.com/librespot-org/librespot
    """

    def __init__(
        self,
        name: str = "Ozma",
        backend: str = "pulseaudio",  # "pulseaudio" works with PipeWire's PA compat
        bitrate: int = 320,           # 96, 160, or 320
    ) -> None:
        self._name = name
        self._backend = backend
        self._bitrate = bitrate
        self._proc: asyncio.subprocess.Process | None = None
        self._available = False

    @property
    def available(self) -> bool:
        return self._available

    @property
    def running(self) -> bool:
        return self._proc is not None and self._proc.returncode is None

    async def start(self) -> bool:
        """Start librespot as a managed subprocess."""
        if not shutil.which("librespot"):
            log.info("librespot not installed — Spotify Connect disabled")
            return False

        cmd = [
            "librespot",
            "--name", self._name,
            "--backend", self._backend,
            "--bitrate", str(self._bitrate),
            "--enable-volume-normalisation",
            "--initial-volume", "80",
            "--device-type", "avr",     # Shows as audio receiver in Spotify
        ]

        try:
            self._proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.PIPE,
            )
            self._available = True
            log.info("Spotify Connect started: '%s' (%dkbps, pid %d)",
                     self._name, self._bitrate, self._proc.pid)
            asyncio.create_task(self._monitor(), name="spotify-monitor")
            return True
        except Exception as e:
            log.warning("Failed to start Spotify Connect: %s", e)
            return False

    async def stop(self) -> None:
        if self._proc and self._proc.returncode is None:
            self._proc.terminate()
            try:
                await asyncio.wait_for(self._proc.wait(), timeout=5.0)
            except asyncio.TimeoutError:
                self._proc.kill()
        self._available = False

    async def _monitor(self) -> None:
        if not self._proc or not self._proc.stderr:
            return
        try:
            while True:
                line = await self._proc.stderr.readline()
                if not line:
                    break
                msg = line.decode(errors="replace").strip()
                if msg:
                    log.debug("Spotify: %s", msg)
        except Exception:
            pass
        log.warning("Spotify Connect exited (rc=%s)", self._proc.returncode)

    def state_dict(self) -> dict[str, Any]:
        return {
            "type": "spotify",
            "name": self._name,
            "available": self._available,
            "running": self.running,
            "bitrate": self._bitrate,
        }


class WiFiAudioManager:
    """
    Manages wireless audio receivers (AirPlay + Spotify Connect).
    """

    def __init__(self, device_name: str = "Ozma") -> None:
        self.airplay = AirPlayReceiver(name=device_name)
        self.spotify = SpotifyReceiver(name=device_name)

    async def start(self) -> None:
        await self.airplay.start()
        await self.spotify.start()

    async def stop(self) -> None:
        await self.airplay.stop()
        await self.spotify.stop()

    def state_dict(self) -> dict[str, Any]:
        return {
            "airplay": self.airplay.state_dict(),
            "spotify": self.spotify.state_dict(),
        }
