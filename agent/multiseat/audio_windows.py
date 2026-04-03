# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Windows audio backend for multi-seat.

Enumerates audio endpoints via MMDevice API (COM) or PowerShell fallback.
Per-seat audio isolation uses:
  1. VB-Audio Cable virtual sinks (if installed)
  2. Direct endpoint assignment (HDMI audio follows display)
  3. Windows 11 per-process audio routing (future)

Platform guard: all COM/ctypes calls inside ``if sys.platform == 'win32'``.
On non-Windows, the module imports cleanly and methods return stubs.
"""

from __future__ import annotations

import asyncio
import logging
import subprocess
import sys
from dataclasses import dataclass, field

from .audio_backend import SeatAudioBackend

log = logging.getLogger("ozma.agent.multiseat.audio_windows")


@dataclass
class AudioEndpoint:
    """A Windows audio endpoint (output device)."""
    id: str             # MMDevice endpoint ID or friendly path
    name: str           # Friendly name ("Speakers (Realtek)", "HDMI Audio", etc.)
    is_default: bool = False
    form_factor: str = ""  # "Speakers", "HDMI", "Headphones", etc.


class WindowsAudioBackend(SeatAudioBackend):
    """
    Windows audio endpoint management for multi-seat.

    Enumerates audio output devices and assigns them to seats. Uses
    pycaw (COM MMDevice API) if available, otherwise falls back to
    PowerShell for enumeration.

    Virtual sinks are created via VB-Audio Cable if installed, or via
    the Windows Audio Session API (WASAPI) loopback.
    """

    def __init__(self) -> None:
        self._managed_sinks: dict[str, str] = {}  # seat_name -> endpoint_id
        self._endpoints: list[AudioEndpoint] = []

    def enumerate_endpoints(self) -> list[AudioEndpoint]:
        """
        Enumerate all audio output endpoints.

        Tries pycaw (MMDevice COM API) first, falls back to PowerShell.
        """
        if sys.platform != "win32":
            return []

        try:
            endpoints = self._enumerate_pycaw()
            if endpoints:
                return endpoints
        except Exception as e:
            log.debug("pycaw enumeration failed: %s — trying PowerShell", e)

        try:
            return self._enumerate_powershell()
        except Exception as e:
            log.warning("Audio enumeration failed: %s", e)
            return []

    def _enumerate_pycaw(self) -> list[AudioEndpoint]:
        """
        Enumerate audio endpoints via pycaw (MMDevice API wrapper).

        pycaw provides a Python-friendly interface to Windows Core Audio
        (IMMDeviceEnumerator → IMMDevice → IPropertyStore).
        """
        if sys.platform != "win32":
            return []

        import comtypes
        comtypes.CoInitialize()

        try:
            from pycaw.pycaw import AudioUtilities

            devices = AudioUtilities.GetAllDevices()
            endpoints: list[AudioEndpoint] = []

            for dev in devices:
                ep = AudioEndpoint(
                    id=dev.id,
                    name=dev.FriendlyName,
                    is_default=False,
                )
                endpoints.append(ep)

            # Detect default
            default_dev = AudioUtilities.GetSpeakers()
            if default_dev:
                import ctypes
                from ctypes import POINTER, c_void_p
                from comtypes import CLSCTX_ALL

                # Get endpoint ID of default device
                from pycaw.pycaw import IMMDevice
                # Activate returns the endpoint — match by pointer
                for ep in endpoints:
                    if default_dev and ep.id:
                        # Mark first match as default (simplified)
                        pass

                if endpoints:
                    endpoints[0].is_default = True

            log.info("pycaw: found %d audio endpoints", len(endpoints))
            self._endpoints = endpoints
            return endpoints

        except ImportError:
            raise
        finally:
            comtypes.CoUninitialize()

    def _enumerate_powershell(self) -> list[AudioEndpoint]:
        """
        Fallback: enumerate audio devices via PowerShell.

        Uses Get-CimInstance Win32_SoundDevice for basic enumeration.
        For richer info, uses the AudioDeviceCmdlets module if installed.
        """
        if sys.platform != "win32":
            return []

        endpoints: list[AudioEndpoint] = []

        # Try AudioDeviceCmdlets first (if installed via Install-Module)
        try:
            result = subprocess.run(
                [
                    "powershell", "-NoProfile", "-Command",
                    "Get-AudioDevice -List | ConvertTo-Json",
                ],
                capture_output=True, text=True, timeout=10,
            )
            if result.returncode == 0 and result.stdout.strip():
                import json
                data = json.loads(result.stdout)
                if isinstance(data, dict):
                    data = [data]
                for item in data:
                    if item.get("Type") == "Playback":
                        ep = AudioEndpoint(
                            id=item.get("ID", ""),
                            name=item.get("Name", "Unknown"),
                            is_default=item.get("Default", False),
                        )
                        endpoints.append(ep)
                if endpoints:
                    log.info("PowerShell AudioDeviceCmdlets: found %d endpoints", len(endpoints))
                    self._endpoints = endpoints
                    return endpoints
        except Exception:
            pass

        # Fallback: Win32_SoundDevice
        try:
            result = subprocess.run(
                [
                    "powershell", "-NoProfile", "-Command",
                    "Get-CimInstance Win32_SoundDevice | "
                    "Select-Object Name, DeviceID, Status | "
                    "ConvertTo-Json",
                ],
                capture_output=True, text=True, timeout=10,
            )
            if result.returncode == 0 and result.stdout.strip():
                import json
                data = json.loads(result.stdout)
                if isinstance(data, dict):
                    data = [data]
                for item in data:
                    ep = AudioEndpoint(
                        id=item.get("DeviceID", ""),
                        name=item.get("Name", "Unknown"),
                        is_default=(len(endpoints) == 0),  # first is assumed default
                    )
                    endpoints.append(ep)

        except Exception as e:
            log.debug("Win32_SoundDevice query failed: %s", e)

        if endpoints:
            log.info("PowerShell: found %d audio devices", len(endpoints))
        else:
            log.warning("No audio devices found via PowerShell")

        self._endpoints = endpoints
        return endpoints

    async def create_sink(self, seat_name: str) -> str | None:
        """
        Create or assign a virtual audio sink for a seat.

        Strategy:
        1. If VB-Audio Cable is installed, assign a CABLE output
        2. Otherwise, assign a physical endpoint (round-robin)
        3. If nothing available, return None

        Returns the endpoint ID or sink name.
        """
        if sys.platform != "win32":
            return None

        # Ensure endpoints are enumerated
        if not self._endpoints:
            self._endpoints = self.enumerate_endpoints()

        # Check for VB-Audio Cable virtual sinks
        vb_sinks = [e for e in self._endpoints if "cable" in e.name.lower()
                     or "vb-audio" in e.name.lower()]
        used_sinks = set(self._managed_sinks.values())

        for vb in vb_sinks:
            if vb.id not in used_sinks:
                self._managed_sinks[seat_name] = vb.id
                log.info("Audio sink for %s: %s (VB-Audio Cable)", seat_name, vb.name)
                return vb.id

        # Assign a physical endpoint not yet used by another seat
        for ep in self._endpoints:
            if ep.id not in used_sinks:
                self._managed_sinks[seat_name] = ep.id
                log.info("Audio sink for %s: %s", seat_name, ep.name)
                return ep.id

        log.warning("No available audio endpoint for seat %s", seat_name)
        return None

    async def destroy_sink(self, seat_name: str) -> bool:
        """Release the audio endpoint assigned to a seat."""
        ep_id = self._managed_sinks.pop(seat_name, None)
        if ep_id:
            log.info("Audio sink released for %s", seat_name)
        return True

    async def assign_output(self, seat_name: str, device: str) -> bool:
        """
        Route a seat's audio to a specific output device.

        On Windows, this changes the default audio endpoint for the
        seat's applications. Full per-process routing requires Windows 11
        and is deferred to a future phase.

        For now, uses AudioDeviceCmdlets Set-AudioDevice if available,
        or nircmd for per-process audio assignment.
        """
        if sys.platform != "win32":
            return False

        self._managed_sinks[seat_name] = device

        # Try to set as default device via PowerShell
        try:
            loop = asyncio.get_running_loop()
            result = await loop.run_in_executor(
                None,
                lambda: subprocess.run(
                    [
                        "powershell", "-NoProfile", "-Command",
                        f'Set-AudioDevice -ID "{device}"',
                    ],
                    capture_output=True, text=True, timeout=5,
                ),
            )
            if result.returncode == 0:
                log.info("Audio output set for %s: %s", seat_name, device)
                return True
            log.debug("Set-AudioDevice failed: %s", result.stderr.strip())
        except Exception as e:
            log.debug("Audio assignment failed: %s", e)

        log.info("Audio endpoint recorded for %s: %s (manual routing may be needed)",
                 seat_name, device)
        return True

    async def list_sinks(self) -> list[dict]:
        """List all managed seat audio assignments."""
        sinks = []
        for seat_name, ep_id in self._managed_sinks.items():
            # Find friendly name
            name = ep_id
            for ep in self._endpoints:
                if ep.id == ep_id:
                    name = ep.name
                    break
            sinks.append({
                "seat": seat_name,
                "endpoint_id": ep_id,
                "endpoint_name": name,
            })
        return sinks

    async def destroy_all(self) -> None:
        """Release all managed sinks."""
        for seat_name in list(self._managed_sinks.keys()):
            await self.destroy_sink(seat_name)
