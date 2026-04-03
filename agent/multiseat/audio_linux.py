# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Linux audio backend for multi-seat.

Creates per-seat virtual audio sinks via PipeWire/PulseAudio (pactl).
Each seat gets its own null sink that applications can output to.
The controller can route each sink independently via VBAN or PipeWire
link management.

Follows the AudioBackendLinux pattern from ozma_desktop_agent.py.
"""

from __future__ import annotations

import asyncio
import logging
import subprocess

from .audio_backend import SeatAudioBackend

log = logging.getLogger("ozma.agent.multiseat.audio_linux")


class LinuxAudioBackend(SeatAudioBackend):
    """
    Per-seat audio via PipeWire/PulseAudio null sinks.

    Each seat gets a `module-null-sink` loaded via `pactl`. This creates
    both a sink (for playback) and a monitor source (for capture/streaming).
    """

    def __init__(self) -> None:
        # seat_name -> pactl module ID
        self._modules: dict[str, int] = {}

    async def create_sink(self, seat_name: str) -> str | None:
        """
        Create a null sink for the given seat.

        The sink is named "ozma-seat-{name}" and shows up in PipeWire/Pulse
        as a selectable audio output.
        """
        sink_name = f"ozma-{seat_name}"

        # Check if already created
        if seat_name in self._modules:
            log.debug("Sink already exists for %s", seat_name)
            return sink_name

        try:
            result = await asyncio.get_running_loop().run_in_executor(
                None,
                lambda: subprocess.run(
                    [
                        "pactl", "load-module", "module-null-sink",
                        f"sink_name={sink_name}",
                        f"sink_properties=device.description=Ozma-{seat_name}",
                        "rate=48000",
                        "channels=2",
                    ],
                    capture_output=True, text=True, timeout=5,
                ),
            )
            if result.returncode == 0 and result.stdout.strip():
                module_id = int(result.stdout.strip())
                self._modules[seat_name] = module_id
                log.info("Audio sink created: %s (module %d)", sink_name, module_id)
                return sink_name
            log.warning("pactl load-module failed: %s", result.stderr.strip())
            return None
        except FileNotFoundError:
            log.warning("pactl not found — audio sinks unavailable (is PipeWire/PulseAudio running?)")
            return None
        except Exception as e:
            log.warning("Failed to create audio sink for %s: %s", seat_name, e)
            return None

    async def destroy_sink(self, seat_name: str) -> bool:
        """Unload the null sink module for the given seat."""
        module_id = self._modules.pop(seat_name, None)
        if module_id is None:
            return True  # already gone

        try:
            result = await asyncio.get_running_loop().run_in_executor(
                None,
                lambda mid=module_id: subprocess.run(
                    ["pactl", "unload-module", str(mid)],
                    capture_output=True, text=True, timeout=5,
                ),
            )
            if result.returncode == 0:
                log.info("Audio sink destroyed for %s (module %d)", seat_name, module_id)
                return True
            log.warning("pactl unload-module failed: %s", result.stderr.strip())
            return False
        except Exception as e:
            log.warning("Failed to destroy audio sink for %s: %s", seat_name, e)
            return False

    async def assign_output(self, seat_name: str, device: str) -> bool:
        """
        Route a seat's virtual sink to a physical output device.

        Creates a PipeWire/PulseAudio loopback from the seat's null sink
        monitor to the target physical sink.

        Args:
            seat_name: The seat whose audio to route
            device: Target PipeWire/PulseAudio sink name (e.g. "alsa_output.usb-...")
        """
        sink_name = f"ozma-{seat_name}"

        try:
            # Load a loopback module: null-sink monitor → physical sink
            loopback_props = f"source={sink_name}.monitor sink={device}"
            result = await asyncio.get_running_loop().run_in_executor(
                None,
                lambda: subprocess.run(
                    [
                        "pactl", "load-module", "module-loopback",
                        f"source={sink_name}.monitor",
                        f"sink={device}",
                        "latency_msec=20",
                    ],
                    capture_output=True, text=True, timeout=5,
                ),
            )
            if result.returncode == 0:
                log.info("Audio routed: %s → %s", sink_name, device)
                return True
            log.warning("Audio routing failed: %s", result.stderr.strip())
            return False
        except Exception as e:
            log.warning("Failed to route audio for %s: %s", seat_name, e)
            return False

    async def list_sinks(self) -> list[dict]:
        """List all managed seat audio sinks."""
        sinks = []
        for seat_name, module_id in self._modules.items():
            sinks.append({
                "seat": seat_name,
                "sink_name": f"ozma-{seat_name}",
                "module_id": module_id,
            })
        return sinks

    async def destroy_all(self) -> None:
        """Destroy all managed sinks. Called during shutdown."""
        for seat_name in list(self._modules.keys()):
            await self.destroy_sink(seat_name)
